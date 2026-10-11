//! Movie-domain adapter over the existing Weavatrix Graph core.

use super::{model::State, store};
use crate::{HostError, Result};
use std::collections::BTreeSet;
use weavatrix_graph::{
    Confidence, Edge, EdgeKind, EvidenceKind, Graph, GraphBuilder, Node, NodeId, NodeKind,
    Provenance, bfs,
};

pub(super) fn build(state: &State) -> Result<Graph> {
    let mut builder = GraphBuilder::new();
    add(&mut builder, "prompt", "production.prompt", &state.request)?;
    add(&mut builder, "scene", "production.scene", &state.plan)?;
    add(
        &mut builder,
        "render",
        "production.render",
        &(
            state.request.width,
            state.request.height,
            state.request.fps,
            state.request.duration,
            "movie-center-pivot-v2",
        ),
    )?;
    add(&mut builder, "qc", "production.qc", &"action-and-output-v1")?;
    link(&mut builder, "scene", "render")?;
    link(&mut builder, "prompt", "scene")?;
    link(&mut builder, "render", "qc")?;
    for (id, path) in &state.request.references {
        let hash = available_digest(std::path::Path::new(path))?;
        add(
            &mut builder,
            &format!("reference/{id}"),
            "production.reference",
            &hash,
        )?;
    }
    if let Some(plan) = &state.plan {
        for asset in &plan.assets {
            let id = format!("asset/{}", asset.id);
            let bytes = state
                .assets
                .get(&asset.id)
                .map(|r| available_digest(std::path::Path::new(&r.path)))
                .transpose()?
                .flatten();
            add(&mut builder, &id, "production.asset", &(asset, bytes))?;
            for reference in &asset.reference_ids {
                let source = if state.request.references.contains_key(reference) {
                    format!("reference/{reference}")
                } else {
                    format!("asset/{reference}")
                };
                link(&mut builder, &source, &id)?;
            }
        }
        for layer in &plan.layers {
            let id = format!("layer/{}", layer.id);
            add(&mut builder, &id, "production.part", layer)?;
            link(&mut builder, &format!("asset/{}", layer.asset), &id)?;
            link(&mut builder, &id, "scene")?;
            if let Some(follow) = &layer.follow {
                link(&mut builder, &format!("layer/{}", follow.target), &id)?;
            }
        }
        for event in &plan.events {
            let id = format!("event/{}", event.id);
            add(&mut builder, &id, "production.event", event)?;
            link(&mut builder, &format!("layer/{}", event.actor), &id)?;
            link(&mut builder, &format!("layer/{}", event.target), &id)?;
            link(&mut builder, &id, "scene")?;
        }
    }
    builder.build().map_err(error)
}

fn available_digest(path: &std::path::Path) -> Result<Option<String>> {
    if path.is_file() {
        Ok(Some(store::file_digest(path)?))
    } else {
        Ok(None)
    }
}

pub(super) fn ordered_assets(state: &State) -> Result<Vec<super::model::Asset>> {
    let graph = build(state)?;
    let order = weavatrix_graph::topological_sort(graph.topology())
        .ok_or_else(|| HostError::message("cyclic production dependencies"))?;
    let Some(plan) = &state.plan else {
        return Ok(vec![]);
    };
    Ok(order
        .into_iter()
        .filter_map(|i| graph.node_at(i))
        .filter_map(|node| {
            node.id
                .as_str()
                .strip_prefix("asset/")
                .and_then(|id| plan.assets.iter().find(|a| a.id == id))
                .cloned()
        })
        .collect())
}

fn add(
    builder: &mut GraphBuilder,
    id: &str,
    kind: &str,
    recipe: &impl serde::Serialize,
) -> Result<()> {
    let digest = store::digest(&serde_json::to_vec(recipe)?);
    let node = Node::new(id, id, NodeKind::custom(kind).map_err(error)?)
        .map_err(error)?
        .with_attribute("recipe_sha256", digest);
    builder.add_node(node).map_err(error)?;
    Ok(())
}

fn link(builder: &mut GraphBuilder, source: &str, target: &str) -> Result<()> {
    let provenance = Provenance::new(
        "reelforge.movie-plan.v1",
        EvidenceKind::custom("declared_binding").map_err(error)?,
        Confidence::Exact,
    )
    .map_err(error)?;
    let edge = Edge::new(
        NodeId::new(source).map_err(error)?,
        NodeId::new(target).map_err(error)?,
        EdgeKind::custom("invalidates").map_err(error)?,
        provenance,
    );
    builder.add_edge(edge).map_err(error)?;
    Ok(())
}

pub(super) fn changes(old: &Graph, new: &Graph) -> (Vec<String>, Vec<String>) {
    let ids: BTreeSet<_> = old
        .nodes()
        .iter()
        .chain(new.nodes())
        .map(|n| n.id.as_str())
        .collect();
    let changed: Vec<_> = ids
        .into_iter()
        .filter(|id| old.node(id) != new.node(id))
        .map(str::to_owned)
        .collect();
    let mut invalidated = BTreeSet::new();
    for id in &changed {
        for graph in [old, new] {
            if let Some(start) = graph.node_index(id) {
                for index in bfs(graph.topology(), start) {
                    if let Some(node) = graph.node_at(index) {
                        invalidated.insert(node.id.to_string());
                    }
                }
            }
        }
    }
    (changed, invalidated.into_iter().collect())
}

fn error(e: impl std::fmt::Display) -> HostError {
    HostError::message(e.to_string())
}
