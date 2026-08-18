//! ONNX weight discovery. Weights stay on disk; this crate never vendors them.

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

/// Setup text printed on exit 2.
#[must_use]
pub fn missing_weights_help(dir: &Path) -> String {
    format!(
        "Place float32 ONNX models at:\n  {d}/person_detect.onnx   (YOLO NCHW RGB → boxes)\n  {d}/person_reid.onnx     (NCHW RGB → embedding, L2)\nHost does not download or vendor weights.",
        d = dir.display()
    )
}

fn first_existing(dir: &Path, names: &[&str]) -> Option<PathBuf> {
    names.iter().map(|n| dir.join(n)).find(|p| p.is_file())
}
