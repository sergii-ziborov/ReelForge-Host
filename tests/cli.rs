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
fn serve_help_lists_http() {
    let out = Command::new(bin())
        .args(["serve", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("--http"), "{text}");
    assert!(text.contains("--token"), "{text}");
}

#[test]
fn lsp_help_is_a_host_mouth() {
    let out = Command::new(bin())
        .args(["lsp", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.to_lowercase().contains("language server") || text.contains("LSP"),
        "{text}"
    );
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

fn project_json(audio_extra: &str) -> String {
    format!(
        r#"{{
  "version": 1,
  "id": "p",
  "name": "av",
  "media": [{{ "id": "a", "uri": "a.mp4" }}],
  "sequences": [{{
    "id": "s",
    "name": "main",
    "tracks": [
      {{
        "id": "v0",
        "kind": "video",
        "items": [{{
          "kind": "clip",
          "id": "pic",
          "media": "a",
          "source": {{
            "start": {{ "ticks": 0, "timescale": 1000 }},
            "duration": {{ "ticks": 2000, "timescale": 1000 }}
          }}
        }}]
      }},
      {{
        "id": "a0",
        "kind": "audio",
        "items": [{{
          "kind": "clip",
          "id": "snd",
          "media": "a",
          "source": {{
            "start": {{ "ticks": 0, "timescale": 1000 }},
            "duration": {{ "ticks": 2000, "timescale": 1000 }}
          }}{audio_extra}
        }}]
      }}
    ]
  }}]
}}"#,
    )
}

#[test]
fn project_command_explains_audio_speed_and_refuses_picture_ops() {
    let dir = tempfile::tempdir().unwrap();
    let speed = dir.path().join("speed.json");
    std::fs::write(
        &speed,
        project_json(r#", "retiming": { "mode": "speed", "factor": 2.0 }"#),
    )
    .unwrap();
    let out = Command::new(bin())
        .args(["project", speed.to_str().unwrap()])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}\n{stdout}");
    assert!(stdout.contains("rf.transform.speed"), "{stdout}");
    assert!(stdout.contains("rf.audio.mix"), "{stdout}");

    let graph = Command::new(bin())
        .args(["project", "--graph", speed.to_str().unwrap()])
        .output()
        .unwrap();
    let graph_out = String::from_utf8_lossy(&graph.stdout);
    assert!(
        graph.status.success(),
        "{}",
        String::from_utf8_lossy(&graph.stderr)
    );
    assert!(graph_out.contains("\"rf.audio.mix\""), "{graph_out}");

    let freeze = dir.path().join("freeze.json");
    std::fs::write(
        &freeze,
        project_json(
            r#", "retiming": { "mode": "freeze", "at": { "ticks": 500, "timescale": 1000 }, "hold": { "ticks": 1000, "timescale": 1000 } }"#,
        ),
    )
    .unwrap();
    let refused = Command::new(bin())
        .args(["project", freeze.to_str().unwrap()])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success(), "{err}");
    assert!(
        err.contains("clip snd: freeze is a picture retime"),
        "{err}"
    );
    assert!(err.contains("audio track"), "{err}");
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
