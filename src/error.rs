//! Host errors. Exit code 2 is reserved for missing ONNX weights.

use std::process::ExitCode;

/// Process exit when detector / re-id weights are not on disk.
pub const MISSING_WEIGHTS_EXIT: u8 = 2;

/// Host orchestrator error.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// Required ONNX files are not in the models directory.
    #[error("weights not ready: {0}")]
    MissingWeights(String),
    /// ffmpeg / ffprobe failed or is not on PATH.
    #[error("ffmpeg: {0}")]
    Ffmpeg(String),
    /// Photo search did not Accept — do not guess a subject.
    #[error("photo search did not Accept: {0}")]
    PhotoNotAccepted(String),
    /// Intelligence compile / rewrite / resolve.
    #[error("intelligence: {0}")]
    Intelligence(String),
    /// ReelForge encode.
    #[error("reelforge: {0}")]
    Encode(String),
    /// SightLoom session / pipeline.
    #[error("sightloom: {0}")]
    SightLoom(String),
    /// I/O.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// Other.
    #[error("{0}")]
    Message(String),
}

impl HostError {
    /// Map to a process exit code (`2` = missing weights).
    #[must_use]
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::MissingWeights(_) => ExitCode::from(MISSING_WEIGHTS_EXIT),
            _ => ExitCode::FAILURE,
        }
    }

    /// Helper.
    #[must_use]
    pub fn message(msg: impl Into<String>) -> Self {
        Self::Message(msg.into())
    }
}

/// Result alias.
pub type Result<T> = std::result::Result<T, HostError>;
