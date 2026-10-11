//! Qualify the native CLI execution trace, not an agent's prose or skill name.

use super::store;
use crate::{HostError, Result};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

pub(super) fn qualify(events: &Path, image: &Path) -> Result<Value> {
    let entries = records(events)?;
    let thread = entries
        .iter()
        .find(|v| v["type"] == "thread.started")
        .and_then(|v| v["thread_id"].as_str())
        .ok_or_else(|| error("CLI thread receipt missing"))?;
    if thread.len() != 36 || !thread.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return Err(error("invalid CLI thread id"));
    }
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .map(|p| PathBuf::from(p).join(".codex"))
        })
        .ok_or_else(|| error("Codex home unavailable for execution receipts"))?;
    let generated = home.join("generated_images").join(thread).canonicalize()?;
    let image = image.canonicalize()?;
    if image.parent() != Some(generated.as_path())
        || !image
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("png"))
    {
        return Err(error(
            "image does not belong to this native generation thread",
        ));
    }
    let outputs = std::fs::read_dir(&generated)?
        .filter_map(std::result::Result::ok)
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("png"))
        })
        .count();
    if outputs != 1 {
        return Err(error("native role must produce exactly one PNG"));
    }
    let trace = find_trace(&home.join("sessions"), thread, 3)?
        .ok_or_else(|| error("native CLI execution trace unavailable; profile unsupported"))?;
    let (calls, completed) = native_calls(&records(&trace)?)?;
    if calls.len() != 1 || calls != completed {
        return Err(error(
            "exactly one completed native image generation is required",
        ));
    }
    Ok(
        json!({"profile":"codex-exec-session-trace-v1","thread_id":thread,"native_call_ids":calls,
        "native_png_outputs":outputs,"events_sha256":store::file_digest(events)?,
        "trace":trace,"trace_sha256":store::file_digest(&trace)?,"generated_path":image,"generated_sha256":store::file_digest(&image)?}),
    )
}

fn records(path: &Path) -> Result<Vec<Value>> {
    BufReader::new(std::fs::File::open(path)?)
        .lines()
        .map(|line| serde_json::from_str(&line?).map_err(HostError::from))
        .collect()
}

fn find_trace(dir: &Path, thread: &str, depth: u8) -> Result<Option<PathBuf>> {
    if !dir.is_dir() {
        return Ok(None);
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_file()
            && path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with(&format!("-{thread}.jsonl")))
        {
            return Ok(Some(path));
        }
        if depth > 0
            && path.is_dir()
            && let Some(found) = find_trace(&path, thread, depth - 1)?
        {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

fn native_calls(entries: &[Value]) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    let mut calls = BTreeSet::new();
    let mut completed = BTreeSet::new();
    for entry in entries.iter().filter(|v| v["type"] == "response_item") {
        let p = &entry["payload"];
        let id = p["call_id"].as_str().unwrap_or("");
        if p["type"] == "custom_tool_call" && p["name"] == "exec" {
            let input = p["input"].as_str().unwrap_or("");
            let count = input.matches("tools.image_gen__imagegen(").count();
            if count > 1 {
                return Err(error("multiple native image calls in one production role"));
            }
            if count == 1 {
                calls.insert(id.to_owned());
            }
        }
        if p["type"] == "custom_tool_call_output"
            && calls.contains(id)
            && p["output"].as_array().is_some_and(|parts| {
                parts.iter().any(|v| {
                    v["text"]
                        .as_str()
                        .is_some_and(|s| s.starts_with("Script completed"))
                })
            })
        {
            completed.insert(id.to_owned());
        }
    }
    Ok((calls, completed))
}

fn error(message: &str) -> HostError {
    HostError::message(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prose_and_skill_reads_do_not_prove_native_execution() {
        let entries = vec![
            json!({"type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"skill","input":"Read imagegen SKILL.md"}}),
            json!({"type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"native","input":"const r = await tools.image_gen__imagegen({prompt:'asset'}); generatedImage(r)"}}),
        ];
        let (calls, completed) = native_calls(&entries).unwrap();
        assert_eq!(calls, BTreeSet::from(["native".into()]));
        assert!(completed.is_empty());
    }
}
