//! Use the same native scene compiler, operations and streaming encoder.

use super::{model::State, motion, store};
use crate::{HostError, Result};
use reelforge::{
    GraphOutput, MediaAsset, MediaAssetId, MediaRefId, MediaTime, NodeId, OperationId, RenderGraph,
    RenderNode, RenderNodeKind, SceneCanvas, SceneMedia, compile_scene,
};
use serde_json::{Value, json};
use std::path::Path;

pub(super) fn render(dir: &Path, state: &mut State) -> Result<()> {
    let actions = motion::action_report(state)?;
    let scene = motion::compile(state)?;
    let revision = dir.join(format!("revision-{}", state.revision));
    std::fs::create_dir_all(&revision)?;
    store::write_json(&revision.join("scene.json"), &scene)?;
    let media: Vec<_> = state
        .assets
        .iter()
        .map(|(id, a)| SceneMedia {
            id: MediaRefId::new(id),
            uri: a.path.clone(),
        })
        .collect();
    let compiled = compile_scene(
        &scene,
        &media,
        SceneCanvas {
            width: state.request.width,
            height: state.request.height,
            duration: MediaTime::new(
                (state.request.duration * 1_000_000.0).round() as i64,
                1_000_000,
            )
            .map_err(|e| HostError::message(e.to_string()))?,
        },
    )
    .map_err(|e| HostError::message(e.to_string()))?;
    let mut graph = compiled.graph;
    let picture = graph.outputs.remove(0).node;
    let score = revision.join("score.wav");
    super::sound::score(state, &score)?;
    graph.assets.push(MediaAsset {
        id: MediaAssetId("score".into()),
        uri: score.to_string_lossy().into_owned(),
        duration: None,
        role: Some("audio".into()),
    });
    graph.nodes.push(RenderNode {
        id: NodeId("score".into()),
        body: RenderNodeKind::Source {
            asset: MediaAssetId("score".into()),
        },
        inputs: vec![],
    });
    let mut output = op(
        &mut graph,
        "sound",
        "rf.audio.mix",
        json!({"mode":"linear","tracks":[{},{}]}),
        vec![picture, NodeId("score".into())],
    );
    output = op(
        &mut graph,
        "intro",
        "rf.transform.fade_in",
        json!({"duration":0.12}),
        vec![output],
    );
    output = op(
        &mut graph,
        "ending",
        "rf.transform.fade_out",
        json!({"duration":0.25}),
        vec![output],
    );
    output = op(
        &mut graph,
        "encode",
        "rf.encode.h264",
        json!({"fps":state.request.fps,"crf":18}),
        vec![output],
    );
    let movie = revision.join("movie.mp4");
    graph.outputs.push(GraphOutput {
        name: "movie".into(),
        node: output,
        uri: Some(movie.to_string_lossy().into_owned()),
    });
    let graph_path = revision.join("render.graph.json");
    store::write_json(&graph_path, &graph)?;
    state.stage = "encode".into();
    store::commit(dir, state, "render.started")?;
    encode(dir, &graph_path)?;
    state.stage = "quality".into();
    store::commit(dir, state, "render.written")?;
    let qc = super::quality::inspect(state, &movie, actions)?;
    store::write_json(&revision.join("quality.json"), &qc)?;
    state.output = Some(movie.to_string_lossy().into_owned());
    state.qc = Some(qc);
    Ok(())
}

fn op(
    graph: &mut RenderGraph,
    id: &str,
    operation: &str,
    params: Value,
    inputs: Vec<NodeId>,
) -> NodeId {
    let id = NodeId(id.into());
    graph.nodes.push(RenderNode {
        id: id.clone(),
        body: RenderNodeKind::Op {
            operation: OperationId::new(operation),
            params,
        },
        inputs,
    });
    id
}

fn encode(dir: &Path, graph_path: &Path) -> Result<()> {
    let cancel = reelforge::CancelToken::new();
    let watched = dir.to_path_buf();
    let callback_cancel = cancel.clone();
    let control = reelforge::WriteControl::new()
        .with_cancel(cancel)
        .with_progress(move |_| {
            if watched.join("cancel.requested").exists() {
                callback_cancel.cancel();
            }
        });
    crate::encode::run_graph_controlled(graph_path, None, None, &control)?;
    if dir.join("cancel.requested").exists() {
        return Err(HostError::message(
            "production cancelled before qualification",
        ));
    }
    Ok(())
}
