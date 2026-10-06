//! SightLoom session: enroll photo, ingest frames, search, save package.

use crate::decode::{RgbFrame, SampledPicture};
use crate::error::{HostError, Result};
use crate::models::{missing_weights_help, require_weights};
use serde::Serialize;
use sightloom::core::{FrameStamp, MediaTime, SourceId, SubjectId, TrackId, TrackKey};
use sightloom::reid::{MatchDecision, SubjectModality};
use sightloom::{FrameView, PixelFormat};
use sightloom_host::HostPipeline;
use sightloom_index::SourceEntry;
use std::collections::HashMap;
use std::path::Path;

/// Ranked photo / track search hit (JSON-safe).
#[derive(Debug, Clone, Default, Serialize)]
pub struct PhotoHit {
    /// VisionIndex subject id.
    pub subject_id: u64,
    /// Fused score.
    pub score: f32,
    /// `accept` / `reject` / `uncertain`.
    pub decision: String,
    /// Track source (0 when the hit is gallery-only).
    pub source_id: u32,
    /// Local track id (0 when the hit is gallery-only).
    pub track_id: u32,
    /// Embedding handle for identity clustering (not serialized).
    #[serde(skip)]
    pub embedding: u64,
}

/// Keep-person after merging fragmented tracks onto one subject.
#[derive(Debug, Clone)]
pub struct KeepIdentity {
    /// Canonical allowed subject.
    pub subject_id: u64,
    /// Subject ids invert must drop (canonical + pre-merge fragments).
    pub allowed_ids: Vec<u64>,
    /// Tracks assigned to the keep-person.
    pub keep_tracks: usize,
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
        let stamp = FrameStamp::new(SourceId(1), frame.source_index, pts, None);
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
            ..PhotoHit::default()
        })
        .collect())
}

/// Cosine search of a still against **video tracks** (not the enrolled JPEG).
///
/// # Errors
///
/// Embed / search.
pub fn search_video_tracks(
    pipe: &mut HostPipeline,
    jpeg: &[u8],
    top_k: usize,
) -> Result<Vec<PhotoHit>> {
    let hits = pipe
        .search_tracks_jpeg(jpeg, top_k.max(1))
        .map_err(|e| HostError::SightLoom(e.to_string()))?;
    Ok(hits
        .into_iter()
        .map(|h| PhotoHit {
            subject_id: h.subject_id.map_or(0, |s| s.0),
            score: h.score,
            decision: if h.score >= TRACK_ACCEPT {
                "accept".into()
            } else if h.score >= TRACK_UNCERTAIN {
                "uncertain".into()
            } else {
                "reject".into()
            },
            source_id: h.track_key.source_id.0,
            track_id: h.track_key.local_track_id.0,
            embedding: h.embedding.0,
        })
        .collect())
}

/// First Accept on a **video** subject. Never Accept the enrolled still itself.
///
/// # Errors
///
/// No Accept, or the only Accepts have no track samples.
pub fn require_video_accept(hits: &[PhotoHit]) -> Result<u64> {
    Ok(best_video_accept(hits)?.subject_id)
}

/// Highest-scoring video Accept (fragmented tracks all look like Accept).
///
/// # Errors
///
/// No Accept with a track or subject id.
pub fn best_video_accept(hits: &[PhotoHit]) -> Result<PhotoHit> {
    hits.iter()
        .filter(|h| h.decision == "accept" && (h.subject_id != 0 || h.track_id != 0))
        .max_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .cloned()
        .ok_or_else(|| {
            HostError::PhotoNotAccepted(if hits.is_empty() {
                "no gallery hits".into()
            } else {
                hits.iter()
                    .map(|h| {
                        format!(
                            "subject={} track={}:{} score={:.3} {}",
                            h.subject_id, h.source_id, h.track_id, h.score, h.decision
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            })
        })
}

/// Keep the photo person. Re-id cosine alone cannot merge cuts: every person
/// scores ~0.5–0.75, so a global threshold either redacts the keep-person or
/// lets a second face through. Per frame we keep **at most one** box — Re-id
/// plus clothing color against the still — and leave everyone else for invert.
///
/// Clothing color loads one sampled PNG for that frame index and drops it.
/// A missing picture scores `0.0`.
///
/// # Errors
///
/// No video Accept, or a sampled PNG that cannot be decoded.
#[allow(clippy::too_many_lines)]
pub fn merge_keep_identity(
    pipe: &mut HostPipeline,
    photo_hits: &[PhotoHit],
    photo_rgb: &[u8],
    photo_w: u32,
    photo_h: u32,
    pictures: &[SampledPicture],
) -> Result<KeepIdentity> {
    let seed = best_video_accept(photo_hits)?;
    let seed_key = TrackKey::new(SourceId(seed.source_id), TrackId(seed.track_id));
    let mut reid: HashMap<(u32, u32), f32> = HashMap::new();
    for h in photo_hits {
        if h.track_id == 0 {
            continue;
        }
        reid.insert((h.source_id, h.track_id), h.score);
    }
    let picture_by_index: HashMap<u64, &SampledPicture> = pictures
        .iter()
        .map(|picture| (picture.index, picture))
        .collect();
    let photo_fallback = clothing_mean(photo_rgb, photo_w, photo_h);

    let mut by_frame: HashMap<u64, Vec<sightloom_index::TrackSample>> = HashMap::new();
    {
        let session = pipe.session();
        for sample in session.index().tracks.effective_samples() {
            by_frame.entry(sample.frame_index).or_default().push(sample);
        }
    }

    let mut keep_keys: Vec<TrackKey> = Vec::new();
    let mut allowed_ids: Vec<u64> = Vec::new();
    if seed.subject_id != 0 {
        allowed_ids.push(seed.subject_id);
    }
    keep_keys.push(seed_key);
    let seed_color =
        track_clothing_mean(&by_frame, seed_key, &picture_by_index)?.unwrap_or(photo_fallback);

    let mut color_sum: HashMap<TrackKey, (f32, u32, Option<SubjectId>)> = HashMap::new();
    for (frame_index, row) in &by_frame {
        let Some(frame) = load_indexed_picture(&picture_by_index, *frame_index)? else {
            continue;
        };
        for sample in row {
            let c = rgb_affinity(seed_color, clothing_box_mean(&frame, *sample));
            let entry = color_sum
                .entry(sample.track_key())
                .or_insert((0.0, 0, sample.subject_id));
            entry.0 += c;
            entry.1 += 1;
            if entry.2.is_none() {
                entry.2 = sample.subject_id;
            }
        }
    }
    for (key, (sum, n, sid)) in &color_sum {
        let color = *sum / (*n as f32).max(1.0);
        let r = reid
            .get(&(key.source_id.0, key.local_track_id.0))
            .copied()
            .unwrap_or(0.0);
        // Face-only fragments of the keep-person have weaker clothing color
        // (~0.62) but still beat the still on Re-id. Do not skip the color
        // floor: Re-id 0.70 with color 0.36 is the other person on a cut.
        let face_fragment = r >= 0.65 && color >= 0.55;
        if (r >= KEEP_REID_MIN && color >= 0.80) || face_fragment {
            eprintln!(
                "global keep track={}:{} reid={r:.3} color={color:.3}",
                key.source_id.0, key.local_track_id.0
            );
            push_keep(&mut keep_keys, &mut allowed_ids, *key, *sid);
        }
    }

    let mut frames_kept = 0_usize;
    let mut frames_none = 0_usize;
    for (frame_index, row) in &by_frame {
        if let Some(seed_sample) = row.iter().find(|s| s.track_key() == seed_key) {
            push_keep(
                &mut keep_keys,
                &mut allowed_ids,
                seed_key,
                seed_sample.subject_id,
            );
            for sample in row {
                if sample.track_key() != seed_key && covers_same_person(*seed_sample, *sample) {
                    push_keep(
                        &mut keep_keys,
                        &mut allowed_ids,
                        sample.track_key(),
                        sample.subject_id,
                    );
                }
            }
            frames_kept += 1;
            continue;
        }
        if row.len() == 1 {
            let sample = row[0];
            let key = sample.track_key();
            let r = reid
                .get(&(key.source_id.0, key.local_track_id.0))
                .copied()
                .unwrap_or(0.0);
            let loaded = load_indexed_picture(&picture_by_index, *frame_index)?;
            let color = loaded.as_ref().map_or(0.0, |frame| {
                rgb_affinity(seed_color, clothing_box_mean(frame, sample))
            });
            let combined = r + COLOR_WEIGHT * color;
            if combined < COMBINED_MIN || color < COLOR_MIN || r < KEEP_REID_MIN {
                frames_none += 1;
                continue;
            }
            push_keep(&mut keep_keys, &mut allowed_ids, key, sample.subject_id);
            frames_kept += 1;
            continue;
        }
        let loaded = load_indexed_picture(&picture_by_index, *frame_index)?;
        let mut ranked: Vec<(f32, f32, f32, sightloom_index::TrackSample)> = Vec::new();
        for sample in row {
            let key = sample.track_key();
            let r = reid
                .get(&(key.source_id.0, key.local_track_id.0))
                .copied()
                .unwrap_or(0.0);
            let color = loaded.as_ref().map_or(0.0, |frame| {
                rgb_affinity(seed_color, clothing_box_mean(frame, *sample))
            });
            ranked.push((r + COLOR_WEIGHT * color, r, color, *sample));
        }
        ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let best = ranked[0];
        let second = ranked.get(1).map_or(0.0, |s| s.0);
        let slam_dunk = best.2 >= 0.85 && best.0 >= COMBINED_MIN;
        if best.0 < COMBINED_MIN
            || best.2 < COLOR_MIN
            || best.1 < KEEP_REID_MIN
            || (!slam_dunk && best.0 - second < COMBINED_GAP)
        {
            frames_none += 1;
            eprintln!(
                "frame {frame_index} keep=none combined={:.3}/{:.3} reid={:.3} color={:.3}",
                best.0, second, best.1, best.2
            );
            continue;
        }
        eprintln!(
            "frame {frame_index} keep track={}:{} combined={:.3} reid={:.3} color={:.3}",
            best.3.track_key().source_id.0,
            best.3.track_key().local_track_id.0,
            best.0,
            best.1,
            best.2
        );
        push_keep(
            &mut keep_keys,
            &mut allowed_ids,
            best.3.track_key(),
            best.3.subject_id,
        );
        for sample in row {
            if sample.track_key() == best.3.track_key() {
                continue;
            }
            if covers_same_person(best.3, *sample) {
                push_keep(
                    &mut keep_keys,
                    &mut allowed_ids,
                    sample.track_key(),
                    sample.subject_id,
                );
            }
        }
        frames_kept += 1;
    }

    let session = pipe.session_mut();
    let canonical = if seed.subject_id != 0 {
        SubjectId(seed.subject_id)
    } else {
        session.register_subject(SubjectModality::PersonAppearance)
    };
    if !allowed_ids.contains(&canonical.0) {
        allowed_ids.insert(0, canonical.0);
    }
    for key in &keep_keys {
        session.assign_subject(*key, canonical);
    }
    stamp_track_subjects(session);
    session.set_subject_label(canonical, "photo");
    let _ = session.rebuild_memory_from_tracks();
    eprintln!(
        "keep identity subject={} tracks={} frames_kept={frames_kept} frames_none={frames_none} allowed={allowed_ids:?}",
        canonical.0,
        keep_keys.len()
    );
    Ok(KeepIdentity {
        subject_id: canonical.0,
        allowed_ids,
        keep_tracks: keep_keys.len(),
    })
}

fn covers_same_person(
    keep: sightloom_index::TrackSample,
    other: sightloom_index::TrackSample,
) -> bool {
    let cx = (other.left + other.right) * 0.5;
    let cy = (other.top + other.bottom) * 0.5;
    if cx >= keep.left && cx <= keep.right && cy >= keep.top && cy <= keep.bottom {
        return true;
    }
    let inter = intersection_area(keep, other);
    if inter <= 0.0 {
        return false;
    }
    let keep_area = box_area(keep).max(1.0);
    let other_area = box_area(other).max(1.0);
    let iou = inter / (keep_area + other_area - inter);
    iou >= 0.25 || inter / other_area.min(keep_area) >= 0.55
}

fn box_area(sample: sightloom_index::TrackSample) -> f32 {
    (sample.right - sample.left).abs() * (sample.bottom - sample.top).abs()
}

fn intersection_area(a: sightloom_index::TrackSample, b: sightloom_index::TrackSample) -> f32 {
    let left = a.left.max(b.left);
    let top = a.top.max(b.top);
    let right = a.right.min(b.right);
    let bottom = a.bottom.min(b.bottom);
    let w = (right - left).max(0.0);
    let h = (bottom - top).max(0.0);
    w * h
}

fn push_keep(
    keep_keys: &mut Vec<TrackKey>,
    allowed_ids: &mut Vec<u64>,
    key: TrackKey,
    subject_id: Option<SubjectId>,
) {
    if !keep_keys.contains(&key) {
        keep_keys.push(key);
    }
    if let Some(sid) = subject_id
        && sid.0 != 0
        && !allowed_ids.contains(&sid.0)
    {
        allowed_ids.push(sid.0);
    }
}

fn clothing_mean(rgb: &[u8], width: u32, height: u32) -> [f32; 3] {
    let top = height as f32 * 0.55;
    box_mean_rgb(rgb, width, height, 0.0, top, width as f32, height as f32)
}

fn clothing_box_mean(frame: &RgbFrame, sample: sightloom_index::TrackSample) -> [f32; 3] {
    let top = sample.top + (sample.bottom - sample.top).abs() * 0.40;
    box_mean(frame, sample.left, top, sample.right, sample.bottom)
}

fn load_indexed_picture(
    pictures: &HashMap<u64, &SampledPicture>,
    frame_index: u64,
) -> Result<Option<RgbFrame>> {
    let Some(picture) = pictures.get(&frame_index) else {
        return Ok(None);
    };
    picture.load().map(Some)
}

fn track_clothing_mean(
    by_frame: &HashMap<u64, Vec<sightloom_index::TrackSample>>,
    seed: TrackKey,
    pictures: &HashMap<u64, &SampledPicture>,
) -> Result<Option<[f32; 3]>> {
    let mut sum = [0.0_f32; 3];
    let mut n = 0.0_f32;
    for (frame_index, row) in by_frame {
        let Some(frame) = load_indexed_picture(pictures, *frame_index)? else {
            continue;
        };
        for sample in row {
            if sample.track_key() != seed {
                continue;
            }
            let c = clothing_box_mean(&frame, *sample);
            sum[0] += c[0];
            sum[1] += c[1];
            sum[2] += c[2];
            n += 1.0;
        }
    }
    if n < 1.0 {
        return Ok(None);
    }
    Ok(Some([sum[0] / n, sum[1] / n, sum[2] / n]))
}

fn box_mean(frame: &RgbFrame, left: f32, top: f32, right: f32, bottom: f32) -> [f32; 3] {
    box_mean_rgb(
        &frame.rgb,
        frame.width,
        frame.height,
        left,
        top,
        right,
        bottom,
    )
}

#[allow(
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation
)]
fn box_mean_rgb(
    rgb: &[u8],
    width: u32,
    height: u32,
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
) -> [f32; 3] {
    if width == 0 || height == 0 {
        return [0.0, 0.0, 0.0];
    }
    let x0 = left.max(0.0).min(width.saturating_sub(1) as f32) as u32;
    let y0 = top.max(0.0).min(height.saturating_sub(1) as f32) as u32;
    let x1 = right.max(left + 1.0).min(width as f32) as u32;
    let y1 = bottom.max(top + 1.0).min(height as f32) as u32;
    let mut sum = [0.0_f32; 3];
    let mut n = 0.0_f32;
    let stride = width as usize * 3;
    for y in y0..y1 {
        let row = y as usize * stride;
        for x in x0..x1 {
            let i = row + x as usize * 3;
            if i + 2 >= rgb.len() {
                continue;
            }
            sum[0] += f32::from(rgb[i]);
            sum[1] += f32::from(rgb[i + 1]);
            sum[2] += f32::from(rgb[i + 2]);
            n += 1.0;
        }
    }
    if n < 1.0 {
        return [0.0, 0.0, 0.0];
    }
    [sum[0] / n, sum[1] / n, sum[2] / n]
}

#[must_use]
fn rgb_affinity(left: [f32; 3], right: [f32; 3]) -> f32 {
    let (h1, c1, v1) = hsv(left);
    let (h2, c2, v2) = hsv(right);
    if c1 > 0.18 && c2 < 0.12 || c2 > 0.18 && c1 < 0.12 {
        return 0.18;
    }
    let mut dh = (h1 - h2).abs();
    if dh > 180.0 {
        dh = 360.0 - dh;
    }
    let hue = (1.0 - (dh / 45.0).min(1.0)).max(0.0);
    let chroma = 1.0 - (c1 - c2).abs();
    let value = 1.0 - (v1 - v2).abs();
    (0.70 * hue + 0.20 * chroma + 0.10 * value).clamp(0.0, 1.0)
}

fn hsv(rgb: [f32; 3]) -> (f32, f32, f32) {
    let r = (rgb[0] / 255.0).clamp(0.0, 1.0);
    let g = (rgb[1] / 255.0).clamp(0.0, 1.0);
    let b = (rgb[2] / 255.0).clamp(0.0, 1.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let chroma = max - min;
    let hue = if chroma < 1.0e-6 {
        0.0
    } else if (max - r).abs() < 1.0e-6 {
        ((g - b) / chroma).rem_euclid(6.0) * 60.0
    } else if (max - g).abs() < 1.0e-6 {
        ((b - r) / chroma + 2.0) * 60.0
    } else {
        ((r - g) / chroma + 4.0) * 60.0
    };
    (hue, chroma, max)
}

fn stamp_track_subjects(session: &mut sightloom::IndexSession) {
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

/// Shrink **other** people to a head window. Never crop the keep-person —
/// that box is the shield so leftover detections cannot mosaic his hands
/// or sweater.
pub fn crop_track_boxes_to_faces(pipe: &mut HostPipeline, skip_subjects: &[u64]) {
    let session = pipe.session_mut();
    let updates: Vec<(u64, [f32; 4])> = session
        .index()
        .tracks
        .effective_samples()
        .into_iter()
        .filter(|s| s.subject_id.is_none_or(|id| !skip_subjects.contains(&id.0)))
        .map(|s| (s.sample_id, face_xyxy(s.left, s.top, s.right, s.bottom)))
        .collect();
    let index = session.index_mut();
    let samples = index.tracks.effective_samples();
    for sample in samples {
        let Some((_, box_xyxy)) = updates.iter().find(|(id, _)| *id == sample.sample_id) else {
            continue;
        };
        let [left, top, right, bottom] = *box_xyxy;
        if (sample.left - left).abs() < f32::EPSILON
            && (sample.top - top).abs() < f32::EPSILON
            && (sample.right - right).abs() < f32::EPSILON
            && (sample.bottom - bottom).abs() < f32::EPSILON
        {
            continue;
        }
        let mut revised = sample;
        revised.left = left;
        revised.top = top;
        revised.right = right;
        revised.bottom = bottom;
        if sample.sample_id != 0 {
            index.tracks.push_revision(revised, sample.sample_id);
        }
    }
}

/// Kill redaction boxes that paint on the keep-person.
///
/// Invert is subject-level. A leftover YOLO blob on his lap / hands / sweater
/// is a *different* subject, so it still mosaics him. Face-crop of that blob
/// lands on the sweater. Sample-level: if it touches the keep box, drop it.
pub fn protect_keep_geometry(pipe: &mut HostPipeline, keep_ids: &[u64]) {
    if keep_ids.is_empty() {
        return;
    }
    let session = pipe.session_mut();
    let mut by_frame: HashMap<u64, Vec<sightloom_index::TrackSample>> = HashMap::new();
    for sample in session.index().tracks.effective_samples() {
        by_frame.entry(sample.frame_index).or_default().push(sample);
    }
    let mut collapsed: Vec<u64> = Vec::new();
    for row in by_frame.values() {
        let keep_boxes: Vec<sightloom_index::TrackSample> = row
            .iter()
            .copied()
            .filter(|s| s.subject_id.is_some_and(|id| keep_ids.contains(&id.0)))
            .collect();
        if keep_boxes.is_empty() {
            continue;
        }
        for sample in row {
            if sample.subject_id.is_some_and(|id| keep_ids.contains(&id.0)) {
                continue;
            }
            if keep_boxes.iter().any(|k| overlaps_keep_shield(*k, *sample)) {
                collapsed.push(sample.sample_id);
            }
        }
    }
    if collapsed.is_empty() {
        return;
    }
    eprintln!(
        "protect keep: collapsed {} overlapping redaction boxes",
        collapsed.len()
    );
    let index = session.index_mut();
    let samples = index.tracks.effective_samples();
    for sample in samples {
        if !collapsed.contains(&sample.sample_id) || sample.sample_id == 0 {
            continue;
        }
        let mut updated = sample;
        updated.right = updated.left;
        updated.bottom = updated.top;
        index.tracks.push_revision(updated, sample.sample_id);
    }
}

fn overlaps_keep_shield(
    keep: sightloom_index::TrackSample,
    other: sightloom_index::TrackSample,
) -> bool {
    let pad_x = (keep.right - keep.left).abs() * 0.14;
    let pad_y = (keep.bottom - keep.top).abs() * 0.14;
    let left = keep.left - pad_x;
    let top = keep.top - pad_y;
    let right = keep.right + pad_x;
    let bottom = keep.bottom + pad_y * 1.35;
    let x0 = left.max(other.left);
    let y0 = top.max(other.top);
    let x1 = right.min(other.right);
    let y1 = bottom.min(other.bottom);
    (x1 - x0).max(0.0) * (y1 - y0).max(0.0) > 0.0
}

/// Head window inside a person box. Faces sit at the top; full-body blobs
/// leave faces open (the walking-clip failure).
#[must_use]
pub fn face_xyxy(left: f32, top: f32, right: f32, bottom: f32) -> [f32; 4] {
    let width = (right - left).abs().max(1.0);
    let height = (bottom - top).abs().max(1.0);
    let face_h = height * 0.30;
    let face_w = width * 0.72;
    let cx = (left + right) * 0.5;
    let face_top = top + height * 0.02;
    [
        cx - face_w * 0.5,
        face_top,
        cx + face_w * 0.5,
        face_top + face_h,
    ]
}

const TRACK_ACCEPT: f32 = 0.50;
const TRACK_UNCERTAIN: f32 = 0.35;
/// Clothing color vs the still. Re-id scores collapse across people (~0.7).
const COLOR_WEIGHT: f32 = 0.85;
const COMBINED_MIN: f32 = 1.05;
const COMBINED_GAP: f32 = 0.08;
/// Still must actually look like the photo's clothes, not just "a person".
const COLOR_MIN: f32 = 0.60;
/// Photo cosine floor for a keep. Clip-2 strangers score ~0.53 against the still.
const KEEP_REID_MIN: f32 = 0.60;

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::{PhotoHit, face_xyxy};

    #[test]
    fn face_window_is_top_of_person_box() {
        let [l, t, r, b] = face_xyxy(0.0, 0.0, 100.0, 200.0);
        assert!(t < 20.0, "face starts near the head, got top={t}");
        assert!(b < 90.0, "face must not cover the torso, got bottom={b}");
        assert!(r - l < 100.0);
        assert!(b - t < 80.0);
    }

    #[test]
    fn best_video_accept_picks_highest_score() {
        let hits = vec![
            PhotoHit {
                subject_id: 8,
                score: 0.71,
                decision: "accept".into(),
                track_id: 2,
                ..PhotoHit::default()
            },
            PhotoHit {
                subject_id: 4,
                score: 0.75,
                decision: "accept".into(),
                track_id: 1,
                ..PhotoHit::default()
            },
            PhotoHit {
                subject_id: 9,
                score: 0.90,
                decision: "reject".into(),
                track_id: 3,
                ..PhotoHit::default()
            },
        ];
        let seed = super::best_video_accept(&hits).unwrap();
        assert_eq!(seed.subject_id, 4);
        assert_eq!(seed.track_id, 1);
    }

    #[test]
    fn clothing_color_separates_rust_from_yellow() {
        let rust = [150.0, 72.0, 48.0];
        let yellow = [210.0, 175.0, 70.0];
        assert!(super::rgb_affinity(rust, rust) > 0.99);
        assert!(
            super::rgb_affinity(rust, yellow) < 0.55,
            "got {}",
            super::rgb_affinity(rust, yellow)
        );
        assert!(super::rgb_affinity(rust, yellow) < super::COLOR_MIN);
        let combined_keep = 0.55 + super::COLOR_WEIGHT * super::rgb_affinity(rust, rust);
        let combined_other = 0.70 + super::COLOR_WEIGHT * super::rgb_affinity(rust, yellow);
        assert!(
            combined_keep > combined_other,
            "back-of-head keep-person must beat a frontal other face"
        );
        assert!(combined_keep >= super::COMBINED_MIN);
    }

    fn sample_box(left: f32, top: f32, right: f32, bottom: f32) -> sightloom_index::TrackSample {
        sightloom_index::TrackSample {
            sample_id: 1,
            supersedes: None,
            revision: 1,
            idempotency_key: 0,
            source_id: sightloom::core::SourceId(1),
            frame_index: 0,
            pts: sightloom::core::MediaTime::new(0, 1).unwrap(),
            track_id: sightloom::core::TrackId(1),
            track_uid: None,
            subject_id: None,
            class_id: None,
            left,
            top,
            right,
            bottom,
            confidence: 1.0,
            mask_ref: 0,
        }
    }

    #[test]
    fn overlapping_face_box_is_same_person() {
        let body = sample_box(100.0, 80.0, 400.0, 900.0);
        let face = sample_box(160.0, 80.0, 340.0, 280.0);
        assert!(super::covers_same_person(body, face));
        let other = sample_box(500.0, 200.0, 700.0, 900.0);
        assert!(!super::covers_same_person(body, other));
        let nested = sample_box(180.0, 100.0, 260.0, 220.0);
        assert!(super::covers_same_person(body, nested));
    }

    #[test]
    fn keep_shield_covers_hands_and_lap_not_far_face() {
        let body = sample_box(80.0, 60.0, 480.0, 720.0);
        let lap = sample_box(120.0, 500.0, 360.0, 780.0);
        let forearm = sample_box(400.0, 380.0, 520.0, 460.0);
        let stranger = sample_box(560.0, 80.0, 700.0, 280.0);
        assert!(super::overlaps_keep_shield(body, lap));
        assert!(super::overlaps_keep_shield(body, forearm));
        assert!(!super::overlaps_keep_shield(body, stranger));
    }
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

    stamp_track_subjects(session);

    let (appearances, _visits, subjects) = session.rebuild_memory_from_tracks();
    if allowed_subject != 0 {
        session.set_subject_label(SubjectId(allowed_subject), "photo");
    }
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
