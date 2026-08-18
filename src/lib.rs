//! ReelForge Host — one process over SightLoom + Intelligence + ReelForge.
//!
//! Talk to this MCP, not four.

#![allow(
    clippy::doc_markdown,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::needless_pass_by_value
)]

mod compile;
mod decode;
mod encode;
mod error;
mod mcp;
mod models;
mod privacy;
mod vision;

pub use compile::{BridgeOut, photo_binding, photo_except_plan, resolve_bridge};
pub use decode::{
    RgbFrame, VideoInfo, extract_rgb_frames, grab_source, is_lavfi_token, is_live_token,
    materialize_video, probe_video,
};
pub use encode::run_graph;
pub use error::{HostError, MISSING_WEIGHTS_EXIT, Result};
pub use mcp::{HostService, MCP_PROTOCOL_VERSION, METHODS, dispatch, handle_jsonrpc, list_methods};
pub use models::{
    DEFAULT_MODELS_DIR, ModelPaths, missing_weights_help, require_weights, resolve_models_dir,
};
pub use privacy::{
    IngestOnlyResult, PhaseTimings, PrivacyExceptOpts, PrivacyExceptResult, ingest_only,
    photo_full_box, privacy_except,
};
pub use vision::{
    PhotoHit, add_video_source, enroll_photo, ingest_frames, ingest_frames_strided, open_pipeline,
    require_accept, search_photo,
};
