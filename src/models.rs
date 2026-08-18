//! ONNX weight discovery. Weights stay on disk; this crate never vendors them.
//!
//! SightLoom owns the loaders. Host owns *where* the files sit (this process,
//! `REELFORGE_MODELS`, or the sibling `../SightLoom/.sightloom-models` cache).

use crate::error::{HostError, Result};
use std::path::{Path, PathBuf};

/// Default cache directory (same name SightLoom host uses).
pub const DEFAULT_MODELS_DIR: &str = ".sightloom-models";

/// Detector file names tried in order.
pub const DETECT_NAMES: &[&str] = &["person_detect.onnx", "yolov8n.onnx"];

/// Re-id file names tried in order.
pub const REID_NAMES: &[&str] = &["person_reid.onnx"];

/// Resolved weight paths.
#[derive(Debug, Clone)]
pub struct ModelPaths {
    /// Cache / models directory.
    pub dir: PathBuf,
    /// Person detector ONNX.
    pub detect: PathBuf,
    /// Person re-id ONNX.
    pub reid: PathBuf,
}

/// Look up required ONNX files.
///
/// # Errors
///
/// [`HostError::MissingWeights`] when detector or re-id is absent.
pub fn require_weights(dir: impl AsRef<Path>) -> Result<ModelPaths> {
    let dir = dir.as_ref().to_path_buf();
    let detect = first_existing(&dir, DETECT_NAMES).ok_or_else(|| {
        HostError::MissingWeights(format!(
            "no {} under {}",
            DETECT_NAMES.join(" / "),
            dir.display()
        ))
    })?;
    let reid = first_existing(&dir, REID_NAMES).ok_or_else(|| {
        HostError::MissingWeights(format!(
            "no {} under {}",
            REID_NAMES.join(" / "),
            dir.display()
        ))
    })?;
    Ok(ModelPaths { dir, detect, reid })
}

/// Pick a cache that already has both ONNX files.
///
/// Order: `explicit` → `REELFORGE_MODELS` / `SIGHTLOOM_MODELS` → `./.sightloom-models`
/// → sibling `../SightLoom/.sightloom-models`. If nothing is ready, returns the
/// explicit/default path so [`require_weights`] can emit exit 2.
#[must_use]
pub fn resolve_models_dir(explicit: Option<&Path>) -> PathBuf {
    if let Some(dir) = explicit {
        return dir.to_path_buf();
    }
    let mut candidates = Vec::new();
    for key in ["REELFORGE_MODELS", "SIGHTLOOM_MODELS"] {
        if let Ok(dir) = std::env::var(key) {
            let dir = dir.trim();
            if !dir.is_empty() {
                candidates.push(PathBuf::from(dir));
            }
        }
    }
    candidates.push(PathBuf::from(DEFAULT_MODELS_DIR));
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("..").join("SightLoom").join(DEFAULT_MODELS_DIR));
    }
    candidates.push(PathBuf::from("../SightLoom/.sightloom-models"));

    for dir in &candidates {
        if require_weights(dir).is_ok() {
            return dir.clone();
        }
    }
    PathBuf::from(DEFAULT_MODELS_DIR)
}

/// Setup text printed on exit 2.
#[must_use]
pub fn missing_weights_help(dir: &Path) -> String {
    format!(
        "Place float32 ONNX models at:\n  {d}/person_detect.onnx   (YOLO NCHW RGB → boxes)\n  {d}/person_reid.onnx     (NCHW RGB → embedding, L2)\nor copy the sibling cache ../SightLoom/.sightloom-models (yolov8n.onnx + person_reid.onnx).\nEnv: REELFORGE_MODELS / SIGHTLOOM_MODELS. Host does not vendor weights.",
        d = dir.display()
    )
}

fn first_existing(dir: &Path, names: &[&str]) -> Option<PathBuf> {
    names.iter().map(|n| dir.join(n)).find(|p| p.is_file())
}
