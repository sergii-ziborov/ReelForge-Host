//! JSON-RPC 2.0 MCP for the host process. Not the Intelligence compiler catalog.

use crate::compile::{parse_redaction_kind, photo_binding, resolve_bridge};
use crate::decode::{extract_rgb_frames, materialize_video, probe_video};
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
        }
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
        "privacy_except" => except(args),
        other => Err(HostError::message(format!("unknown host method `{other}`"))),
    }
}

fn mcp_tools() -> Vec<Value> {
    METHODS
        .iter()
        .map(|name| {
            json!({
                "name": name,
                "description": format!("ReelForge Host method `{name}`"),
                "inputSchema": { "type": "object", "additionalProperties": true }
            })
        })
        .collect()
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
    let max_frames = args.get("max_frames").and_then(Value::as_u64).unwrap_or(0) as u32;
    let video = materialize_video(&video, &work, live_secs)?;
    let info = probe_video(&video)?;
    let mut frames = extract_rgb_frames(&video, &work.join("frames"), fps)?;
    if max_frames > 0 && frames.len() > max_frames as usize {
        frames.truncate(max_frames as usize);
    }
    let pipe = svc.ensure_pipe()?;
    add_video_source(pipe, &video);
    let tracks = ingest_frames(pipe, &frames)?;
    let package = work.join("vision_index");
    save_package(pipe, &package)?;
    svc.last_package = Some(package.clone());
    svc.work_dir = work;
    Ok(json!({
        "frames": frames.len(),
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
        let extra = photo_binding(Path::new(photo), box_xyxy, sid);
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

fn except(args: &Value) -> Result<Value> {
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
        embed_every: args
            .get("embed_every")
            .and_then(Value::as_u64)
            .unwrap_or(1) as u32,
        redaction: parse_redaction_kind(args.get("style").and_then(Value::as_str))?,
    };
    let out = privacy_except(&opts)?;
    Ok(serde_json::to_value(out)?)
}
