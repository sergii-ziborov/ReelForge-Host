//! Prompt-driven production lives in the shared Host, with persistent evidence.

mod assets;
mod graph;
mod model;
mod motion;
mod planner;
mod provider;
mod quality;
mod receipt;
mod render;
mod sound;
mod store;

use crate::{HostError, Result};
use model::{Create, State};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
};

pub(crate) fn valid_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 80
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(HostError::message(
            "movie/asset/part id must contain 1..80 letters, digits, hyphens or underscores",
        ));
    }
    Ok(())
}

fn directory(root: &Path, args: &Value) -> Result<PathBuf> {
    let id = args["id"]
        .as_str()
        .ok_or_else(|| HostError::message("movie id required"))?;
    valid_id(id)?;
    Ok(root.join(id))
}

pub(crate) fn dispatch(root: &Path, method: &str, args: &Value) -> Result<Value> {
    if method == "movie_create" {
        return create(root, args);
    }
    let dir = directory(root, args)?;
    if !dir.join("production.events").is_file() {
        return Err(HostError::message("unknown movie id"));
    }
    match method {
        "movie_inspect" => inspect(&dir),
        "movie_cancel" => {
            std::fs::write(dir.join("cancel.requested"), b"cancel")?;
            inspect(&dir)
        }
        "movie_resume" => {
            let lease = lease(&dir)?;
            if dir.join("cancel.requested").exists() {
                std::fs::remove_file(dir.join("cancel.requested"))?;
            }
            let mut state = store::load(&dir)?;
            if state.status == "completed" {
                drop(lease);
                return inspect(&dir);
            }
            state.status = "running".into();
            state.error = None;
            store::commit(&dir, &mut state, "production.resumed")?;
            start(dir.clone(), state, lease);
            inspect(&dir)
        }
        "movie_revise" => revise(&dir, args),
        "movie_rebuild" => rebuild(&dir, args),
        _ => Err(HostError::message("unknown production method")),
    }
}

fn create(root: &Path, args: &Value) -> Result<Value> {
    let request: Create = serde_json::from_value(args.clone())?;
    valid_id(&request.id)?;
    if request.prompt.trim().is_empty()
        || request.prompt.len() > 32_000
        || !request.duration.is_finite()
        || !(1.0..=30.0).contains(&request.duration)
        || !(16..=1920).contains(&request.width)
        || !(16..=1920).contains(&request.height)
        || !request.width.is_multiple_of(2)
        || !request.height.is_multiple_of(2)
        || !(1..=60).contains(&request.fps)
        || (request.duration * f64::from(request.fps)
            - (request.duration * f64::from(request.fps)).round())
        .abs()
            > 1e-6
        || !(1..=100).contains(&request.max_images)
        || request.images_already_used > request.max_images
    {
        return Err(HostError::message(
            "invalid prompt, canvas, duration, fps or image budget; duration must align with the frame clock",
        ));
    }
    if request.references.len() > 20 {
        return Err(HostError::message("too many reference files"));
    }
    for (id, path) in &request.references {
        valid_id(id)?;
        if !Path::new(path).is_absolute() || !Path::new(path).is_file() {
            return Err(HostError::message(
                "references require existing absolute files on Host",
            ));
        }
    }
    std::fs::create_dir_all(root)?;
    let dir = root.join(&request.id);
    std::fs::create_dir(&dir)?;
    let dir = dir.canonicalize()?;
    let mut state = State {
        images_used: request.images_already_used,
        request,
        revision: 0,
        status: "running".into(),
        stage: "plan".into(),
        error: None,
        plan: None,
        assets: std::collections::BTreeMap::default(),
        changed_nodes: vec![],
        invalidated_nodes: vec![],
        output: None,
        qc: None,
        graph: None,
        pending_generation: None,
    };
    store::commit(&dir, &mut state, "production.created")?;
    let lease = lease(&dir)?;
    start(dir.clone(), state, lease);
    inspect(&dir)
}

fn lease(dir: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("worker.lock"))?;
    file.try_lock()
        .map_err(|e| HostError::message(format!("movie already has an active producer: {e}")))?;
    Ok(file)
}

fn active(dir: &Path) -> bool {
    lease(dir).is_err()
}

fn inspect(dir: &Path) -> Result<Value> {
    let state = store::load(dir)?;
    let current = graph::build(&state)?;
    let graph = state.graph.as_ref().unwrap_or(&current);
    let external_changes = graph::changes(graph, &current);
    let running = active(dir);
    let bytes_current = state.output.as_ref().is_some_and(|p| {
        store::file_digest(Path::new(p))
            .is_ok_and(|hash| state.qc.as_ref().is_some_and(|q| q["sha256"] == hash))
    });
    let requires_rebuild =
        state.status == "completed" && (!external_changes.0.is_empty() || !bytes_current);
    Ok(
        json!({"project":state,"producer_active":running,"interrupted":!running && state.status=="running","requires_rebuild":requires_rebuild,"graph":graph,"external_changed_nodes":external_changes.0,"external_invalidated_nodes":external_changes.1,"project_dir":dir,"output_url":state.output.as_ref().filter(|_|!requires_rebuild && state.status=="completed").map(|_|format!("/movies/{}/video",state.request.id))}),
    )
}

fn revise(dir: &Path, args: &Value) -> Result<Value> {
    let lease = lease(dir)?;
    let mut state = store::load(dir)?;
    if state.pending_generation.is_some() {
        return Err(HostError::message(
            "resolve pending generation by resuming before revising",
        ));
    }
    if args["base_revision"].as_u64() != Some(state.revision) {
        return Err(HostError::message(
            "stale movie revision; inspect current graph before revising",
        ));
    }
    let prompt = args["prompt"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| HostError::message("revision prompt required"))?;
    if prompt.len() > 32_000 {
        return Err(HostError::message("revision prompt too long"));
    }
    state.request.prompt = prompt.into();
    state.status = "running".into();
    state.stage = "plan".into();
    state.error = None;
    state.output = None;
    state.qc = None;
    if dir.join("cancel.requested").exists() {
        std::fs::remove_file(dir.join("cancel.requested"))?;
    }
    store::commit(dir, &mut state, "production.revision_requested")?;
    start(dir.to_path_buf(), state, lease);
    inspect(dir)
}

fn rebuild(dir: &Path, args: &Value) -> Result<Value> {
    let lease = lease(dir)?;
    let mut state = store::load(dir)?;
    if args["base_revision"].as_u64() != Some(state.revision) {
        return Err(HostError::message("stale movie revision"));
    }
    let plan = state
        .plan
        .as_ref()
        .ok_or_else(|| HostError::message("saved plan missing"))?;
    planner::validate(plan, &state)?;
    let current = graph::build(&state)?;
    if let Some(old) = &state.graph {
        (state.changed_nodes, state.invalidated_nodes) = graph::changes(old, &current);
    }
    state.graph = Some(current);
    state.status = "running".into();
    state.stage = "assets".into();
    state.error = None;
    state.output = None;
    state.qc = None;
    if dir.join("cancel.requested").exists() {
        std::fs::remove_file(dir.join("cancel.requested"))?;
    }
    store::commit(dir, &mut state, "production.rebuild_requested")?;
    start(dir.to_path_buf(), state, lease);
    inspect(dir)
}

fn start(dir: PathBuf, mut state: State, lease: File) {
    std::thread::spawn(move || {
        let _lease = lease;
        let result = produce(&dir, &mut state);
        if let Err(error) = result {
            state.status = if dir.join("cancel.requested").exists() {
                "cancelled"
            } else {
                "failed"
            }
            .into();
            state.error = Some(error.to_string());
        } else {
            state.status = "completed".into();
            state.stage = "complete".into();
        }
        if let Err(error) = store::commit(&dir, &mut state, "production.finished") {
            eprintln!("production journal failed: {error}");
        }
    });
}

fn produce(dir: &Path, state: &mut State) -> Result<()> {
    let old = state
        .graph
        .clone()
        .map_or_else(|| graph::build(state), Ok)?;
    if state.stage == "plan" || state.plan.is_none() {
        let mut proposed = None;
        for attempt in 0..3 {
            if dir.join("cancel.requested").exists() {
                return Err(HostError::message("production cancelled"));
            }
            match planner::prepare(dir, state) {
                Ok(plan) => {
                    proposed = Some(plan);
                    state.error = None;
                    break;
                }
                Err(error) => {
                    state.error = Some(error.to_string());
                    store::commit(dir, state, "plan.rejected")?;
                    if attempt == 2 {
                        return Err(error);
                    }
                }
            }
        }
        let plan = proposed.ok_or_else(|| HostError::message("no validated creative plan"))?;
        state.plan = Some(plan);
        state.stage = "assets".into();
        let new = graph::build(state)?;
        (state.changed_nodes, state.invalidated_nodes) = graph::changes(&old, &new);
        store::write_json(&dir.join(format!("graph-r{}.json", state.revision)), &new)?;
        state.graph = Some(new);
        store::commit(dir, state, "plan.validated")?;
    }
    assets::prepare(dir, state)?;
    state.graph = Some(graph::build(state)?);
    state.stage = "motion".into();
    store::commit(dir, state, "motion.started")?;
    render::render(dir, state)
}

pub(crate) fn video(root: &Path, id: &str) -> Result<PathBuf> {
    valid_id(id)?;
    let state = store::load(&root.join(id))?;
    let current = graph::build(&state)?;
    if state
        .graph
        .as_ref()
        .is_some_and(|g| !graph::changes(g, &current).0.is_empty())
    {
        return Err(HostError::message(
            "movie inputs or render profile changed; rebuild required",
        ));
    }
    if state.status != "completed" {
        return Err(HostError::message(
            "movie has no qualified completed output",
        ));
    }
    let output = PathBuf::from(
        state
            .output
            .ok_or_else(|| HostError::message("movie output missing"))?,
    );
    if !output.is_file() {
        return Err(HostError::message("movie output file missing"));
    }
    if state.qc.as_ref().and_then(|q| q["sha256"].as_str())
        != Some(store::file_digest(&output)?.as_str())
    {
        return Err(HostError::message("qualified output bytes changed"));
    }
    Ok(output)
}

pub(crate) fn tools() -> Vec<Value> {
    let id = json!({"type":"string","pattern":"^[A-Za-z0-9_-]{1,80}$"});
    let tool = |name: &str, description: &str, schema: Value| json!({"name":name,"description":description,"inputSchema":schema});
    let mut tools = vec![tool(
        "movie_create",
        "Create a persistent movie from a prompt. Host plans via Codex CLI, generates missing assets within the per-video budget, animates separate objects and qualifies native ReelForge output. Returns immediately; inspect progress.",
        json!({"type":"object","additionalProperties":false,"required":["id","prompt","duration","width","height","fps","max_images"],"properties":{"id":id,"prompt":{"type":"string"},"duration":{"type":"number","minimum":1,"maximum":30},"width":{"type":"integer","minimum":16,"maximum":1920},"height":{"type":"integer","minimum":16,"maximum":1920},"fps":{"type":"integer","minimum":1,"maximum":60},"max_images":{"type":"integer","minimum":1,"maximum":100},"images_already_used":{"type":"integer","minimum":0},"references":{"type":"object","additionalProperties":{"type":"string"}}}}),
    )];
    for (name, description) in [
        (
            "movie_inspect",
            "Read saved production status, receipts and the actual dependency graph.",
        ),
        (
            "movie_resume",
            "Resume interrupted production using accepted exact bytes; reserved generation usage remains consumed.",
        ),
        (
            "movie_cancel",
            "Request cancellation of this production; provider outcomes and consumed usage are retained.",
        ),
    ] {
        tools.push(tool(name,description,json!({"type":"object","additionalProperties":false,"required":["id"],"properties":{"id":id}})));
    }
    tools.push(tool("movie_revise","Revise a movie by prompt against a current base revision. The saved graph identifies changed and affected nodes; matching verified assets are reused.",json!({"type":"object","additionalProperties":false,"required":["id","base_revision","prompt"],"properties":{"id":id,"base_revision":{"type":"integer","minimum":1},"prompt":{"type":"string"}}})));
    tools.push(tool("movie_rebuild","Rebuild the saved validated scene and verified resources with the current native renderer. No new planning call. Requires the current revision.",json!({"type":"object","additionalProperties":false,"required":["id","base_revision"],"properties":{"id":id,"base_revision":{"type":"integer","minimum":1}}})));
    tools
}

#[cfg(test)]
mod tests;
