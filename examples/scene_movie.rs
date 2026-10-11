//! Render saved editable scenes through the shared Host compiler and encoder.
//!
//! `cargo run --release --example scene_movie -- movie.json`
//!
//! The JSON supplies `shots` (`compile_scene` requests), `titles` (optional native
//! `TextClip` rasters), `audio` (accepted file cues), optional `audio_gain`,
//! `fps`, and `output`.
//! Creative assets must already exist. Re-rendering makes no provider calls.
#![allow(clippy::print_stdout)]

use reelforge::{
    Duration, GraphOutput, MediaAsset, MediaAssetId, NodeId, OperationId, RenderGraph, RenderNode,
    RenderNodeKind, Rgba8, TextClip, TextClipOptions, Time, materialize_graph,
};
use reelforge_host::{HostService, dispatch};
use serde_json::{Value, json};
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let spec_path = std::env::args().nth(1).ok_or("movie JSON path required")?;
    let spec: Value = serde_json::from_str(&std::fs::read_to_string(spec_path)?)?;
    let output = spec["output"].as_str().ok_or("output required")?;
    let parent = Path::new(output).parent().ok_or("output parent required")?;
    std::fs::create_dir_all(parent)?;
    raster_titles(&spec)?;

    let mut service = HostService::new();
    let mut movie = RenderGraph::new();
    let mut shots = Vec::new();
    for (index, request) in spec["shots"]
        .as_array()
        .ok_or("shots required")?
        .iter()
        .enumerate()
    {
        let response = dispatch(&mut service, "compile_scene", request)?;
        std::fs::write(
            parent.join(format!("scene-{index}.compiled.json")),
            serde_json::to_vec_pretty(&response)?,
        )?;
        let mut graph: RenderGraph = serde_json::from_value(response["graph"].clone())?;
        // Decode and sample the actual source bindings before starting encode.
        let preview = materialize_graph(&graph)?;
        let at = preview.duration().as_secs() * 0.5;
        save_frame(
            &preview.frame_at(Time::from_secs(at))?,
            &parent.join(format!("scene-{index}.png")),
        )?;
        namespace(&mut graph, &format!("shot-{index}/"));
        shots.push(graph.outputs[0].node.clone());
        movie.assets.extend(graph.assets);
        movie.nodes.extend(graph.nodes);
    }
    let mut picture = op(
        &mut movie,
        "movie-cut",
        "rf.timeline.concat",
        json!({}),
        shots,
    );
    picture = op(
        &mut movie,
        "movie-in",
        "rf.transform.fade_in",
        json!({"duration": 0.18}),
        vec![picture],
    );
    picture = op(
        &mut movie,
        "movie-out",
        "rf.transform.fade_out",
        json!({"duration": 0.30}),
        vec![picture],
    );
    picture = attach_audio(&mut movie, picture, &spec)?;
    if let Some(gain) = spec["audio_gain"].as_f64() {
        picture = op(
            &mut movie,
            "movie-gain",
            "rf.audio.gain",
            json!({"factor": gain}),
            vec![picture],
        );
    }
    let fps = spec["fps"].as_f64().ok_or("fps required")?;
    picture = op(
        &mut movie,
        "movie-encode",
        "rf.encode.h264",
        json!({"fps": fps, "crf": 18}),
        vec![picture],
    );
    movie.outputs.push(GraphOutput {
        name: "movie".into(),
        node: picture,
        uri: Some(output.into()),
    });
    movie.validate()?;
    let graph_path = parent.join("movie.graph.json");
    std::fs::write(&graph_path, movie.to_json_pretty()?)?;
    let result = dispatch(&mut service, "run_graph", &json!({"graph": graph_path}))?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn attach_audio(
    movie: &mut RenderGraph,
    picture: NodeId,
    spec: &Value,
) -> Result<NodeId, Box<dyn std::error::Error>> {
    if let Some(audio) = spec["audio"].as_array().filter(|cues| !cues.is_empty()) {
        let mut inputs = vec![picture];
        let mut tracks = vec![json!({})];
        for (index, cue) in audio.iter().enumerate() {
            let id = format!("audio-{index}");
            movie.assets.push(MediaAsset {
                id: MediaAssetId(id.clone()),
                uri: cue["uri"].as_str().ok_or("audio cue uri required")?.into(),
                duration: None,
                role: Some("audio".into()),
            });
            let node = NodeId(id.clone());
            movie.nodes.push(RenderNode {
                id: node.clone(),
                body: RenderNodeKind::Source {
                    asset: MediaAssetId(id),
                },
                inputs: vec![],
            });
            inputs.push(node);
            tracks.push(json!({"gain": cue["gain"], "start": cue["start"]}));
        }
        return Ok(op(
            movie,
            "movie-sound",
            "rf.audio.mix",
            json!({"mode": "linear", "tracks": tracks}),
            inputs,
        ));
    }
    Ok(picture)
}

fn raster_titles(spec: &Value) -> Result<(), Box<dyn std::error::Error>> {
    let Some(titles) = spec["titles"].as_array() else {
        return Ok(());
    };
    for title in titles {
        let size = u32::try_from(title["size"].as_u64().ok_or("title size required")?)?;
        let mut options = TextClipOptions::new(
            title["text"].as_str().ok_or("title text required")?,
            size,
            Duration::from_secs(1.0),
        )
        .with_font_path(title["font"].as_str().ok_or("title font required")?)
        .with_padding(4);
        if let Some(color) = title["color"].as_array() {
            let components: Vec<_> = color
                .iter()
                .map(|v| {
                    v.as_u64()
                        .and_then(|v| u8::try_from(v).ok())
                        .ok_or("invalid title color")
                })
                .collect::<Result<_, _>>()?;
            if components.len() != 4 {
                return Err("title color requires RGBA".into());
            }
            options = options.with_color(Rgba8::new(
                components[0],
                components[1],
                components[2],
                components[3],
            ));
        }
        let clip = TextClip::new(&options)?;
        save_frame(
            clip.frame(),
            Path::new(title["uri"].as_str().ok_or("title uri required")?),
        )?;
    }
    Ok(())
}

fn save_frame(frame: &reelforge::Frame, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let size = frame.size();
    let color = match frame.format() {
        reelforge::FrameFormat::Rgb8 => image::ColorType::Rgb8,
        reelforge::FrameFormat::Rgba8 => image::ColorType::Rgba8,
    };
    image::save_buffer_with_format(
        path,
        frame.data(),
        size.width,
        size.height,
        color,
        image::ImageFormat::Png,
    )?;
    Ok(())
}

fn namespace(graph: &mut RenderGraph, prefix: &str) {
    for asset in &mut graph.assets {
        asset.id.0 = format!("{prefix}{}", asset.id.0);
    }
    for node in &mut graph.nodes {
        node.id.0 = format!("{prefix}{}", node.id.0);
        for input in &mut node.inputs {
            input.0 = format!("{prefix}{}", input.0);
        }
        if let RenderNodeKind::Source { asset } = &mut node.body {
            asset.0 = format!("{prefix}{}", asset.0);
        }
    }
    for output in &mut graph.outputs {
        output.node.0 = format!("{prefix}{}", output.node.0);
    }
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
