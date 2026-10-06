//! ReelForge Host — one process over SightLoom + Intelligence + ReelForge.
//!
//! Talk to this MCP, not four.

#![allow(
    clippy::doc_markdown,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::needless_pass_by_value
)]

mod capture;
mod compile;
mod decode;
mod encode;
mod error;
mod http;
mod lsp;
mod mcp;
mod models;
mod privacy;
mod vision;

pub use capture::{
    capture_input, is_capture_project_file, is_capture_session_dir, is_capture_token,
    materialize_capture, resolve_capture_videos,
};
pub use compile::{
    BridgeOut, parse_redaction_kind, photo_binding, photo_except_plan, resolve_bridge,
};
pub use decode::{
    ANALYSIS_MAX_FRAMES, ExtractCancel, ProxyMap, ProxySample, RgbFrame, VideoInfo,
    applied_frame_cap, extract_rgb_frames, extract_rgb_frames_cancellable,
    extract_rgb_frames_limited, fresh_frames_dir, grab_source, is_lavfi_token, is_live_token,
    materialize_video, probe_has_audio, probe_video,
};
pub use encode::run_graph;
pub use error::{HostError, MISSING_WEIGHTS_EXIT, Result};
pub use http::{
    DEFAULT_HTTP_BIND, HttpServeOpts, is_loopback_bind, require_token_for_bind, serve_http,
    serve_http_listener,
};
pub use lsp::{
    EDIT_TYPES, JOB_KEYS, LspCompletion, LspDiagnostic, SELECTOR_KINDS, STYLES, completions_at,
    diagnose_text, hover_at, serve_lsp,
};
pub use mcp::{HostService, MCP_PROTOCOL_VERSION, METHODS, dispatch, handle_jsonrpc, list_methods};
pub use models::{
    DEFAULT_MODELS_DIR, ModelPaths, missing_weights_help, require_weights, resolve_models_dir,
};
pub use privacy::{
    AudioStatus, IngestOnlyResult, PhaseTimings, PrivacyExceptOpts, PrivacyExceptResult,
    ingest_only, photo_full_box, privacy_except,
};
pub use vision::{
    KeepIdentity, PhotoHit, add_video_source, best_video_accept, crop_track_boxes_to_faces,
    enroll_photo, face_xyxy, ingest_frames, ingest_frames_strided, merge_keep_identity,
    open_pipeline, protect_keep_geometry, require_accept, require_video_accept, search_photo,
    search_video_tracks,
};
