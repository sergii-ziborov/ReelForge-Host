//! Capture session / project ingest — no ONNX. Uses lavfi only for concat.

use reelforge_host::{materialize_video, resolve_capture_videos};
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
    assert_eq!(out.file_name().map(PathBuf::from), Some(PathBuf::from("capture.mp4")));
    assert!(out.is_file());
    assert!(out.metadata().unwrap().len() > 0);
}
