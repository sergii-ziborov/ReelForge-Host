//! CLI smoke. Full e2e needs ONNX weights (exit 2 without them).

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_reelforge-host")
}

#[test]
fn version_and_methods() {
    let out = Command::new(bin()).arg("version").output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("reelforge-host"), "{stdout}");

    let out = Command::new(bin()).arg("methods").output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("ingest_video"), "{stdout}");
    assert!(stdout.contains("privacy_except"), "{stdout}");
    assert!(stdout.contains("run_graph"), "{stdout}");
}

#[test]
fn privacy_except_without_weights_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("no-models");
    std::fs::create_dir_all(&models).unwrap();
    let video = dir.path().join("missing.mp4");
    let photo = dir.path().join("missing.jpg");
    let output = dir.path().join("out.mp4");

    let out = Command::new(bin())
        .args([
            "privacy-except",
            "--video",
            video.to_str().unwrap(),
            "--photo",
            photo.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
            "--work-dir",
            dir.path().join("work").to_str().unwrap(),
            "--models-dir",
            models.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let code = out.status.code().unwrap_or(0);
    assert_eq!(code, 2, "stderr={}", String::from_utf8_lossy(&out.stderr));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("person_detect") || err.contains("weights") || err.contains("not ready"),
        "{err}"
    );
}
