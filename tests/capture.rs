//! Capture session / project ingest — no ONNX. Uses lavfi only for concat.

use reelforge::{
    CaptureProject, CropRect, MediaRef, MediaRefId, Metadata, ProjectId, RenderNodeKind, Retiming,
    Sequence, SequenceId, SourceRange, TimelineClip, TimelineClipId, TimelineItem, TimelineTrack,
    TimelineTrackId, TrackKind, compile_project,
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
            &format!("sine=frequency={hz}:duration=0.5"),
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
    let mut video = TimelineTrack::new(TimelineTrackId::new("v0"), TrackKind::Video);
    video.items.push(TimelineItem::Clip(picture_clip));
    let mut mic_track = TimelineTrack::new(TimelineTrackId::new("a0"), TrackKind::Audio);
    mic_track
        .items
        .push(TimelineItem::Clip(placed_clip("mic", "mic", 0.0, 0.5)));
    let mut system_track = TimelineTrack::new(TimelineTrackId::new("a1"), TrackKind::Audio);
    system_track
        .items
        .push(TimelineItem::Clip(placed_clip("system", "sys", 0.0, 0.5)));
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
    let picture = dir.path().join("picture.mp4");
    let mic = dir.path().join("mic.wav");
    let system = dir.path().join("system.wav");
    lavfi_split(&picture);
    sine_wav(&mic, "440");
    sine_wav(&system, "880");
    let project = edited_project(&picture, &mic, &system);

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
    let info = probe_video(&out).unwrap();
    assert_eq!((info.width, info.height), (32, 32));
    assert!(
        (0.35..=0.70).contains(&info.duration_secs),
        "trimmed 1s at 2x should be ~0.5s, got {}",
        info.duration_secs
    );
    assert!(info.has_audio);
    let frames = extract_rgb_frames(&out, &dir.path().join("frames"), 10).unwrap();
    let frame = &frames[0];
    let i = ((frame.height / 2) * frame.width + frame.width / 2) as usize * 3;
    let rgb = &frame.rgb[i..i + 3];
    assert!(
        rgb[2] > 150 && rgb[0] < 80,
        "kept the blue half, got {rgb:?}"
    );
}
