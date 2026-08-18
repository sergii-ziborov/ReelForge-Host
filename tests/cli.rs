//! CLI smoke. Full e2e needs ONNX weights (exit 2 without them).

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_reelforge-host")
}

#[test]
fn resolve_models_dir_finds_sibling_sightloom_cache() {
    let dir = reelforge_host::resolve_models_dir(None);
    let ready = reelforge_host::require_weights(&dir);
    if ready.is_err() {
        eprintln!("skip: no sibling SightLoom/.sightloom-models on this checkout");
        return;
    }
    let paths = ready.unwrap();
    assert!(paths.detect.is_file());
    assert!(paths.reid.is_file());
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
fn unknown_style_fails_before_weights() {
    let out = Command::new(bin())
        .args([
            "privacy-except",
            "--video",
            "missing.mp4",
            "--photo",
            "missing.jpg",
            "--output",
            "out.mp4",
            "--style",
            "swirl",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("unknown redaction style") || err.contains("swirl"),
        "{err}"
    );
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
