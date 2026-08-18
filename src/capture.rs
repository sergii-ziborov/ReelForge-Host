//! Ingest Capture sessions / projects. Host does not grab the screen.
//!
//! Capture owns gdigrab + the session store. We only take **committed**
//! `media[].uri` values (or `SessionStore` segments). Never glob `segments/`.

use crate::error::{HostError, Result};
use reelforge_capture_schema::CaptureProject;
use reelforge_capture_store::SessionStore;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `capture:…` / `session:…` token for `--video`.
#[must_use]
pub fn is_capture_token(src: &str) -> bool {
    let t = src.trim();
    let lower = t.to_ascii_lowercase();
    lower.starts_with("capture:") || lower.starts_with("session:")
}

/// Directory looks like a Capture session (manifest / WAL), not a video file.
#[must_use]
pub fn is_capture_session_dir(path: &Path) -> bool {
    path.is_dir()
        && (path.join("manifest.json").is_file() || path.join("wal.jsonl").is_file())
}

/// JSON file that parses as `CaptureProject` v0/v1 with at least one video.
#[must_use]
pub fn is_capture_project_file(path: &Path) -> bool {
    videos_from_project_file(path).is_ok_and(|v| !v.is_empty())
}

/// Resolve `--video` token / path to a Capture session or project file.
#[must_use]
pub fn capture_input(src: &Path) -> Option<PathBuf> {
    let token = src.to_string_lossy();
    let t = token.trim();
    let rest = t
        .strip_prefix("capture:")
        .or_else(|| t.strip_prefix("CAPTURE:"))
        .or_else(|| t.strip_prefix("session:"))
        .or_else(|| t.strip_prefix("SESSION:"));
    if let Some(rest) = rest {
        let rest = rest.trim();
        if rest.is_empty() {
            return None;
        }
        let direct = PathBuf::from(rest);
        if direct.exists() {
            return Some(direct);
        }
        let under = PathBuf::from("sessions").join(rest);
        if under.exists() {
            return Some(under);
        }
        return Some(direct);
    }
    if is_capture_session_dir(src) || is_capture_project_file(src) {
        return Some(src.to_path_buf());
    }
    None
}

/// Committed video files, in record order. Loose `segments/` tails are ignored.
///
/// # Errors
///
/// Missing session/project, or no committed video.
pub fn resolve_capture_videos(src: &Path) -> Result<Vec<PathBuf>> {
    if is_capture_session_dir(src) {
        return videos_from_session(src);
    }
    if src.is_file() {
        return videos_from_project_file(src);
    }
    Err(HostError::message(format!(
        "not a Capture session or project: {}",
        src.display()
    )))
}

/// One file if a single segment; otherwise concat into `work_dir/capture.mp4`.
///
/// # Errors
///
/// Resolve or ffmpeg concat.
pub fn materialize_capture(src: &Path, work_dir: &Path) -> Result<PathBuf> {
    let videos = resolve_capture_videos(src)?;
    match videos.as_slice() {
        [] => Err(HostError::message(
            "no committed Capture video (finish the session; do not glob segments/)",
        )),
        [one] => {
            if !one.is_file() {
                return Err(HostError::message(format!(
                    "Capture media missing: {}",
                    one.display()
                )));
            }
            Ok(one.clone())
        }
        many => {
            std::fs::create_dir_all(work_dir)?;
            let dest = work_dir.join("capture.mp4");
            concat_videos(many, &dest)?;
            Ok(dest)
        }
    }
}

fn videos_from_session(dir: &Path) -> Result<Vec<PathBuf>> {
    let store = SessionStore::open(dir).map_err(|e| HostError::message(format!("capture session: {e}")))?;
    let segs = &store.manifest().segments;
    if segs.is_empty() {
        return Err(HostError::message(
            "no committed Capture segments (run a supervised capture; Host will not glob the tail)",
        ));
    }
    let mut out = Vec::with_capacity(segs.len());
    for seg in segs {
        let path = store.root().join(&seg.path);
        if !path.is_file() {
            return Err(HostError::message(format!(
                "committed segment missing: {}",
                path.display()
            )));
        }
        out.push(abs_path(&path));
    }
    Ok(out)
}

fn videos_from_project_file(path: &Path) -> Result<Vec<PathBuf>> {
    let text = std::fs::read_to_string(path)?;
    let project = CaptureProject::from_json(&text)
        .map_err(|e| HostError::message(format!("capture project: {e}")))?;
    let mut out = Vec::new();
    for media in &project.media {
        if media.role.as_deref() == Some("audio") {
            continue;
        }
        let p = PathBuf::from(&media.uri);
        if p.is_file() {
            out.push(abs_path(&p));
        }
    }
    if out.is_empty() {
        return Err(HostError::message(
            "CaptureProject has no readable video media[].uri",
        ));
    }
    Ok(out)
}

fn abs_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
    }
}

fn concat_videos(files: &[PathBuf], dest: &Path) -> Result<()> {
    let list = dest.with_file_name("capture.concat.txt");
    let mut body = String::new();
    for f in files {
        let p = f.to_string_lossy().replace('\\', "/").replace('\'', r"'\''");
        body.push_str("file '");
        body.push_str(&p);
        body.push_str("'\n");
    }
    std::fs::write(&list, body)?;
    let copy = run_concat(&list, dest, true)?;
    if copy && dest.is_file() && dest.metadata()?.len() > 0 {
        return Ok(());
    }
    let _ = std::fs::remove_file(dest);
    if run_concat(&list, dest, false)? && dest.is_file() && dest.metadata()?.len() > 0 {
        return Ok(());
    }
    Err(HostError::Ffmpeg(
        "ffmpeg concat of Capture segments failed".into(),
    ))
}

fn run_concat(list: &Path, dest: &Path, copy: bool) -> Result<bool> {
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-f",
        "concat",
        "-safe",
        "0",
        "-i",
    ])
    .arg(list);
    if copy {
        cmd.args(["-c", "copy"]);
    } else {
        cmd.args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-crf", "23"]);
    }
    let status = cmd
        .arg(dest)
        .status()
        .map_err(|e| HostError::Ffmpeg(format!("ffmpeg concat spawn: {e}")))?;
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reelforge_capture_core::{
        CaptureSpec, HZ_1K, MediaTime, SegmentId, SessionId, SessionMeta,
    };
    use reelforge_capture_store::{SegmentRecord, SessionStore};

    fn session_parent() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn commit_one(store: &mut SessionStore, rel: &str, bytes: &[u8]) {
        let path = store.root().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, bytes).unwrap();
        store
            .commit_segment(SegmentRecord {
                id: SegmentId(1),
                path: rel.into(),
                start: MediaTime::from_secs(0.0, HZ_1K).unwrap(),
                end: MediaTime::from_secs(1.0, HZ_1K).unwrap(),
            })
            .unwrap();
    }

    #[test]
    fn capture_tokens() {
        assert!(is_capture_token("capture:sessions/ses_1"));
        assert!(is_capture_token("SESSION:foo"));
        assert!(!is_capture_token("scene.mp4"));
        assert!(!is_capture_token("cam"));
    }

    #[test]
    fn session_uses_committed_not_tail() {
        let parent = session_parent();
        let mut store = SessionStore::create(
            parent.path(),
            SessionMeta {
                id: SessionId::new("ses_host"),
                name: "t".into(),
                spec: CaptureSpec::screen(),
                started_unix: None,
                duration: None,
            },
        )
        .unwrap();
        commit_one(&mut store, "segments/000001.mkv", b"good");
        std::fs::write(store.root().join("segments/000099.mkv"), b"tail").unwrap();

        let videos = resolve_capture_videos(store.root()).unwrap();
        assert_eq!(videos.len(), 1);
        assert!(videos[0].ends_with("000001.mkv"));
        assert!(!videos.iter().any(|p| p.ends_with("000099.mkv")));
    }

    #[test]
    fn empty_session_is_an_error() {
        let parent = session_parent();
        let store = SessionStore::create(
            parent.path(),
            SessionMeta {
                id: SessionId::new("ses_empty"),
                name: "t".into(),
                spec: CaptureSpec::screen(),
                started_unix: None,
                duration: None,
            },
        )
        .unwrap();
        let err = resolve_capture_videos(store.root()).unwrap_err().to_string();
        assert!(err.contains("committed"), "{err}");
    }

    #[test]
    fn project_json_skips_audio_role() {
        let dir = session_parent();
        let video = dir.path().join("clip.mp4");
        let audio = dir.path().join("leg.m4a");
        std::fs::write(&video, b"v").unwrap();
        std::fs::write(&audio, b"a").unwrap();
        let project = dir.path().join("project.json");
        let json = format!(
            r#"{{
              "version": 1,
              "id": "prj_1",
              "name": "t",
              "media": [
                {{ "id": "v1", "uri": "{}", "role": "video" }},
                {{ "id": "a1", "uri": "{}", "role": "audio" }}
              ]
            }}"#,
            video.to_string_lossy().replace('\\', "/"),
            audio.to_string_lossy().replace('\\', "/")
        );
        std::fs::write(&project, json).unwrap();
        let videos = resolve_capture_videos(&project).unwrap();
        assert_eq!(videos.len(), 1);
        assert!(videos[0].ends_with("clip.mp4"));
    }
}
