//! SightLoom session: enroll photo, ingest frames, search, save package.

use crate::decode::RgbFrame;
use crate::error::{HostError, Result};
use crate::models::{missing_weights_help, require_weights};
use serde::Serialize;
use sightloom::core::{FrameStamp, MediaTime, SourceId, SubjectId, TrackKey};
use sightloom::reid::{MatchDecision, SubjectModality};
use sightloom::{FrameView, PixelFormat};
use sightloom_host::HostPipeline;
use sightloom_index::SourceEntry;
use std::path::Path;

/// Ranked photo search hit (JSON-safe).
#[derive(Debug, Clone, Serialize)]
pub struct PhotoHit {
    /// VisionIndex subject id.
    pub subject_id: u64,
    /// Fused score.
    pub score: f32,
    /// `accept` / `reject` / `uncertain`.
    pub decision: String,
}

/// Open an ONNX `HostPipeline` or fail with exit-2 semantics.
///
/// # Errors
///
/// Missing weights or model load.
pub fn open_pipeline(name: &str, models_dir: &Path) -> Result<HostPipeline> {
    let _paths = require_weights(models_dir).map_err(|e| {
        if let HostError::MissingWeights(msg) = e {
            HostError::MissingWeights(format!("{msg}\n{}", missing_weights_help(models_dir)))
        } else {
            e
        }
    })?;
    HostPipeline::from_onnx_cache(name, models_dir).map_err(|e| {
        HostError::MissingWeights(format!("{e}\n{}", missing_weights_help(models_dir)))
    })
}

/// Register the video as source 1.
pub fn add_video_source(pipe: &mut HostPipeline, video: &Path) {
    pipe.session_mut().add_source(SourceEntry {
        source_id: 1,
        uri: format!("file://{}", video.display()),
        hash: None,
    });
}

/// Detect + track + embed every RGB frame.
///
/// # Errors
///
/// Detector / tracker / embed.
pub fn ingest_frames(pipe: &mut HostPipeline, frames: &[RgbFrame]) -> Result<usize> {
    ingest_frames_strided(pipe, frames, 1)
}

/// Detect+track every frame; embed every `embed_every`th (P1 skip).
///
/// # Errors
///
/// Detector / tracker / embed.
pub fn ingest_frames_strided(
    pipe: &mut HostPipeline,
    frames: &[RgbFrame],
    embed_every: u32,
) -> Result<usize> {
    let stride = embed_every.max(1);
    let mut tracks = 0_usize;
    for frame in frames {
        let view = FrameView::new(
            frame.width,
            frame.height,
            frame.width as usize * 3,
            PixelFormat::Rgb8,
            &frame.rgb,
        );
        let pts = MediaTime::new(frame.ticks, frame.timescale)
            .map_err(|e| HostError::SightLoom(format!("pts: {e:?}")))?;
        let stamp = FrameStamp::new(SourceId(1), frame.index, pts, None);
        let tracked = if frame.index.is_multiple_of(u64::from(stride)) {
            pipe.ingest_frame(stamp, &view)
                .map_err(|e| HostError::SightLoom(e.to_string()))?
        } else {
            pipe.ingest_frame_track_only(stamp, &view)
                .map_err(|e| HostError::SightLoom(e.to_string()))?
        };
        tracks = tracks.max(tracked.len());
    }
    Ok(tracks)
}

/// Enroll a JPEG/PNG as a gallery subject.
///
/// # Errors
///
/// Decode / embed.
pub fn enroll_photo(pipe: &mut HostPipeline, jpeg: &[u8]) -> Result<u64> {
    let sid = pipe
        .enroll_photo(jpeg)
        .map_err(|e| HostError::SightLoom(e.to_string()))?;
    Ok(sid.0)
}

/// Search the gallery with a JPEG/PNG.
///
/// # Errors
///
/// Decode / embed / search.
pub fn search_photo(pipe: &mut HostPipeline, jpeg: &[u8], top_k: usize) -> Result<Vec<PhotoHit>> {
    let hits = pipe
        .search_photo_jpeg(jpeg, top_k.max(1))
        .map_err(|e| HostError::SightLoom(e.to_string()))?;
    Ok(hits
        .into_iter()
        .map(|h| PhotoHit {
            subject_id: h.subject_id.0,
            score: h.score,
            decision: match h.decision {
                MatchDecision::Accept => "accept".into(),
                MatchDecision::Reject => "reject".into(),
                MatchDecision::Uncertain => "uncertain".into(),
            },
        })
        .collect())
}

/// First Accept hit, or a hard error (never guess).
///
/// # Errors
///
/// No Accept in the ranking.
pub fn require_accept(hits: &[PhotoHit]) -> Result<u64> {
    if let Some(hit) = hits.iter().find(|h| h.decision == "accept") {
        return Ok(hit.subject_id);
    }
    let summary = if hits.is_empty() {
        "no gallery hits".into()
    } else {
        hits.iter()
            .map(|h| {
                format!(
                    "subject={} score={:.3} {}",
                    h.subject_id, h.score, h.decision
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    Err(HostError::PhotoNotAccepted(summary))
}

/// Resolve track embeddings against the gallery, give leftover tracks new
/// subjects, stamp every sample, rebuild appearances/profiles.
///
/// Without this, enroll lives only in `gallery.json` and Intelligence sees
/// `subjects: []`.
///
/// # Errors
///
/// Identity resolve failures.
pub fn finalize_identities(
    pipe: &mut HostPipeline,
    allowed_subject: u64,
    at: MediaTime,
) -> Result<(usize, usize)> {
    let session = pipe.session_mut();
    let resolved = session
        .resolve_pending_identities(at, Some(SubjectModality::PersonAppearance))
        .map_err(|e| HostError::SightLoom(format!("resolve identities: {e}")))?;

    let keys: Vec<TrackKey> = {
        let mut keys = Vec::new();
        for sample in session.index().tracks.effective_samples() {
            let key = sample.track_key();
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        keys
    };
    for key in keys {
        if session.subject_for_track_key(key).is_none() {
            let sid = session.register_subject(SubjectModality::PersonAppearance);
            session.assign_subject(key, sid);
        }
    }

    let assignments: Vec<(TrackKey, SubjectId)> = {
        let mut out = Vec::new();
        for sample in session.index().tracks.effective_samples() {
            let key = sample.track_key();
            if let Some(sid) = session.subject_for_track_key(key) {
                out.push((key, sid));
            }
        }
        out
    };
    {
        let index = session.index_mut();
        let samples = index.tracks.effective_samples();
        for sample in samples {
            let Some((_, sid)) = assignments.iter().find(|(k, _)| *k == sample.track_key()) else {
                continue;
            };
            if sample.subject_id == Some(*sid) {
                continue;
            }
            let mut updated = sample;
            updated.subject_id = Some(*sid);
            if sample.sample_id != 0 {
                index.tracks.push_revision(updated, sample.sample_id);
            }
        }
    }

    let (appearances, _visits, subjects) = session.rebuild_memory_from_tracks();
    session.set_subject_label(SubjectId(allowed_subject), "photo");
    let _ = resolved;
    Ok((appearances, subjects))
}

/// Write VisionIndex package.
///
/// # Errors
///
/// Package I/O.
pub fn save_package(pipe: &HostPipeline, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    pipe.save_package(dir)
        .map_err(|e| HostError::SightLoom(e.to_string()))
}
