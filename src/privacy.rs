//! Killer path: video + photo → blur everyone except the accepted subject.

use crate::compile::{photo_binding, photo_except_plan, resolve_bridge};
use crate::decode::{extract_rgb_frames, probe_video};
use crate::encode::run_graph;
use crate::error::Result;
use crate::vision::{
    add_video_source, enroll_photo, finalize_identities, ingest_frames, open_pipeline,
    require_accept, save_package, search_photo,
};
use serde::Serialize;
use std::path::{Path, PathBuf};

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
    /// Extracted frame rate for ingest.
    pub sample_fps: u32,
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
}

/// Full pipeline. Missing weights → [`crate::HostError::MissingWeights`] (exit 2).
///
/// # Errors
///
/// Weights, ffmpeg, photo not Accept, compile, encode.
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
    std::fs::create_dir_all(&opts.work_dir)?;
    let _info = probe_video(&opts.video)?;
    eprintln!(
        "extract frames @ {} fps from {}",
        opts.sample_fps.max(1),
        opts.video.display()
    );
    let frames = extract_rgb_frames(
        &opts.video,
        &opts.work_dir.join("frames"),
        opts.sample_fps.max(1),
    )?;
    eprintln!("extracted {} frames", frames.len());

    let mut pipe = open_pipeline("privacy-except", &opts.models_dir)?;
    add_video_source(&mut pipe, &opts.video);

    let photo_bytes = std::fs::read(&opts.photo)?;
    let decoded = sightloom_host::decode_encoded_rgb(&photo_bytes)
        .map_err(|e| crate::error::HostError::SightLoom(e.to_string()))?;
    let photo_box = [0.0, 0.0, decoded.width as f32, decoded.height as f32];

    let enrolled = enroll_photo(&mut pipe, &photo_bytes)?;
    eprintln!("enrolled photo subject={enrolled}");
    let tracks = ingest_frames(&mut pipe, &frames)?;
    eprintln!("ingested frames={} peak_tracks={tracks}", frames.len());
    let hits = search_photo(&mut pipe, &photo_bytes, 3)?;
    for h in &hits {
        eprintln!(
            "search hit subject={} score={:.3} {}",
            h.subject_id, h.score, h.decision
        );
    }
    let subject_id = require_accept(&hits)?;
    let last_pts = frames
        .last()
        .and_then(|f| sightloom::core::MediaTime::new(f.ticks, f.timescale).ok())
        .ok_or_else(|| crate::error::HostError::message("no frames for identity resolve"))?;
    let (appearances, subjects) = finalize_identities(&mut pipe, subject_id, last_pts)?;
    eprintln!("memory appearances={appearances} subjects={subjects} allowed={subject_id}");
    let _ = enrolled;

    let package = opts.work_dir.join("vision_index");
    save_package(&pipe, &package)?;

    let plan = photo_except_plan(&opts.video, &opts.photo, photo_box, &opts.output);
    let binding = photo_binding(&opts.photo, photo_box, subject_id);
    let bridged = resolve_bridge(
        &package,
        plan,
        &[binding],
        Some(&opts.output),
        &opts.work_dir,
    )?;
    let written = run_graph(
        &bridged.graph_path,
        Some(&bridged.mask_package),
        Some(&opts.output),
    )?;

    Ok(PrivacyExceptResult {
        subject_id,
        output: written,
        package: package.to_string_lossy().into_owned(),
        graph: bridged.graph_path.to_string_lossy().into_owned(),
        frames: frames.len(),
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
