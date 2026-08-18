//! Host encode path works without ONNX (inline redaction graph).

use reelforge::{
    GraphOutput, MaskSample, MaskTimeline, MediaAsset, MediaAssetId, MediaTime, NodeId,
    RENDER_GRAPH_VERSION, RegionRedaction, RenderGraph, RenderNode, RenderNodeKind,
};
use reelforge_host::{probe_has_audio, run_graph};
use std::path::Path;
use std::process::Command;

fn ffmpeg_ok() -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-version"])
        .output()
        .is_ok_and(|o| o.status.success())
}

fn gen_color_mp4(path: &Path) {
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=white:s=64x64:d=0.4:r=10",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            "-crf",
            "28",
        ])
        .arg(path)
        .status()
        .expect("ffmpeg");
    assert!(status.success());
}

fn gen_av_mp4(path: &Path) {
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=white:s=64x64:d=0.4:r=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=0.4",
            "-shortest",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            "-c:a",
            "aac",
            "-crf",
            "28",
        ])
        .arg(path)
        .status()
        .expect("ffmpeg");
    assert!(status.success());
}

fn tiny_graph(input: &Path, output: &Path) -> RenderGraph {
    let mut masks = MaskTimeline::new();
    masks.push(MaskSample::ellipse(
        MediaTime::new(0, 10).unwrap(),
        32.0,
        32.0,
        12.0,
    ));
    RenderGraph {
        version: RENDER_GRAPH_VERSION,
        assets: vec![MediaAsset {
            id: MediaAssetId("a".into()),
            uri: input.to_string_lossy().into_owned(),
            duration: None,
            role: Some("video".into()),
        }],
        nodes: vec![
            RenderNode {
                id: NodeId("src".into()),
                body: RenderNodeKind::Source {
                    asset: MediaAssetId("a".into()),
                },
                inputs: vec![],
            },
            RenderNode {
                id: NodeId("blur".into()),
                body: RenderNodeKind::Redaction {
                    redaction: RegionRedaction::gaussian(masks, 8.0),
                },
                inputs: vec![NodeId("src".into())],
            },
            RenderNode {
                id: NodeId("out".into()),
                body: RenderNodeKind::Output {
                    name: "main".into(),
                },
                inputs: vec![NodeId("blur".into())],
            },
        ],
        outputs: vec![GraphOutput {
            name: "main".into(),
            node: NodeId("out".into()),
            uri: Some(output.to_string_lossy().into_owned()),
        }],
    }
}

fn write_and_run(dir: &Path, input: &Path) -> String {
    let output = dir.join("out.mp4");
    let graph_path = dir.join("graph.json");
    let graph = tiny_graph(input, &output);
    std::fs::write(&graph_path, graph.to_json_pretty().unwrap()).unwrap();
    run_graph(&graph_path, None, Some(&output)).unwrap()
}

#[test]
fn run_graph_writes_mp4() {
    if !ffmpeg_ok() {
        eprintln!("skip: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("src.mp4");
    gen_color_mp4(&input);
    assert!(!probe_has_audio(&input).unwrap());
    let written = write_and_run(dir.path(), &input);
    assert!(Path::new(&written).is_file());
    assert!(Path::new(&written).metadata().unwrap().len() > 0);
    assert!(!probe_has_audio(Path::new(&written)).unwrap());
}

#[test]
fn run_graph_keeps_source_audio() {
    if !ffmpeg_ok() {
        eprintln!("skip: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("src.mp4");
    gen_av_mp4(&input);
    assert!(probe_has_audio(&input).unwrap(), "fixture must have audio");
    let written = write_and_run(dir.path(), &input);
    assert!(
        probe_has_audio(Path::new(&written)).unwrap(),
        "encode dropped companion audio"
    );
}
