//! Criterion benches for the Host orchestrator.
//!
//! CPU + optional FFmpeg encode. No ONNX inference (weights stay off-disk).
//!
//! ```text
//! cargo bench --bench host
//! ```

#![allow(missing_docs, clippy::all, clippy::pedantic)]

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use reelforge::{
    GraphOutput, MaskSample, MaskTimeline, MediaAsset, MediaAssetId, MediaTime, NodeId,
    RENDER_GRAPH_VERSION, RegionRedaction, RenderGraph, RenderNode, RenderNodeKind,
};
use reelforge_host::{
    HostService, PhotoHit, dispatch, handle_jsonrpc, photo_binding, photo_except_plan,
    require_accept, run_graph,
};
use reelforge_intelligence_core::{
    SelectorBinding, SemanticEdit, SemanticEditPlan, SubjectSelector, UncertaintyPolicy,
    rewrite_selectors,
};
use reelforge_intelligence_sightloom::encode_slm1_rle;
use sightloom::core::{AppearanceId, ClassId, SourceId, SubjectId, TrackId};
use sightloom_index::{
    Appearance, SourceEntry, SubjectProfile, TrackSample, VisionIndex, VisionIndexPackage,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tempfile::TempDir;

// Re-export helper so the bench can call rewrite through dispatch without a new public alias.
fn rewrite_via_dispatch(plan: SemanticEditPlan, bindings: &[SelectorBinding]) -> SemanticEditPlan {
    let mut svc = HostService::new();
    let args = serde_json::json!({ "plan": plan, "bindings": bindings });
    let out = dispatch(&mut svc, "rewrite_plan", &args).unwrap();
    serde_json::from_value(out).unwrap()
}

fn pick_plan(n_edits: usize) -> (SemanticEditPlan, Vec<SelectorBinding>) {
    let mut plan = SemanticEditPlan::new("scene.mp4");
    let mut bindings = Vec::with_capacity(n_edits);
    for i in 0..n_edits {
        let box_xyxy = [i as f32, 0.0, i as f32 + 10.0, 20.0];
        plan = plan.with_edit(SemanticEdit::BlurEveryoneExcept {
            allowed: SubjectSelector::FramePick {
                media: format!("photo-{i}.jpg"),
                frame_index: 0,
                box_xyxy,
            },
            uncertain_identity: Some(UncertaintyPolicy::Blur),
        });
        bindings.push(SelectorBinding {
            media: format!("photo-{i}.jpg"),
            frame_index: 0,
            box_xyxy,
            ids: vec![i as u64 + 1],
        });
    }
    (plan, bindings)
}

fn bench_rewrite(c: &mut Criterion) {
    let mut g = c.benchmark_group("rewrite");
    g.measurement_time(Duration::from_secs(3));
    g.sample_size(50);
    for n in [1usize, 8, 32] {
        let (plan, bindings) = pick_plan(n);
        g.bench_with_input(BenchmarkId::new("rewrite_selectors", n), &n, |b, _| {
            b.iter(|| {
                let out = rewrite_selectors(black_box(plan.clone()), black_box(&bindings)).unwrap();
                black_box(out)
            });
        });
        g.bench_with_input(BenchmarkId::new("mcp_rewrite_plan", n), &n, |b, _| {
            b.iter(|| {
                let out = rewrite_via_dispatch(black_box(plan.clone()), black_box(&bindings));
                black_box(out)
            });
        });
    }
    g.finish();
}

fn bench_mcp(c: &mut Criterion) {
    let mut g = c.benchmark_group("mcp");
    g.measurement_time(Duration::from_secs(2));
    g.sample_size(80);
    g.bench_function("tools_list", |b| {
        let mut svc = HostService::new();
        let raw = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        b.iter(|| {
            let resp = handle_jsonrpc(black_box(&mut svc), black_box(raw)).unwrap();
            black_box(resp)
        });
    });
    g.bench_function("require_accept_hit", |b| {
        let hits = vec![
            PhotoHit {
                subject_id: 2,
                score: 0.4,
                decision: "uncertain".into(),
            },
            PhotoHit {
                subject_id: 1,
                score: 0.91,
                decision: "accept".into(),
            },
        ];
        b.iter(|| black_box(require_accept(black_box(&hits)).unwrap()));
    });
    g.finish();
}

fn media_time(ticks: i64, ts: u32) -> sightloom::core::MediaTime {
    sightloom::core::MediaTime::new(ticks, ts).unwrap()
}

fn write_two_person_package(dir: &Path, frames: u64, fps: u32) {
    let mut index = VisionIndex::new("bench-cam");
    index.add_source(SourceEntry {
        source_id: 1,
        uri: "file://bench.mp4".into(),
        hash: None,
    });
    let alice = index.masks.insert(encode_slm1_rle(64, 64, &[0, 16, 48]));
    let bob = index.masks.insert(encode_slm1_rle(64, 64, &[16, 16, 32]));
    for frame in 0..frames {
        for (sid, left, mask) in [(1_u64, 4.0, alice.0), (2, 36.0, bob.0)] {
            index.push_track(TrackSample {
                sample_id: 0,
                supersedes: None,
                revision: 1,
                idempotency_key: 0,
                source_id: SourceId(1),
                frame_index: frame,
                pts: media_time(i64::try_from(frame).unwrap(), fps),
                track_id: TrackId(u32::try_from(sid).unwrap()),
                track_uid: None,
                subject_id: Some(SubjectId(sid)),
                class_id: Some(ClassId(0)),
                left,
                top: 4.0,
                right: left + 20.0,
                bottom: 60.0,
                confidence: 0.9,
                mask_ref: mask,
            });
        }
    }
    let last = media_time(i64::try_from(frames.saturating_sub(1)).unwrap(), fps);
    let start = media_time(0, fps);
    for (id, label) in [(1_u64, "alice"), (2, "bob")] {
        index.subjects.push(SubjectProfile {
            subject_id: SubjectId(id),
            label: Some(label.into()),
            appearance_count: 1,
            source_count: 1,
            total_duration_ns: 1_000_000_000,
            first_seen: Some(start),
            last_seen: Some(last),
            embedding: None,
        });
        index.appearances.push(Appearance {
            appearance_id: AppearanceId(id),
            subject_id: Some(SubjectId(id)),
            track_id: Some(TrackId(u32::try_from(id).unwrap())),
            source_id: SourceId(1),
            start,
            end: last,
            class_id: Some(ClassId(0)),
            peak_confidence: 0.9,
            evidence: None,
        });
    }
    VisionIndexPackage::save(&index, dir).unwrap();
}

fn bench_resolve_bridge(c: &mut Criterion) {
    let tmp = TempDir::new().unwrap();
    let pkg = tmp.path().join("vision_index");
    write_two_person_package(&pkg, 30, 10);
    let video = PathBuf::from("bench.mp4");
    let photo = PathBuf::from("alice.jpg");
    let output = tmp.path().join("out.mp4");
    let plan = photo_except_plan(&video, &photo, [0.0, 0.0, 64.0, 64.0], &output);
    let binding = photo_binding(&photo, [0.0, 0.0, 64.0, 64.0], 1);

    let mut g = c.benchmark_group("resolve_bridge");
    g.measurement_time(Duration::from_secs(4));
    g.sample_size(20);
    g.bench_function("photo_except_final", |b| {
        b.iter(|| {
            let work = tempfile::tempdir().unwrap();
            let out = reelforge_host::resolve_bridge(
                black_box(&pkg),
                black_box(plan.clone()),
                black_box(std::slice::from_ref(&binding)),
                Some(black_box(output.as_path())),
                work.path(),
                reelforge_intelligence_core::RedactionKind::Pixelate,
            )
            .unwrap();
            black_box(out)
        });
    });
    g.finish();
}

fn ffmpeg_ok() -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-version"])
        .output()
        .is_ok_and(|o| o.status.success())
}

fn gen_color_mp4(path: &Path, secs: &str) {
    gen_color_sized(path, secs, "320x180");
}

fn gen_color_sized(path: &Path, secs: &str, size: &str) {
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("color=c=white:s={size}:d={secs}:r=10"),
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

fn inline_graph(input: &Path, output: &Path) -> RenderGraph {
    let mut masks = MaskTimeline::new();
    masks.push(MaskSample::ellipse(
        MediaTime::new(0, 10).unwrap(),
        160.0,
        90.0,
        40.0,
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

fn bench_ffmpeg_host(c: &mut Criterion) {
    if !ffmpeg_ok() {
        eprintln!("skip ffmpeg benches: ffmpeg not on PATH");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let video = tmp.path().join("src.mp4");
    gen_color_mp4(&video, "1");

    let mut g = c.benchmark_group("ffmpeg");
    g.measurement_time(Duration::from_secs(6));
    g.sample_size(10);
    g.bench_function("probe_video", |b| {
        b.iter(|| {
            let info = reelforge_host::probe_video(black_box(&video)).unwrap();
            black_box(info)
        });
    });
    g.bench_function("extract_rgb_5fps", |b| {
        b.iter(|| {
            let frames_dir = tempfile::tempdir().unwrap();
            let frames =
                reelforge_host::extract_rgb_frames(black_box(&video), frames_dir.path(), 5)
                    .unwrap();
            black_box(frames.len())
        });
    });
    g.bench_function("run_graph_encode_1s", |b| {
        b.iter(|| {
            let work = tempfile::tempdir().unwrap();
            let out = work.path().join("out.mp4");
            let graph_path = work.path().join("graph.json");
            let graph = inline_graph(&video, &out);
            std::fs::write(&graph_path, graph.to_json_pretty().unwrap()).unwrap();
            let written =
                run_graph(black_box(&graph_path), None, Some(black_box(out.as_path()))).unwrap();
            black_box(written)
        });
    });
    g.finish();
}

fn bench_photo_plan(c: &mut Criterion) {
    let mut g = c.benchmark_group("plan");
    g.measurement_time(Duration::from_secs(2));
    g.sample_size(60);
    g.bench_function("photo_except_plan_bind", |b| {
        let video = PathBuf::from("scene.mp4");
        let photo = PathBuf::from("alice.jpg");
        let output = PathBuf::from("out.mp4");
        b.iter(|| {
            let plan = photo_except_plan(
                black_box(&video),
                black_box(&photo),
                black_box([0.0, 0.0, 864.0, 1152.0]),
                black_box(&output),
            );
            let bind = photo_binding(&photo, [0.0, 0.0, 864.0, 1152.0], 1);
            let out = rewrite_selectors(plan, std::slice::from_ref(&bind)).unwrap();
            black_box(out)
        });
    });
    g.finish();
}

fn bench_onnx_ingest(c: &mut Criterion) {
    let models = reelforge_host::resolve_models_dir(None);
    if reelforge_host::require_weights(&models).is_err() {
        eprintln!("skip onnx ingest: no sibling weights");
        return;
    }
    let scene = PathBuf::from("../ReelForge-Intelligence/target/real-video-e2e/scene.mp4");
    if !scene.is_file() {
        eprintln!("skip onnx ingest: no scene.mp4");
        return;
    }
    let tmp = TempDir::new().unwrap();
    let frames = reelforge_host::extract_rgb_frames(&scene, &tmp.path().join("frames"), 2).unwrap();
    let frames: Vec<_> = frames.into_iter().take(4).collect();
    if frames.is_empty() {
        return;
    }

    let mut g = c.benchmark_group("onnx");
    g.measurement_time(Duration::from_secs(12));
    g.sample_size(10);
    g.bench_function("ingest_4frames_720p", |b| {
        b.iter_custom(|iters| {
            let mut pipe = reelforge_host::open_pipeline("bench-onnx", &models).unwrap();
            reelforge_host::add_video_source(&mut pipe, &scene);
            let start = std::time::Instant::now();
            for _ in 0..iters {
                let _ = reelforge_host::ingest_frames(&mut pipe, black_box(&frames)).unwrap();
            }
            start.elapsed()
        });
    });
    g.finish();
}

fn bench_encode_720p(c: &mut Criterion) {
    if !ffmpeg_ok() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let video = tmp.path().join("src720.mp4");
    gen_color_sized(&video, "1", "1280x720");
    let mut g = c.benchmark_group("ffmpeg");
    g.measurement_time(Duration::from_secs(8));
    g.sample_size(10);
    g.bench_function("run_graph_encode_1s_720p", |b| {
        b.iter(|| {
            let work = tempfile::tempdir().unwrap();
            let out = work.path().join("out.mp4");
            let graph_path = work.path().join("graph.json");
            let graph = inline_graph(&video, &out);
            std::fs::write(&graph_path, graph.to_json_pretty().unwrap()).unwrap();
            let written = run_graph(&graph_path, None, Some(out.as_path())).unwrap();
            black_box(written)
        });
    });
    g.finish();
}

criterion_group!(
    benches,
    bench_rewrite,
    bench_mcp,
    bench_photo_plan,
    bench_resolve_bridge,
    bench_ffmpeg_host,
    bench_encode_720p,
    bench_onnx_ingest
);
criterion_main!(benches);
