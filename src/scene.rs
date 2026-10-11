//! Expose the existing editable scene compiler through the Host catalog.

use crate::{HostError, Result};
use reelforge::{MediaRefId, MediaTime, SceneCanvas, SceneDocument, SceneMedia, compile_scene};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    scene: Value,
    media: Vec<Media>,
    canvas: Canvas,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Media {
    id: String,
    uri: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Canvas {
    width: u32,
    height: u32,
    duration: MediaTime,
}

pub(crate) fn compile(args: &Value) -> Result<Value> {
    let request: Request = serde_json::from_value(args.clone())?;
    MediaTime::new(
        request.canvas.duration.ticks,
        request.canvas.duration.timescale,
    )
    .map_err(|err| HostError::Encode(err.to_string()))?;
    // Use the document loader so version migration and future-version refusal
    // have exactly the same behavior as the native project API.
    let scene = SceneDocument::from_json(&request.scene.to_string())
        .map_err(|err| HostError::Encode(err.to_string()))?;
    let media: Vec<_> = request
        .media
        .into_iter()
        .map(|item| SceneMedia {
            id: MediaRefId::new(item.id),
            uri: item.uri,
        })
        .collect();
    let compiled = compile_scene(
        &scene,
        &media,
        SceneCanvas {
            width: request.canvas.width,
            height: request.canvas.height,
            duration: request.canvas.duration,
        },
    )
    .map_err(|err| HostError::Encode(err.to_string()))?;
    Ok(json!({
        "scene_id": compiled.resolved.id,
        "duration": compiled.resolved.duration,
        "visible_parts": compiled.resolved.parts.len(),
        "graph": compiled.graph,
    }))
}
