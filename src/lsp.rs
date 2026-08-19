//! Stdio LSP for Host job JSON and `SemanticEditPlan`.
//!
//! Same process, same contract. Not a `ReelForge-LSP` product.
//! Completions + hover + diagnostics. The job still runs through CLI / MCP.

use crate::error::Result;
use crate::mcp::METHODS;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::Path;

/// Legal `--style` / job `style` tokens. Host default is pixelate.
pub const STYLES: &[&str] = &["pixelate", "gaussian", "solid"];

/// Top-level keys on a Host job document (`privacy_except` args).
pub const JOB_KEYS: &[&str] = &[
    "video",
    "photo",
    "output",
    "style",
    "work_dir",
    "models_dir",
    "sample_fps",
    "max_frames",
    "embed_every",
];

/// `SemanticEdit.type` tags the compiler accepts.
pub const EDIT_TYPES: &[&str] = &[
    "blur_everyone_except",
    "blur_subject",
    "redact_pii",
    "follow_subject",
    "build_subject_reel",
    "build_most_frequent_subject_reel",
    "build_anomaly_reel",
    "create_event_clips",
];

/// `SubjectSelector.kind` tags.
pub const SELECTOR_KINDS: &[&str] = &[
    "frame_pick",
    "subject_set",
    "subject_ids",
    "track_ids",
    "most_frequent",
];

/// One diagnostic the editor can underline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LspDiagnostic {
    /// 0-based start line.
    pub line: u32,
    /// 0-based start character.
    pub character: u32,
    /// 1 = error, 2 = warning.
    pub severity: u32,
    /// Human message. Fail-closed copy when it is about Accept / style / paths.
    pub message: String,
}

/// One completion item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LspCompletion {
    /// Insert text.
    pub label: String,
    /// 1 = text, 12 = value, 14 = keyword.
    pub kind: u32,
    /// Short contract note.
    pub detail: String,
}

/// Serve LSP on stdin/stdout until `exit`.
///
/// # Errors
///
/// IO or JSON framing failure.
pub fn serve_lsp() -> Result<()> {
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let mut stdout = io::stdout();
    let mut docs: HashMap<String, String> = HashMap::new();
    loop {
        let Some(msg) = read_lsp_message(&mut stdin)? else {
            break;
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        if method == "exit" {
            break;
        }
        for out in handle_lsp(&mut docs, &msg) {
            write_lsp_message(&mut stdout, &out)?;
        }
    }
    Ok(())
}

/// Diagnose a buffer. Used by the server and by tests.
#[must_use]
pub fn diagnose_text(text: &str) -> Vec<LspDiagnostic> {
    if text.trim().is_empty() {
        return vec![LspDiagnostic {
            line: 0,
            character: 0,
            severity: 2,
            message: "empty document — a Host job needs video, photo, output".into(),
        }];
    }
    let parsed: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            let line = u32::try_from(e.line().saturating_sub(1)).unwrap_or(0);
            let character = u32::try_from(e.column().saturating_sub(1)).unwrap_or(0);
            return vec![LspDiagnostic {
                line,
                character,
                severity: 1,
                message: format!("invalid JSON: {e}"),
            }];
        }
    };
    let Some(obj) = parsed.as_object() else {
        return vec![LspDiagnostic {
            line: 0,
            character: 0,
            severity: 1,
            message: "root must be a JSON object (Host job or SemanticEditPlan)".into(),
        }];
    };

    if obj.contains_key("edits") || (obj.contains_key("media") && obj.contains_key("version")) {
        return diagnose_plan(text, obj);
    }
    if obj.contains_key("video")
        || obj.contains_key("photo")
        || obj.get("method").and_then(Value::as_str) == Some("tools/call")
    {
        return diagnose_job(text, obj);
    }
    vec![LspDiagnostic {
        line: 0,
        character: 0,
        severity: 2,
        message: "not a Host job (video/photo/output) and not a SemanticEditPlan (media/edits)"
            .into(),
    }]
}

/// Completions at a byte offset.
#[must_use]
pub fn completions_at(text: &str, offset: usize) -> Vec<LspCompletion> {
    let prefix = &text[..offset.min(text.len())];
    let ctx = prefix
        .rsplit(|c: char| c == '\n' || c == '{' || c == ',')
        .next()
        .unwrap_or(prefix)
        .trim();

    if ctx.contains("\"style\"") {
        return STYLES
            .iter()
            .map(|s| LspCompletion {
                label: (*s).to_string(),
                kind: 12,
                detail: style_detail(s).into(),
            })
            .collect();
    }
    if ctx.contains("\"type\"") {
        return EDIT_TYPES
            .iter()
            .map(|s| LspCompletion {
                label: (*s).to_string(),
                kind: 14,
                detail: edit_detail(s).into(),
            })
            .collect();
    }
    if ctx.contains("\"kind\"") {
        return SELECTOR_KINDS
            .iter()
            .map(|s| LspCompletion {
                label: (*s).to_string(),
                kind: 14,
                detail: selector_detail(s).into(),
            })
            .collect();
    }
    if ctx.contains("\"name\"") && prefix.contains("tools/call") {
        return METHODS
            .iter()
            .map(|s| LspCompletion {
                label: (*s).to_string(),
                kind: 14,
                detail: "Host MCP tool".into(),
            })
            .collect();
    }

    JOB_KEYS
        .iter()
        .map(|s| LspCompletion {
            label: (*s).to_string(),
            kind: 14,
            detail: job_key_detail(s).into(),
        })
        .chain(METHODS.iter().map(|s| LspCompletion {
            label: (*s).to_string(),
            kind: 14,
            detail: "Host MCP tool".into(),
        }))
        .collect()
}

/// Hover text for the token at `offset`.
#[must_use]
pub fn hover_at(text: &str, offset: usize) -> Option<String> {
    let token = token_at(text, offset)?;
    Some(match token.as_str() {
        "pixelate" => style_detail("pixelate").into(),
        "gaussian" => style_detail("gaussian").into(),
        "solid" => style_detail("solid").into(),
        "privacy_except" => {
            "Killer path: video + photo → Accept → redact everyone except that person.".into()
        }
        "search_photo" => {
            "Photo search. Host refuses to guess: no Accept → no subject.".into()
        }
        "blur_everyone_except" => {
            "Keep `allowed` sharp. Everyone else is redacted. FramePick must rewrite to SubjectIds."
                .into()
        }
        "frame_pick" => {
            "Click/box on a still. Fail-closed: must Accept before encode.".into()
        }
        "redact_pii" => "Plates / screens / text. Missing evidence is an error.".into(),
        "video" => "Path on the Host machine. Not a browser upload.".into(),
        "photo" => "Reference still of the one person who stays sharp.".into(),
        other if METHODS.contains(&other) => format!("Host MCP tool `{other}`"),
        _ => return None,
    })
}

fn diagnose_job(text: &str, obj: &serde_json::Map<String, Value>) -> Vec<LspDiagnostic> {
    let mut out = Vec::new();
    let args = obj
        .get("params")
        .and_then(|p| p.get("arguments"))
        .and_then(Value::as_object)
        .unwrap_or(obj);

    for key in ["video", "photo", "output"] {
        match args.get(key).and_then(Value::as_str) {
            None | Some("") => out.push(diag_on_key(
                text,
                key,
                1,
                format!("{key} required — Host job will not start"),
            )),
            Some(path) if looks_like_path(path) && !Path::new(path).exists() => out.push(
                diag_on_key(
                    text,
                    key,
                    1,
                    format!("{key} not found on this machine: {path}"),
                ),
            ),
            Some(_) => {}
        }
    }
    if let Some(style) = args.get("style").and_then(Value::as_str)
        && !STYLES.contains(&style)
    {
        out.push(diag_on_key(
            text,
            "style",
            1,
            format!("unknown redaction style `{style}` (pixelate | gaussian | solid)"),
        ));
    }
    out
}

fn diagnose_plan(text: &str, obj: &serde_json::Map<String, Value>) -> Vec<LspDiagnostic> {
    let mut out = Vec::new();
    if obj.get("media").and_then(Value::as_str).unwrap_or("").is_empty() {
        out.push(diag_on_key(
            text,
            "media",
            1,
            "SemanticEditPlan.media is required".into(),
        ));
    }
    let edits = obj.get("edits").and_then(Value::as_array);
    if edits.is_none_or(Vec::is_empty) {
        out.push(LspDiagnostic {
            line: 0,
            character: 0,
            severity: 2,
            message: "plan has no edits — nothing to compile".into(),
        });
    }
    if let Some(edits) = edits {
        for (i, edit) in edits.iter().enumerate() {
            let ty = edit.get("type").and_then(Value::as_str).unwrap_or("");
            if ty.is_empty() {
                out.push(LspDiagnostic {
                    line: 0,
                    character: 0,
                    severity: 1,
                    message: format!("edits[{i}] missing type"),
                });
                continue;
            }
            if !EDIT_TYPES.contains(&ty) {
                out.push(diag_on_key(
                    text,
                    "type",
                    1,
                    format!("unknown edit type `{ty}`"),
                ));
            }
            if ty == "blur_everyone_except" {
                let kind = edit
                    .get("allowed")
                    .and_then(|a| a.get("kind"))
                    .and_then(Value::as_str);
                if kind.is_none() {
                    out.push(LspDiagnostic {
                        line: 0,
                        character: 0,
                        severity: 1,
                        message: "blur_everyone_except needs allowed.kind (usually frame_pick)"
                            .into(),
                    });
                }
            }
        }
    }
    match serde_json::from_str::<reelforge_intelligence_core::SemanticEditPlan>(text) {
        Ok(_) => {}
        Err(e) => out.push(LspDiagnostic {
            line: u32::try_from(e.line().saturating_sub(1)).unwrap_or(0),
            character: u32::try_from(e.column().saturating_sub(1)).unwrap_or(0),
            severity: 1,
            message: format!("plan does not deserialize: {e}"),
        }),
    }
    out
}

fn handle_lsp(docs: &mut HashMap<String, String>, msg: &Value) -> Vec<Value> {
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let id = msg.get("id").cloned();
    match method {
        "initialize" => vec![ok(
            id,
            json!({
                "capabilities": {
                    "textDocumentSync": 1,
                    "completionProvider": { "triggerCharacters": ["\"", ":"] },
                    "hoverProvider": true
                },
                "serverInfo": {
                    "name": "reelforge-host",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        )],
        "shutdown" => vec![ok(id, Value::Null)],
        "textDocument/didOpen" => {
            let uri = uri_of(msg);
            let text = msg
                .pointer("/params/textDocument/text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            docs.insert(uri.clone(), text.clone());
            vec![publish(&uri, &text)]
        }
        "textDocument/didChange" => {
            let uri = uri_of(msg);
            let text = msg
                .pointer("/params/contentChanges/0/text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            docs.insert(uri.clone(), text.clone());
            vec![publish(&uri, &text)]
        }
        "textDocument/didClose" => {
            docs.remove(&uri_of(msg));
            Vec::new()
        }
        "textDocument/completion" => {
            let uri = uri_of(msg);
            let text = docs.get(&uri).cloned().unwrap_or_default();
            let offset = offset_of(&text, msg);
            let items: Vec<Value> = completions_at(&text, offset)
                .into_iter()
                .map(|c| {
                    json!({
                        "label": c.label,
                        "kind": c.kind,
                        "detail": c.detail,
                        "insertText": c.label
                    })
                })
                .collect();
            vec![ok(id, json!(items))]
        }
        "textDocument/hover" => {
            let uri = uri_of(msg);
            let text = docs.get(&uri).cloned().unwrap_or_default();
            let offset = offset_of(&text, msg);
            let value = match hover_at(&text, offset) {
                Some(s) => json!({ "contents": { "kind": "markdown", "value": s } }),
                None => Value::Null,
            };
            vec![ok(id, value)]
        }
        "initialized" | "textDocument/didSave" => Vec::new(),
        "" => Vec::new(),
        other if id.is_some() => vec![json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("method not found: {other}") }
        })],
        _ => Vec::new(),
    }
}

fn publish(uri: &str, text: &str) -> Value {
    let diags: Vec<Value> = diagnose_text(text)
        .into_iter()
        .map(|d| {
            json!({
                "range": {
                    "start": { "line": d.line, "character": d.character },
                    "end": { "line": d.line, "character": d.character + 1 }
                },
                "severity": d.severity,
                "source": "reelforge-host",
                "message": d.message
            })
        })
        .collect();
    json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": { "uri": uri, "diagnostics": diags }
    })
}

fn ok(id: Option<Value>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "result": result })
}

fn uri_of(msg: &Value) -> String {
    msg.pointer("/params/textDocument/uri")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn offset_of(text: &str, msg: &Value) -> usize {
    let line = msg
        .pointer("/params/position/line")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let character = msg
        .pointer("/params/position/character")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    line_char_to_offset(text, line, character)
}

fn line_char_to_offset(text: &str, line: usize, character: usize) -> usize {
    let mut remaining = line;
    let mut off = 0;
    for part in text.split_inclusive('\n') {
        if remaining == 0 {
            return off + character.min(part.trim_end_matches('\n').len());
        }
        remaining -= 1;
        off += part.len();
    }
    text.len()
}

fn token_at(text: &str, offset: usize) -> Option<String> {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let mut i = offset.min(bytes.len().saturating_sub(1));
    while i > 0 && is_token(bytes[i - 1]) {
        i -= 1;
    }
    let start = i;
    while i < bytes.len() && is_token(bytes[i]) {
        i += 1;
    }
    if start == i {
        return None;
    }
    Some(text[start..i].to_string())
}

fn is_token(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn looks_like_path(s: &str) -> bool {
    if s == "…" || s == "..." {
        return false;
    }
    s.contains('/') || s.contains('\\') || s.contains(':')
}

fn diag_on_key(text: &str, key: &str, severity: u32, message: String) -> LspDiagnostic {
    let needle = format!("\"{key}\"");
    if let Some(pos) = text.find(&needle) {
        let (line, character) = offset_to_line_char(text, pos);
        LspDiagnostic {
            line,
            character,
            severity,
            message,
        }
    } else {
        LspDiagnostic {
            line: 0,
            character: 0,
            severity,
            message,
        }
    }
}

fn offset_to_line_char(text: &str, offset: usize) -> (u32, u32) {
    let mut line = 0_u32;
    let mut last = 0;
    for (i, c) in text.char_indices() {
        if i >= offset {
            break;
        }
        if c == '\n' {
            line += 1;
            last = i + 1;
        }
    }
    let character = u32::try_from(offset.saturating_sub(last)).unwrap_or(0);
    (line, character)
}

fn style_detail(style: &str) -> &'static str {
    match style {
        "pixelate" => "Default. Mosaic. Use when the file leaves your control.",
        "gaussian" => "Soft blur. Recoverable — preview only, not anonymity.",
        "solid" => "Opaque black plate. Most defensible.",
        _ => "Redaction style",
    }
}

fn edit_detail(ty: &str) -> &'static str {
    match ty {
        "blur_everyone_except" => "Keep allowed sharp; redact everyone else.",
        "blur_subject" => "Redact one selected subject.",
        "redact_pii" => "Plates / screens / text. Fail-closed on missing evidence.",
        _ => "Semantic edit",
    }
}

fn selector_detail(kind: &str) -> &'static str {
    match kind {
        "frame_pick" => "Box on a still. Must Accept before encode.",
        "subject_ids" => "Already-frozen VisionIndex ids.",
        _ => "Subject selector",
    }
}

fn job_key_detail(key: &str) -> &'static str {
    match key {
        "video" => "Host-machine path to the source.",
        "photo" => "Still of the person who stays sharp.",
        "output" => "Where Host writes the redacted mp4.",
        "style" => "pixelate (default) | gaussian | solid",
        _ => "privacy_except argument",
    }
}

fn read_lsp_message(stdin: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut content_length = None;
    let mut header = String::new();
    loop {
        header.clear();
        let n = stdin.read_line(&mut header)?;
        if n == 0 {
            return Ok(None);
        }
        let line = header.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some(rest) = line.split_once(':') {
            if rest.0.eq_ignore_ascii_case("Content-Length") {
                content_length = Some(
                    rest.1
                        .trim()
                        .parse::<usize>()
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
                );
            }
        }
    }
    let Some(len) = content_length else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "LSP frame missing Content-Length",
        ));
    };
    let mut buf = vec![0_u8; len];
    stdin.read_exact(&mut buf)?;
    serde_json::from_slice(&buf).map(Some).map_err(io::Error::other)
}

fn write_lsp_message(stdout: &mut impl Write, value: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(value)?;
    write!(stdout, "Content-Length: {}\r\n\r\n", body.len())?;
    stdout.write_all(&body)?;
    stdout.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_missing_photo_is_error() {
        let diags = diagnose_text(r#"{ "video": "a.mp4", "output": "out.mp4" }"#);
        assert!(
            diags.iter().any(|d| d.message.contains("photo required")),
            "{diags:?}"
        );
    }

    #[test]
    fn bad_style_is_error() {
        let diags = diagnose_text(
            r#"{ "video": "a.mp4", "photo": "a.jpg", "output": "o.mp4", "style": "swirl" }"#,
        );
        assert!(
            diags.iter().any(|d| d.message.contains("swirl")),
            "{diags:?}"
        );
    }

    #[test]
    fn plan_deserializes_blur_everyone_except() {
        let text = r#"{
            "version": 2,
            "media": "scene.mp4",
            "edits": [{
                "type": "blur_everyone_except",
                "allowed": { "kind": "frame_pick", "media": "alice.jpg", "frame_index": 0, "box_xyxy": [0,0,1,1] }
            }]
        }"#;
        let diags: Vec<_> = diagnose_text(text)
            .into_iter()
            .filter(|d| d.severity == 1)
            .collect();
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn style_completions() {
        let text = r#"{ "style": "" }"#;
        let off = text.find("style").unwrap() + 8;
        let items = completions_at(text, off);
        let labels: Vec<_> = items.iter().map(|c| c.label.as_str()).collect();
        assert!(labels.contains(&"pixelate"), "{labels:?}");
        assert!(labels.contains(&"gaussian"), "{labels:?}");
    }

    #[test]
    fn hover_privacy_except() {
        let text = "privacy_except";
        let h = hover_at(text, 3).expect("hover");
        assert!(h.to_lowercase().contains("accept"), "{h}");
    }
}
