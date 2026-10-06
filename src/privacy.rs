//! Killer path: video + photo → blur everyone except the accepted subject.

use crate::compile::{photo_binding, photo_except_plan, resolve_bridge};
use crate::decode::{
    ExtractCancel, RGB_BATCH_FRAMES, applied_frame_cap, extract_sampled_pictures,
    extract_sampled_pictures_cancellable, fresh_frames_dir, materialize_video, probe_video,
    visit_rgb_batches, visit_rgb_batches_cancellable,
};
use crate::encode::run_graph;
use crate::error::Result;
use crate::vision::{
    add_video_source, crop_track_boxes_to_faces, finalize_identities, ingest_frames_strided,
    merge_keep_identity, open_pipeline, protect_keep_geometry, save_package, search_video_tracks,
};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Options for [`privacy_except`].
#[derive(Debug, Clone)]
pub struct PrivacyExceptOpts {
    /// Input video.
    pub video: PathBuf,
    /// Reference photo of the allowed person.
    pub photo: PathBuf,
    /// Output mp4.
    pub output: PathBuf,
    /// Scratch directory (frames, VisionIndex, graph, masks).
    pub work_dir: PathBuf,
    /// ONNX cache (default `.sightloom-models`).
    pub models_dir: PathBuf,
    /// Extracted frame rate for ingest (skip-frame vs source fps).
    pub sample_fps: u32,
    /// Cap extracted frames. `0` uses the analysis cap, not the whole file.
    pub max_frames: u32,
    /// Seconds to grab when `--video cam` / `lavfi:`.
    pub live_secs: f64,
    /// Embed every Nth sampled frame (`1` = every frame).
    pub embed_every: u32,
    /// Privacy fill. Host default is pixelate (gaussian is recoverable).
    pub redaction: reelforge_intelligence_core::RedactionKind,
    /// When set, extract and later RGB batches stop after the flag is raised.
    pub cancel: Option<ExtractCancel>,
}

/// Result of the killer path.
#[derive(Debug, Clone, Serialize)]
pub struct PrivacyExceptResult {
    /// Accepted subject id.
    pub subject_id: u64,
    /// Written output path.
    pub output: String,
    /// VisionIndex package directory.
    pub package: String,
    /// Graph JSON path.
    pub graph: String,
    /// Frames ingested.
    pub frames: usize,
    /// Peak tracks on any ingested frame.
    pub peak_tracks: usize,
    /// Detect+track+embed frames per second.
    pub ingest_fps: f64,
    /// Phase timings in milliseconds.
    pub phases_ms: PhaseTimings,
    /// Companion audio: `kept` or `none`. Dropped audio is an error, not a status.
    pub audio: AudioStatus,
}

/// Wall-clock phases for the killer path (P1).
#[derive(Debug, Clone, Serialize)]
pub struct PhaseTimings {
    /// ffmpeg extract.
    pub extract: u64,
    /// Photo enroll.
    pub enroll: u64,
    /// Detect + track + embed.
    pub ingest: u64,
    /// Photo search + Accept.
    pub search: u64,
    /// Identity finalize + package + Intelligence bridge.
    pub compile: u64,
    /// ReelForge encode.
    pub encode: u64,
    /// End-to-end.
    pub total: u64,
}

/// Companion audio status after encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioStatus {
    /// Source had audio and it is in the output.
    Kept,
    /// Source had no audio stream (live grab / silent file).
    None,
}

/// Full pipeline. Missing weights → [`crate::HostError::MissingWeights`] (exit 2).
///
/// # Errors
///
/// Weights, ffmpeg, photo not Accept, compile, encode.
#[allow(clippy::too_many_lines)]
pub fn privacy_except(opts: &PrivacyExceptOpts) -> Result<PrivacyExceptResult> {
    let _ = crate::models::require_weights(&opts.models_dir).map_err(|e| {
        if let crate::error::HostError::MissingWeights(msg) = e {
            crate::error::HostError::MissingWeights(format!(
                "{msg}\n{}",
                crate::models::missing_weights_help(&opts.models_dir)
            ))
        } else {
            e
        }
    })?;
    let t0 = Instant::now();
    std::fs::create_dir_all(&opts.work_dir)?;
    let video = materialize_video(&opts.video, &opts.work_dir, opts.live_secs.max(0.2))?;
    let info = probe_video(&video)?;
    let max_frames = applied_frame_cap(opts.max_frames);
    eprintln!(
        "extract frames @ {} fps from {} (max_frames={max_frames})",
        opts.sample_fps.max(1),
        video.display(),
    );
    let t_extract = Instant::now();
    let pictures = extract_sampled_pictures_cancellable(
        &video,
        &fresh_frames_dir(&opts.work_dir),
        opts.sample_fps.max(1),
        max_frames,
        opts.cancel.as_ref(),
    )?;
    let extract_ms = elapsed_ms(t_extract);
    eprintln!("extracted {} frames in {extract_ms} ms", pictures.len());

    let mut pipe = open_pipeline("privacy-except", &opts.models_dir)?;
    add_video_source(&mut pipe, &video);

    let photo_bytes = std::fs::read(&opts.photo)?;
    let decoded = sightloom_host::decode_encoded_rgb(&photo_bytes)
        .map_err(|e| crate::error::HostError::SightLoom(e.to_string()))?;
    let photo_box = [0.0, 0.0, decoded.width as f32, decoded.height as f32];

    let t_enroll = Instant::now();
    // Do not enroll the still first. Gallery search Accepts the JPEG itself
    // (score 1.0) and never binds the walking person.
    let enroll_ms = elapsed_ms(t_enroll);

    let t_ingest = Instant::now();
    let mut tracks = 0_usize;
    visit_rgb_batches_cancellable(&pictures, RGB_BATCH_FRAMES, opts.cancel.as_ref(), |batch| {
        if opts
            .cancel
            .as_ref()
            .is_some_and(ExtractCancel::is_cancelled)
        {
            return Err(crate::error::HostError::Ffmpeg("extract cancelled".into()));
        }
        tracks = tracks.max(ingest_frames_strided(&mut pipe, batch, opts.embed_every)?);
        Ok(())
    })?;
    let ingest_ms = elapsed_ms(t_ingest);
    let ingest_fps = if ingest_ms == 0 {
        0.0
    } else {
        (pictures.len() as f64) * 1000.0 / ingest_ms as f64
    };
    eprintln!(
        "ingested frames={} peak_tracks={tracks} in {ingest_ms} ms ({ingest_fps:.2} fps)",
        pictures.len()
    );

    let last_pts = pictures
        .last()
        .and_then(|f| sightloom::core::MediaTime::new(f.ticks, f.timescale).ok())
        .ok_or_else(|| crate::error::HostError::message("no frames for identity resolve"))?;
    let t_search = Instant::now();
    let (appearances, subjects) = finalize_identities(&mut pipe, 0, last_pts)?;
    let hits = search_video_tracks(&mut pipe, &photo_bytes, 64)?;
    for h in &hits {
        eprintln!(
            "track hit subject={} track={}:{} score={:.3} {}",
            h.subject_id, h.source_id, h.track_id, h.score, h.decision
        );
    }
    let keep = merge_keep_identity(
        &mut pipe,
        &hits,
        &decoded.rgb,
        decoded.width,
        decoded.height,
        &pictures,
    )?;
    let subject_id = keep.subject_id;
    crop_track_boxes_to_faces(&mut pipe, &keep.allowed_ids);
    protect_keep_geometry(&mut pipe, &keep.allowed_ids);
    let search_ms = elapsed_ms(t_search);
    eprintln!(
        "memory appearances={appearances} subjects={subjects} allowed={subject_id} keep_tracks={}",
        keep.keep_tracks
    );

    let t_compile = Instant::now();

    let package = opts.work_dir.join("vision_index");
    save_package(&pipe, &package)?;

    let plan = photo_except_plan(&video, &opts.photo, photo_box, &opts.output);
    let binding = photo_binding(&opts.photo, photo_box, keep.allowed_ids.clone());
    let bridged = resolve_bridge(
        &package,
        plan,
        &[binding],
        Some(&opts.output),
        &opts.work_dir,
        opts.redaction,
    )?;
    eprintln!(
        "compile redact_subjects={} graph={}",
        bridged.subjects,
        bridged.graph_path.display()
    );
    let compile_ms = elapsed_ms(t_compile);

    let t_encode = Instant::now();
    let written = run_graph(
        &bridged.graph_path,
        Some(&bridged.mask_package),
        Some(&opts.output),
    )?;
    let encode_ms = elapsed_ms(t_encode);
    let out_audio = crate::decode::probe_has_audio(Path::new(&written))?;
    if info.has_audio && !out_audio {
        return Err(crate::error::HostError::Encode(
            "source has audio but encode dropped it".into(),
        ));
    }
    let audio = if out_audio {
        AudioStatus::Kept
    } else {
        AudioStatus::None
    };
    let total_ms = elapsed_ms(t0);
    eprintln!("encode {encode_ms} ms; audio={audio:?}; total {total_ms} ms");

    Ok(PrivacyExceptResult {
        subject_id,
        output: written,
        package: package.to_string_lossy().into_owned(),
        graph: bridged.graph_path.to_string_lossy().into_owned(),
        frames: pictures.len(),
        peak_tracks: tracks,
        ingest_fps,
        phases_ms: PhaseTimings {
            extract: extract_ms,
            enroll: enroll_ms,
            ingest: ingest_ms,
            search: search_ms,
            compile: compile_ms,
            encode: encode_ms,
            total: total_ms,
        },
        audio,
    })
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Vision-only ingest for FPS (no photo, no encode).
#[derive(Debug, Clone, Serialize)]
pub struct IngestOnlyResult {
    /// Frames run through detect+track+embed.
    pub frames: usize,
    /// Peak tracks.
    pub peak_tracks: usize,
    /// Detect+reid FPS.
    pub ingest_fps: f64,
    /// Ingest milliseconds.
    pub ingest_ms: u64,
    /// Frame size.
    pub width: u32,
    /// Frame size.
    pub height: u32,
}

/// Detect+track+embed only (P1 throughput).
///
/// # Errors
///
/// Weights, ffmpeg, detect.
pub fn ingest_only(
    video: &Path,
    work_dir: &Path,
    models_dir: &Path,
    sample_fps: u32,
    max_frames: u32,
    live_secs: f64,
    embed_every: u32,
) -> Result<IngestOnlyResult> {
    let _ = crate::models::require_weights(models_dir).map_err(|e| {
        if let crate::error::HostError::MissingWeights(msg) = e {
            crate::error::HostError::MissingWeights(format!(
                "{msg}\n{}",
                crate::models::missing_weights_help(models_dir)
            ))
        } else {
            e
        }
    })?;
    std::fs::create_dir_all(work_dir)?;
    let video = materialize_video(video, work_dir, live_secs.max(0.2))?;
    let max_frames = applied_frame_cap(max_frames);
    let pictures = extract_sampled_pictures(
        &video,
        &fresh_frames_dir(work_dir),
        sample_fps.max(1),
        max_frames,
    )?;
    let mut pipe = open_pipeline("ingest-only", models_dir)?;
    add_video_source(&mut pipe, &video);
    let mut width = 0_u32;
    let mut height = 0_u32;
    let mut peak_tracks = 0_usize;
    let t0 = Instant::now();
    visit_rgb_batches(&pictures, RGB_BATCH_FRAMES, |batch| {
        if let Some(frame) = batch.first()
            && width == 0
        {
            width = frame.width;
            height = frame.height;
        }
        peak_tracks = peak_tracks.max(ingest_frames_strided(&mut pipe, batch, embed_every)?);
        Ok(())
    })?;
    let ingest_ms = elapsed_ms(t0);
    let ingest_fps = if ingest_ms == 0 {
        0.0
    } else {
        (pictures.len() as f64) * 1000.0 / ingest_ms as f64
    };
    Ok(IngestOnlyResult {
        frames: pictures.len(),
        peak_tracks,
        ingest_fps,
        ingest_ms,
        width,
        height,
    })
}

/// JPEG/PNG pixel box (full frame) for a photo file.
///
/// # Errors
///
/// Unreadable / undecodable image.
pub fn photo_full_box(photo: &Path) -> Result<[f32; 4]> {
    let bytes = std::fs::read(photo)?;
    let decoded = sightloom_host::decode_encoded_rgb(&bytes)
        .map_err(|e| crate::error::HostError::SightLoom(e.to_string()))?;
    Ok([0.0, 0.0, decoded.width as f32, decoded.height as f32])
}
