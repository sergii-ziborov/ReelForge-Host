//! Capture session / project ingest — no ONNX. Uses lavfi only for concat.

use reelforge::{
    CaptureProject, CropRect, Gap, GraphRunOptions, JobState, JobStore, MediaRef, MediaRefId,
    MediaTime, Metadata, ProjectId, RenderGraph, RenderNodeKind, Retiming, Sequence, SequenceId,
    SourceRange, StageCache, TimelineClip, TimelineClipId, TimelineItem, TimelineTrack,
    TimelineTrackId, TrackKind, WriteControl, compile_project, run_render_job, submit_render_job,
};
use reelforge_host::{extract_rgb_frames, materialize_video, probe_video, resolve_capture_videos};
use std::path::{Path, PathBuf};
use std::process::Command;

fn ffmpeg_ok() -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-version"])
        .output()
        .is_ok_and(|o| o.status.success())
}

fn lavfi_clip(path: &Path, color: &str) {
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("color=c={color}:s=64x64:d=0.3:r=10"),
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

#[test]
fn materialize_capture_token_missing_is_error() {
    let dir = tempfile::tempdir().unwrap();
    let err = materialize_video(
        Path::new("capture:definitely-missing-session"),
        dir.path(),
        0.3,
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("Capture") || err.contains("session") || err.contains("not a Capture"),
        "{err}"
    );
}

#[test]
fn concat_two_project_clips() {
    if !ffmpeg_ok() {
        eprintln!("skip: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.mp4");
    let b = dir.path().join("b.mp4");
    lavfi_clip(&a, "red");
    lavfi_clip(&b, "blue");
    let project = dir.path().join("project.json");
    let json = format!(
        r#"{{
          "version": 1,
          "id": "prj_concat",
          "name": "t",
          "media": [
            {{ "id": "a", "uri": "{}", "role": "video" }},
            {{ "id": "b", "uri": "{}", "role": "video" }}
          ]
        }}"#,
        a.to_string_lossy().replace('\\', "/"),
        b.to_string_lossy().replace('\\', "/")
    );
    std::fs::write(&project, json).unwrap();
    assert_eq!(resolve_capture_videos(&project).unwrap().len(), 2);
    let work = dir.path().join("work");
    let out = materialize_video(&project, &work, 0.3).unwrap();
    assert_eq!(
        out.file_name().map(PathBuf::from),
        Some(PathBuf::from("capture.mp4"))
    );
    assert!(out.is_file());
    assert!(out.metadata().unwrap().len() > 0);
}

fn lavfi_split(path: &Path) {
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=red:s=64x64:d=1.0:r=10",
            "-f",
            "lavfi",
            "-i",
            "color=c=blue:s=64x64:d=1.0:r=10",
            "-filter_complex",
            "[0:v][1:v]concat=n=2:v=1:a=0",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            "-crf",
            "18",
            "-an",
        ])
        .arg(path)
        .status()
        .expect("ffmpeg");
    assert!(status.success());
}

fn sine_wav(path: &Path, hz: &str) {
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency={hz}:duration=0.15"),
            "-c:a",
            "pcm_s16le",
        ])
        .arg(path)
        .status()
        .expect("ffmpeg");
    assert!(status.success());
}

fn uri(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn library_ref(id: &str, path: &Path, role: &str) -> MediaRef {
    MediaRef {
        id: MediaRefId::new(id),
        uri: uri(path),
        duration: None,
        role: Some(role.into()),
    }
}

fn placed_clip(id: &str, media: &str, start: f64, dur: f64) -> TimelineClip {
    TimelineClip {
        id: TimelineClipId::new(id),
        media: MediaRefId::new(media),
        source: SourceRange::from_secs(start, dur).unwrap(),
        retiming: Retiming::Identity,
        transition_in: None,
        crop: None,
        scale_to: None,
        metadata: Metadata::default(),
    }
}

fn edited_project(picture: &Path, mic: &Path, system: &Path) -> CaptureProject {
    let mut project = CaptureProject::new(ProjectId::new("prj_edit"), "edit");
    project.media.push(library_ref("v", picture, "video"));
    project.media.push(library_ref("mic", mic, "audio"));
    project.media.push(library_ref("sys", system, "audio"));
    let mut picture_clip = placed_clip("picture", "v", 1.0, 1.0);
    picture_clip.retiming = Retiming::Speed { factor: 2.0 };
    picture_clip.crop = Some(CropRect {
        x: 0,
        y: 0,
        w: 32,
        h: 32,
    });
    let gap = TimelineItem::Gap(Gap {
        duration: MediaTime::from_secs(0.5, 1_000).unwrap(),
    });
    let mut video = TimelineTrack::new(TimelineTrackId::new("v0"), TrackKind::Video);
    video.items.push(gap.clone());
    video.items.push(TimelineItem::Clip(picture_clip));
    let mut mic_track = TimelineTrack::new(TimelineTrackId::new("a0"), TrackKind::Audio);
    mic_track
        .items
        .push(TimelineItem::Clip(placed_clip("mic", "mic", 0.0, 0.15)));
    let mut system_track = TimelineTrack::new(TimelineTrackId::new("a1"), TrackKind::Audio);
    system_track.items.push(gap);
    system_track
        .items
        .push(TimelineItem::Clip(placed_clip("system", "sys", 0.0, 0.15)));
    let mut sequence = Sequence::new(SequenceId::new("s"), "main");
    sequence.tracks.push(video);
    sequence.tracks.push(mic_track);
    sequence.tracks.push(system_track);
    project.sequences.push(sequence);
    project
}

#[test]
fn editorial_project_keeps_trim_crop_speed_and_both_audio_legs() {
    if !ffmpeg_ok() {
        eprintln!("skip: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let project = prepared_project(dir.path());

    let compiled = compile_project(&project).unwrap();
    let mix_inputs = compiled
        .graph
        .nodes
        .iter()
        .find_map(|node| match &node.body {
            RenderNodeKind::Op { operation, .. } if operation.as_str() == "rf.audio.mix" => {
                Some(node.inputs.len())
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(mix_inputs, 3, "picture plus both audio legs");

    let path = dir.path().join("project.json");
    std::fs::write(&path, project.to_json_pretty().unwrap()).unwrap();
    let work = dir.path().join("work");
    let out = materialize_video(&path, &work, 0.3).unwrap();
    assert_editorial_contract(&out, &dir.path().join("frames"));
}

#[test]
fn editorial_cache_and_resume_keep_the_same_contract() {
    if !ffmpeg_ok() {
        eprintln!("skip: ffmpeg not on PATH");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let project = prepared_project(dir.path());
    let cached = dir.path().join("cached.mp4");
    let graph = graph_for(&project, &cached);
    let store = JobStore::open(dir.path().join("jobs")).unwrap();
    let opts =
        GraphRunOptions::new().with_cache(StageCache::open(dir.path().join("cache")).unwrap());
    let mut job = submit_render_job(&store, &graph, &opts).unwrap();
    run_render_job(&store, &mut job, &graph, &WriteControl::default(), &opts).unwrap();
    assert_eq!(job.state, JobState::Done);
    assert_editorial_contract(&cached, &dir.path().join("frames-cache"));
    let bytes = std::fs::read(&cached).unwrap();
    std::fs::remove_file(&cached).unwrap();
    run_render_job(&store, &mut job, &graph, &WriteControl::default(), &opts).unwrap();
    assert_eq!(std::fs::read(&cached).unwrap(), bytes);

    let resumed = dir.path().join("resumed.mp4");
    let again = graph_for(&project, &resumed);
    let resume_store = JobStore::open(dir.path().join("resume-jobs")).unwrap();
    let plain = GraphRunOptions::new();
    let mut second = submit_render_job(&resume_store, &again, &plain).unwrap();
    run_render_job(
        &resume_store,
        &mut second,
        &again,
        &WriteControl::default(),
        &plain,
    )
    .unwrap();
    let stamps = stage_stamps(&resume_store.stages_dir(&second.id));
    assert!(!stamps.is_empty(), "resume needs persisted stages");
    std::fs::remove_file(&resumed).unwrap();
    run_render_job(
        &resume_store,
        &mut second,
        &again,
        &WriteControl::default(),
        &plain,
    )
    .unwrap();
    for (path, stamp) in &stamps {
        let now = path.metadata().unwrap().modified().unwrap();
        assert_eq!(
            &now,
            stamp,
            "skipped stage was rewritten: {}",
            path.display()
        );
    }
    assert_editorial_contract(&resumed, &dir.path().join("frames-resume"));
}

fn prepared_project(dir: &Path) -> CaptureProject {
    let picture = dir.join("picture.mp4");
    let mic = dir.join("mic.wav");
    let system = dir.join("system.wav");
    lavfi_split(&picture);
    sine_wav(&mic, "440");
    sine_wav(&system, "880");
    edited_project(&picture, &mic, &system)
}

fn graph_for(project: &CaptureProject, dest: &Path) -> RenderGraph {
    let mut graph = compile_project(project).unwrap().graph;
    let output = graph.outputs.first_mut().expect("compiled output");
    output.uri = Some(dest.to_string_lossy().into_owned());
    graph
}

fn assert_editorial_contract(path: &Path, frames_dir: &Path) {
    let info = probe_video(path).unwrap();
    assert_eq!((info.width, info.height), (32, 32));
    assert!(
        (0.85..=1.25).contains(&info.duration_secs),
        "0.5s gap plus trimmed 1s at 2x should be ~1s, got {}",
        info.duration_secs
    );
    assert!(info.has_audio);
    let frames = extract_rgb_frames(path, frames_dir, 10).unwrap();
    assert!(frames.len() >= 8, "frames {}", frames.len());
    let gap_px = center_rgb(&frames[0]);
    let blue_px = center_rgb(&frames[7]);
    assert!(
        gap_px.iter().all(|c| *c < 40),
        "gap stays black, got {gap_px:?}"
    );
    assert!(
        blue_px[2] > 150 && blue_px[0] < 80,
        "kept the blue half after the gap, got {blue_px:?}"
    );
    let bursts = audio_bursts(path);
    assert!(bursts.len() >= 2, "both legs should sound, got {bursts:?}");
    assert!(bursts[0] < 0.2, "mic starts at 0, got {bursts:?}");
    assert!(
        (0.35..=0.8).contains(&bursts[1]),
        "system starts after the 0.5s gap, got {bursts:?}"
    );
}

fn stage_stamps(dir: &Path) -> Vec<(PathBuf, std::time::SystemTime)> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).expect("stages") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("mp4") {
            let stamp = path
                .metadata()
                .expect("stage meta")
                .modified()
                .expect("mtime");
            found.push((path, stamp));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

fn center_rgb(frame: &reelforge_host::RgbFrame) -> [u8; 3] {
    let x = frame.width / 2;
    let y = frame.height / 2;
    let i = (y as usize * frame.width as usize + x as usize) * 3;
    [frame.rgb[i], frame.rgb[i + 1], frame.rgb[i + 2]]
}

fn audio_bursts(path: &Path) -> Vec<f64> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args(["-vn", "-ac", "1", "-ar", "16000", "-f", "f32le", "-"])
        .output()
        .expect("ffmpeg pcm");
    assert!(out.status.success(), "pcm decode failed");
    let mut bursts = Vec::new();
    let mut heard = false;
    let mut last = 0.0_f64;
    for (i, chunk) in out.stdout.chunks_exact(4).enumerate() {
        let sample = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let t = f64::from(u32::try_from(i).unwrap_or(u32::MAX)) / 16_000.0;
        if sample.abs() > 0.05 {
            if !heard || t - last > 0.2 {
                bursts.push(t);
            }
            heard = true;
            last = t;
        } else if t - last > 0.05 {
            heard = false;
        }
    }
    bursts
}
