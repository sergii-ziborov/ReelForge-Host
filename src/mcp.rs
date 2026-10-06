//! JSON-RPC 2.0 MCP for the host process. Not the Intelligence compiler catalog.

use crate::compile::{parse_redaction_kind, photo_binding, resolve_bridge};
use crate::decode::{
    ExtractCancel, RGB_BATCH_FRAMES, applied_frame_cap, extract_sampled_pictures_cancellable,
    fresh_frames_dir, materialize_video, probe_video, sample_coverage,
    visit_rgb_batches_cancellable,
};
use crate::encode::run_graph;
use crate::error::{HostError, Result};
use crate::privacy::{PrivacyExceptOpts, privacy_except};
use crate::vision::{
    add_video_source, enroll_photo, ingest_frames, open_pipeline, require_accept, save_package,
    search_photo,
};
use reelforge_intelligence_core::{SemanticEditPlan, bindings_from_value, rewrite_selectors};
use serde_json::{Value, json};
use sightloom_host::HostPipeline;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// JSON-RPC protocol version (same family as Intelligence).
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Host MCP tool names.
pub const METHODS: &[&str] = &[
    "ingest_video",
    "enroll_photo",
    "search_photo",
    "rewrite_plan",
    "resolve_bridge",
    "run_graph",
    "privacy_except",
    "list_methods",
];

/// List tool names.
#[must_use]
pub fn list_methods() -> &'static [&'static str] {
    METHODS
}

/// Live session between MCP calls.
#[derive(Default)]
pub struct HostService {
    /// ONNX cache.
    pub models_dir: PathBuf,
    /// Default scratch dir.
    pub work_dir: PathBuf,
    pipe: Option<HostPipeline>,
    last_package: Option<PathBuf>,
    cancel: ExtractCancel,
    inflight: Arc<Mutex<Option<Value>>>,
    early: Arc<Mutex<Vec<Value>>>,
}

/// Shared cancel flag for one MCP session.
///
/// The reader records `notifications/cancelled` here while a call is still
/// inside [`handle_jsonrpc`]. Cloning shares the same flag.
#[derive(Clone, Debug)]
pub struct CancelScope {
    cancel: ExtractCancel,
    inflight: Arc<Mutex<Option<Value>>>,
    early: Arc<Mutex<Vec<Value>>>,
}

impl HostService {
    /// New service with default model / work dirs.
    #[must_use]
    pub fn new() -> Self {
        Self {
            models_dir: crate::models::resolve_models_dir(None),
            work_dir: PathBuf::from("work"),
            pipe: None,
            last_package: None,
            cancel: ExtractCancel::new(),
            inflight: Arc::new(Mutex::new(None)),
            early: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Flag and in-flight id shared with the MCP reader.
    #[must_use]
    pub fn cancel_scope(&self) -> CancelScope {
        CancelScope {
            cancel: self.cancel.clone(),
            inflight: Arc::clone(&self.inflight),
            early: Arc::clone(&self.early),
        }
    }

    fn arm_request(&self, id: &Value) -> InflightGuard {
        if self.take_early(id) {
            self.cancel.cancel();
        } else {
            self.cancel.reset();
        }
        *lock_mut(&self.inflight) = Some(id.clone());
        if self.take_early(id) {
            self.cancel.cancel();
        }
        InflightGuard {
            inflight: Arc::clone(&self.inflight),
            id: id.clone(),
        }
    }

    fn take_early(&self, id: &Value) -> bool {
        let mut early = lock_mut(&self.early);
        let hit = early.iter().any(|item| item == id);
        early.retain(|item| item != id);
        hit
    }

    fn pipe_mut(&mut self) -> Result<&mut HostPipeline> {
        self.pipe.as_mut().ok_or_else(|| {
            HostError::message("no session: call ingest_video or enroll_photo first")
        })
    }

    fn ensure_pipe(&mut self) -> Result<&mut HostPipeline> {
        if self.pipe.is_none() {
            self.pipe = Some(open_pipeline("mcp", &self.models_dir)?);
        }
        self.pipe
            .as_mut()
            .ok_or_else(|| HostError::message("pipeline missing"))
    }
}

impl CancelScope {
    /// Whether the in-flight request has been cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Record `notifications/cancelled` and return true when `raw` is that notification.
    ///
    /// The caller must not dispatch a consumed line. A missing `requestId` is ignored.
    #[must_use]
    pub fn take_cancel_line(&self, raw: &str) -> bool {
        let Some(id) = cancelled_request_id(raw) else {
            return false;
        };
        self.note_cancelled(&id);
        true
    }

    fn note_cancelled(&self, id: &Value) {
        if inflight_is(&self.inflight, id) {
            self.cancel.cancel();
            return;
        }
        {
            let mut early = lock_mut(&self.early);
            if !early.iter().any(|item| item == id) {
                if early.len() == 32 {
                    early.remove(0);
                }
                early.push(id.clone());
            }
        }
        if inflight_is(&self.inflight, id) {
            self.cancel.cancel();
        }
    }
}

fn cancelled_request_id(raw: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(raw).ok()?;
    if value.get("method").and_then(Value::as_str) != Some("notifications/cancelled") {
        return None;
    }
    value
        .get("params")
        .and_then(|params| params.get("requestId"))
        .filter(|id| !id.is_null())
        .cloned()
}

fn inflight_is(inflight: &Mutex<Option<Value>>, id: &Value) -> bool {
    lock_mut(inflight).as_ref() == Some(id)
}

fn lock_mut<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct InflightGuard {
    inflight: Arc<Mutex<Option<Value>>>,
    id: Value,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        let mut slot = lock_mut(&self.inflight);
        if slot.as_ref() == Some(&self.id) {
            *slot = None;
        }
    }
}

/// Handle one JSON-RPC 2.0 line. Notifications return `None`.
#[must_use]
pub fn handle_jsonrpc(svc: &mut HostService, raw: &str) -> Option<Value> {
    let parsed: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            return Some(jsonrpc_error(
                &Value::Null,
                -32700,
                format!("parse error: {e}"),
            ));
        }
    };
    let id = parsed.get("id").cloned().unwrap_or(Value::Null);
    let method = parsed.get("method").and_then(Value::as_str).unwrap_or("");
    let params = parsed.get("params").cloned().unwrap_or(Value::Null);
    let is_notification = parsed.get("id").is_none();
    let _inflight = parsed
        .get("id")
        .filter(|value| !value.is_null())
        .map(|value| svc.arm_request(value));

    if method == "notifications/cancelled" {
        if let Some(request_id) = params.get("requestId").filter(|value| !value.is_null()) {
            svc.cancel_scope().note_cancelled(request_id);
        }
        return if is_notification {
            None
        } else {
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": {} }))
        };
    }

    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "reelforge-host",
                "version": env!("CARGO_PKG_VERSION")
            }
        })),
        "notifications/initialized" | "initialized" | "ping" => Ok(json!({})),
        #[cfg(test)]
        "wait_for_cancel" => wait_for_cancel(svc),
        "tools/list" => Ok(json!({ "tools": mcp_tools() })),
        "tools/call" => match params.get("name").and_then(Value::as_str) {
            None => Err(HostError::message("tools/call: name required")),
            Some(name) => {
                let args = params.get("arguments").cloned().unwrap_or(Value::Null);
                dispatch(svc, name, &args).map(|value| {
                    json!({
                        "content": [{ "type": "text", "text": value.to_string() }],
                        "structuredContent": value
                    })
                })
            }
        },
        "shutdown" => Ok(Value::Bool(true)),
        "" => Err(HostError::message("method required")),
        other => {
            if METHODS.contains(&other) {
                dispatch(svc, other, &params)
            } else {
                return Some(jsonrpc_error(
                    &id,
                    -32601,
                    format!("method not found: {other}"),
                ));
            }
        }
    };

    if is_notification {
        return None;
    }
    match result {
        Ok(value) => Some(json!({ "jsonrpc": "2.0", "id": id, "result": value })),
        Err(e) => Some(jsonrpc_error(&id, -32603, e.to_string())),
    }
}

/// Dispatch one host tool.
///
/// # Errors
///
/// Unknown method or tool failure.
pub fn dispatch(svc: &mut HostService, method: &str, args: &Value) -> Result<Value> {
    match method {
        "list_methods" => Ok(json!(METHODS)),
        "ingest_video" => ingest_video(svc, args),
        "enroll_photo" => enroll(svc, args),
        "search_photo" => search(svc, args),
        "rewrite_plan" => rewrite(args),
        "resolve_bridge" => resolve(svc, args),
        "run_graph" => run(args),
        "privacy_except" => except(svc, args),
        other => Err(HostError::message(format!("unknown host method `{other}`"))),
    }
}

#[allow(clippy::too_many_lines)]
fn mcp_tools() -> Vec<Value> {
    vec![
        tool(
            "privacy_except",
            "Killer path: video + photo of the one person to keep sharp. Photo search must Accept or the job fails. Default style is pixelate.",
            json!({
                "type": "object",
                "required": ["video", "photo", "output"],
                "properties": {
                    "video": { "type": "string", "description": "Path on the Host machine" },
                    "photo": { "type": "string", "description": "Reference still of the allowed person" },
                    "output": { "type": "string", "description": "Output mp4 path on the Host machine" },
                    "style": { "type": "string", "enum": ["pixelate", "gaussian", "solid"], "default": "pixelate" },
                    "work_dir": { "type": "string" },
                    "models_dir": { "type": "string" },
                    "sample_fps": { "type": "integer", "default": 5 },
                    "max_frames": { "type": "integer", "default": 0, "description": "0 uses the Host analysis cap. Extraction stops before further frames are decoded." },
                    "embed_every": { "type": "integer", "default": 1 }
                }
            }),
        ),
        tool(
            "search_photo",
            "Rank gallery hits for a still. Host never picks the nearest face. Check accepted.",
            json!({
                "type": "object",
                "required": ["photo"],
                "properties": {
                    "photo": { "type": "string" },
                    "top_k": { "type": "integer", "default": 3 }
                }
            }),
        ),
        tool(
            "enroll_photo",
            "JPEG/PNG → gallery subject id.",
            json!({
                "type": "object",
                "required": ["photo"],
                "properties": { "photo": { "type": "string" } }
            }),
        ),
        tool(
            "ingest_video",
            "Decode + detect/track/embed + save VisionIndex package.",
            json!({
                "type": "object",
                "required": ["video"],
                "properties": {
                    "video": { "type": "string" },
                    "work_dir": { "type": "string" },
                    "sample_fps": { "type": "integer", "default": 5 },
                    "max_frames": { "type": "integer", "default": 0, "description": "0 uses the Host analysis cap. Extraction stops before further frames are decoded." }
                }
            }),
        ),
        tool(
            "rewrite_plan",
            "Rewrite FramePick selectors to SubjectIds using bindings.",
            json!({
                "type": "object",
                "required": ["plan"],
                "properties": {
                    "plan": { "type": "object" },
                    "bindings": { "type": "array" }
                }
            }),
        ),
        tool(
            "resolve_bridge",
            "Intelligence freeze + mask package + RenderGraph JSON.",
            json!({
                "type": "object",
                "required": ["plan"],
                "properties": {
                    "plan": { "type": "object" },
                    "package": { "type": "string" },
                    "photo": { "type": "string" },
                    "subject_id": { "type": "integer" },
                    "output": { "type": "string" },
                    "style": { "type": "string", "enum": ["pixelate", "gaussian", "solid"] }
                }
            }),
        ),
        tool(
            "run_graph",
            "Encode a compiled RenderGraph. Source audio is muxed or the job fails.",
            json!({
                "type": "object",
                "required": ["graph"],
                "properties": {
                    "graph": { "type": "string" },
                    "mask_package": { "type": "string" },
                    "output": { "type": "string" }
                }
            }),
        ),
        tool(
            "list_methods",
            "List Host MCP tool names. Intelligence compile_plan is not here.",
            json!({ "type": "object", "properties": {} }),
        ),
    ]
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema
    })
}

fn jsonrpc_error(id: &Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() }
    })
}

fn arg_path(args: &Value, key: &str) -> Result<PathBuf> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| HostError::message(format!("{key} required")))
}

fn opt_path(args: &Value, key: &str, fallback: impl FnOnce() -> PathBuf) -> PathBuf {
    args.get(key)
        .and_then(Value::as_str)
        .map_or_else(fallback, PathBuf::from)
}

fn ingest_video(svc: &mut HostService, args: &Value) -> Result<Value> {
    let video = arg_path(args, "video")?;
    let work = opt_path(args, "work_dir", || svc.work_dir.clone());
    let fps = args
        .get("sample_fps")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .max(1) as u32;
    let live_secs = args.get("live_secs").and_then(Value::as_f64).unwrap_or(3.0);
    let max_frames =
        applied_frame_cap(args.get("max_frames").and_then(Value::as_u64).unwrap_or(0) as u32);
    let video = materialize_video(&video, &work, live_secs)?;
    let info = probe_video(&video)?;
    let frames_dir = fresh_frames_dir(&work);
    let cancel = svc.cancel.clone();
    let pictures =
        extract_sampled_pictures_cancellable(&video, &frames_dir, fps, max_frames, Some(&cancel))?;
    let pipe = svc.ensure_pipe()?;
    add_video_source(pipe, &video);
    let mut tracks = 0_usize;
    visit_rgb_batches_cancellable(&pictures, RGB_BATCH_FRAMES, Some(&cancel), |batch| {
        if cancel.is_cancelled() {
            return Err(HostError::Ffmpeg("extract cancelled".into()));
        }
        tracks = tracks.max(ingest_frames(pipe, batch)?);
        Ok(())
    })?;
    let package = work.join("vision_index");
    save_package(pipe, &package)?;
    svc.last_package = Some(package.clone());
    svc.work_dir = work;
    Ok(json!({
        "frames": pictures.len(),
        "max_frames": max_frames,
        "coverage": sample_coverage(&pictures, fps, max_frames, info.duration_secs),
        "tracks": tracks,
        "width": info.width,
        "height": info.height,
        "package": package,
    }))
}

fn enroll(svc: &mut HostService, args: &Value) -> Result<Value> {
    let photo = arg_path(args, "photo")?;
    let bytes = std::fs::read(&photo)?;
    let pipe = svc.ensure_pipe()?;
    let id = enroll_photo(pipe, &bytes)?;
    Ok(json!({ "subject_id": id }))
}

fn search(svc: &mut HostService, args: &Value) -> Result<Value> {
    let photo = arg_path(args, "photo")?;
    let top_k = args.get("top_k").and_then(Value::as_u64).unwrap_or(3) as usize;
    let bytes = std::fs::read(&photo)?;
    let pipe = svc.pipe_mut()?;
    let hits = search_photo(pipe, &bytes, top_k)?;
    let accepted = require_accept(&hits).ok();
    Ok(json!({ "hits": hits, "accepted": accepted }))
}

fn rewrite(args: &Value) -> Result<Value> {
    let plan = args
        .get("plan")
        .ok_or_else(|| HostError::message("rewrite_plan: plan required"))?;
    let plan: SemanticEditPlan =
        serde_json::from_value(plan.clone()).map_err(|e| HostError::Intelligence(e.to_string()))?;
    let bindings = bindings_from_value(args.get("bindings").unwrap_or(&Value::Null))
        .map_err(|e| HostError::Intelligence(e.to_string()))?;
    let out =
        rewrite_selectors(plan, &bindings).map_err(|e| HostError::Intelligence(e.to_string()))?;
    Ok(serde_json::to_value(out)?)
}

fn resolve(svc: &HostService, args: &Value) -> Result<Value> {
    let package = args
        .get("package")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| svc.last_package.clone())
        .ok_or_else(|| HostError::message("resolve_bridge: package required"))?;
    let plan = args
        .get("plan")
        .ok_or_else(|| HostError::message("resolve_bridge: plan required"))?;
    let mut plan: SemanticEditPlan =
        serde_json::from_value(plan.clone()).map_err(|e| HostError::Intelligence(e.to_string()))?;
    let bindings = bindings_from_value(args.get("bindings").unwrap_or(&Value::Null))
        .map_err(|e| HostError::Intelligence(e.to_string()))?;
    if let Some(photo) = args.get("photo").and_then(Value::as_str) {
        let sid = args
            .get("subject_id")
            .and_then(Value::as_u64)
            .ok_or_else(|| HostError::message("resolve_bridge: subject_id required with photo"))?;
        let box_xyxy = crate::privacy::photo_full_box(Path::new(photo))?;
        let extra = photo_binding(Path::new(photo), box_xyxy, vec![sid]);
        plan = rewrite_selectors(plan, &[extra])
            .map_err(|e| HostError::Intelligence(e.to_string()))?;
    }
    let output = args.get("output").and_then(Value::as_str).map(Path::new);
    let work = opt_path(args, "work_dir", || svc.work_dir.clone());
    let kind = parse_redaction_kind(args.get("style").and_then(Value::as_str))?;
    let out = resolve_bridge(&package, plan, &bindings, output, &work, kind)?;
    Ok(serde_json::to_value(out)?)
}

fn run(args: &Value) -> Result<Value> {
    let graph = arg_path(args, "graph")?;
    let masks = args
        .get("mask_package")
        .and_then(Value::as_str)
        .map(Path::new);
    let output = args.get("output").and_then(Value::as_str).map(Path::new);
    let written = run_graph(&graph, masks, output)?;
    let audio = crate::decode::probe_has_audio(Path::new(&written)).unwrap_or(false);
    Ok(json!({ "output": written, "audio": audio }))
}

fn except(svc: &HostService, args: &Value) -> Result<Value> {
    let opts = PrivacyExceptOpts {
        video: arg_path(args, "video")?,
        photo: arg_path(args, "photo")?,
        output: arg_path(args, "output")?,
        work_dir: opt_path(args, "work_dir", || PathBuf::from("work")),
        models_dir: opt_path(args, "models_dir", || {
            crate::models::resolve_models_dir(None)
        }),
        sample_fps: args
            .get("sample_fps")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .max(1) as u32,
        max_frames: args.get("max_frames").and_then(Value::as_u64).unwrap_or(0) as u32,
        live_secs: args.get("live_secs").and_then(Value::as_f64).unwrap_or(3.0),
        embed_every: args.get("embed_every").and_then(Value::as_u64).unwrap_or(1) as u32,
        redaction: parse_redaction_kind(args.get("style").and_then(Value::as_str))?,
        cancel: Some(svc.cancel.clone()),
    };
    let out = privacy_except(&opts)?;
    Ok(serde_json::to_value(out)?)
}

#[cfg(test)]
fn wait_for_cancel(svc: &HostService) -> Result<Value> {
    let start = std::time::Instant::now();
    while !svc.cancel.is_cancelled() {
        if start.elapsed() > std::time::Duration::from_secs(3) {
            return Err(HostError::message("cancel did not arrive"));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Err(HostError::Ffmpeg("extract cancelled".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_scope_hits_only_the_inflight_id() {
        let svc = HostService::new();
        let scope = svc.cancel_scope();
        let guard = svc.arm_request(&json!(4));
        assert!(scope.take_cancel_line(
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":9}}"#
        ));
        assert!(!scope.is_cancelled());
        assert!(scope.take_cancel_line(
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":4}}"#
        ));
        assert!(scope.is_cancelled());
        drop(guard);
        let _next = svc.arm_request(&json!(5));
        assert!(!scope.is_cancelled());
        assert!(!scope.take_cancel_line(r#"{"jsonrpc":"2.0","method":"ping"}"#));
    }

    #[test]
    fn early_cancel_is_waiting_when_the_request_starts() {
        let svc = HostService::new();
        let scope = svc.cancel_scope();
        assert!(scope.take_cancel_line(
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"job-1"}}"#
        ));
        assert!(!scope.is_cancelled());
        let _guard = svc.arm_request(&json!("job-1"));
        assert!(scope.is_cancelled());
    }
}
