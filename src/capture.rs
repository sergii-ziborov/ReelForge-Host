//! Ingest Capture sessions / projects. Host does not grab the screen.
//!
//! Capture owns gdigrab + the session store. A session or a media-only project
//! contributes **committed** `media[].uri` values (or `SessionStore` segments).
//! Never glob `segments/`. A project with sequences is compiled and rendered,
//! so trim, crop, speed, and audio tracks survive.

use crate::error::{HostError, Result};
use reelforge_capture_schema::CaptureProject;
use reelforge_capture_store::SessionStore;
use std::path::{Component, Path, PathBuf};
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
    path.is_dir() && (path.join("manifest.json").is_file() || path.join("wal.jsonl").is_file())
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
/// A project file that has sequences is rendered with [`reelforge::compile_project`]
/// instead of concatenating library files. An unreadable or future schema does
/// not fall through to that concat. Relative media stays inside the project
/// directory; an import with no sequences still uses the media library.
///
/// # Errors
///
/// Resolve, compile, render, or ffmpeg concat.
pub fn materialize_capture(src: &Path, work_dir: &Path) -> Result<PathBuf> {
    if src.is_file() {
        let text = std::fs::read_to_string(src)?;
        let project = CaptureProject::from_json(&text).map_err(|err| {
            HostError::message(format!(
                "capture project refuses an unreadable document: {err}"
            ))
        })?;
        if !project.sequences.is_empty() {
            return render_editorial(&text, work_dir);
        }
        let videos = videos_from_parsed(&project, src)?;
        return materialize_videos(&videos, work_dir);
    }
    let videos = resolve_capture_videos(src)?;
    materialize_videos(&videos, work_dir)
}

fn materialize_videos(videos: &[PathBuf], work_dir: &Path) -> Result<PathBuf> {
    match videos {
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
    let store =
        SessionStore::open(dir).map_err(|e| HostError::message(format!("capture session: {e}")))?;
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
    videos_from_parsed(&project, path)
}

fn videos_from_parsed(project: &CaptureProject, project_file: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for media in &project.media {
        if media.role.as_deref() == Some("audio") {
            continue;
        }
        if let Some(path) = project_media_path(project_file, &media.uri)? {
            out.push(path);
        }
    }
    if out.is_empty() {
        return Err(HostError::message(
            "CaptureProject has no readable video media[].uri",
        ));
    }
    Ok(out)
}

/// A relative URI stays inside the project directory. An absolute file is kept.
fn project_media_path(project_file: &Path, uri: &str) -> Result<Option<PathBuf>> {
    let raw = Path::new(uri);
    if raw.is_absolute() {
        return Ok(raw.is_file().then(|| raw.to_path_buf()));
    }
    let Some(root) = project_file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Err(HostError::message(
            "capture project refuses a relative media path without a project directory",
        ));
    };
    let Some(lexical) = path_inside(root, raw) else {
        return Err(HostError::message(format!(
            "capture project refuses a media path outside the project: {uri}"
        )));
    };
    if !lexical.is_file() {
        return Ok(None);
    }
    let root_canon = std::fs::canonicalize(root).map_err(|err| {
        HostError::message(format!("capture project root {}: {err}", root.display()))
    })?;
    let file_canon = std::fs::canonicalize(&lexical).map_err(|err| {
        HostError::message(format!(
            "capture project media {}: {err}",
            lexical.display()
        ))
    })?;
    if !file_canon.starts_with(&root_canon) {
        return Err(HostError::message(format!(
            "capture project refuses a media path outside the project: {uri}"
        )));
    }
    Ok(Some(file_canon))
}

fn path_inside(root: &Path, relative: &Path) -> Option<PathBuf> {
    let mut parts: Vec<std::ffi::OsString> = root
        .components()
        .map(|component| component.as_os_str().to_os_string())
        .collect();
    let root_len = parts.len();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.len() <= root_len {
                    return None;
                }
                parts.pop();
            }
            Component::Normal(name) => parts.push(name.to_os_string()),
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(parts.into_iter().collect())
}

/// Render a project that already has sequences. A parse or compile failure
/// does not fall through to concatenating the media library.
fn render_editorial(text: &str, work_dir: &Path) -> Result<PathBuf> {
    let project = reelforge::CaptureProject::from_json(text).map_err(|err| {
        HostError::message(format!(
            "capture project refuses to concat an editorial document: {err}"
        ))
    })?;
    if project.sequences.is_empty() {
        return Err(HostError::message(
            "capture project refuses to render an import-only document as a timeline",
        ));
    }
    let compiled = reelforge::compile_project(&project)
        .map_err(|e| HostError::message(format!("compile CaptureProject: {e}")))?;
    std::fs::create_dir_all(work_dir)?;
    let dest = work_dir.join("capture.mp4");
    let mut graph = compiled.graph;
    let Some(output) = graph.outputs.first_mut() else {
        return Err(HostError::message("compiled CaptureProject has no output"));
    };
    output.uri = Some(dest.to_string_lossy().into_owned());
    reelforge::run_render_graph_with(
        &graph,
        &reelforge::WriteControl::default(),
        &reelforge::GraphRunOptions::default(),
    )
    .map_err(|e| HostError::message(format!("render CaptureProject: {e}")))?;
    if !dest.is_file() || dest.metadata()?.len() == 0 {
        return Err(HostError::message("compiled CaptureProject wrote no video"));
    }
    Ok(dest)
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
        let p = f
            .to_string_lossy()
            .replace('\\', "/")
            .replace('\'', r"'\''");
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
        cmd.args([
            "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-crf", "23",
        ]);
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
        let err = resolve_capture_videos(store.root())
            .unwrap_err()
            .to_string();
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

    #[test]
    fn relative_media_resolves_inside_the_project() {
        let dir = session_parent();
        let clips = dir.path().join("clips");
        std::fs::create_dir_all(&clips).unwrap();
        std::fs::write(clips.join("a.mp4"), b"v").unwrap();
        let project = dir.path().join("project.json");
        std::fs::write(
            &project,
            r#"{
              "version": 1,
              "id": "prj_rel",
              "name": "t",
              "media": [{ "id": "v1", "uri": "clips/a.mp4", "role": "video" }]
            }"#,
        )
        .unwrap();
        let videos = resolve_capture_videos(&project).unwrap();
        assert_eq!(videos.len(), 1);
        assert!(videos[0].starts_with(std::fs::canonicalize(dir.path()).unwrap()));
        assert!(videos[0].ends_with("a.mp4"));
    }

    #[test]
    fn relative_media_outside_the_project_is_refused() {
        let dir = session_parent();
        let outside = dir.path().join("outside.mp4");
        std::fs::write(&outside, b"secret").unwrap();
        let nested = dir.path().join("proj");
        std::fs::create_dir_all(&nested).unwrap();
        let project = nested.join("project.json");
        std::fs::write(
            &project,
            r#"{
              "version": 1,
              "id": "prj_out",
              "name": "t",
              "media": [{ "id": "v1", "uri": "../outside.mp4", "role": "video" }]
            }"#,
        )
        .unwrap();
        let err = resolve_capture_videos(&project).unwrap_err().to_string();
        assert!(err.contains("refuses"), "{err}");
        assert!(err.contains("outside"), "{err}");
    }

    #[test]
    fn future_project_schema_does_not_concat_media() {
        let dir = session_parent();
        let video = dir.path().join("clip.mp4");
        std::fs::write(&video, b"v").unwrap();
        let project = dir.path().join("project.json");
        let uri = video.to_string_lossy().replace('\\', "/");
        std::fs::write(
            &project,
            format!(
                r#"{{
                  "version": 99,
                  "id": "prj_new",
                  "name": "t",
                  "media": [{{ "id": "v1", "uri": "{uri}", "role": "video" }}],
                  "sequences": [{{ "id": "seq", "name": "main" }}]
                }}"#
            ),
        )
        .unwrap();
        let work = dir.path().join("work");
        let err = materialize_capture(&project, &work)
            .unwrap_err()
            .to_string();
        assert!(err.contains("refuses"), "{err}");
        assert!(err.contains("newer"), "{err}");
        assert!(!work.join("capture.mp4").exists());
    }

    #[test]
    fn editorial_without_clips_does_not_return_the_library() {
        let dir = session_parent();
        let video = dir.path().join("clip.mp4");
        std::fs::write(&video, b"library").unwrap();
        let project = dir.path().join("project.json");
        let uri = video.to_string_lossy().replace('\\', "/");
        std::fs::write(
            &project,
            format!(
                r#"{{
                  "version": 1,
                  "id": "prj_edit",
                  "name": "t",
                  "media": [{{ "id": "v1", "uri": "{uri}", "role": "video" }}],
                  "sequences": [{{ "id": "seq", "name": "main" }}]
                }}"#
            ),
        )
        .unwrap();
        let work = dir.path().join("work");
        let err = materialize_capture(&project, &work)
            .unwrap_err()
            .to_string();
        assert!(err.contains("compile CaptureProject"), "{err}");
        assert!(!work.join("capture.mp4").exists());
    }
}
