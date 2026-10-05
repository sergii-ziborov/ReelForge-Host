//! Host ffmpeg decode: probe + RGB frames. No libav.

use crate::error::{HostError, Result};
use serde::Deserialize;
use std::path::Path;
use std::process::Command;

/// One decoded RGB8 frame with media time.
#[derive(Debug, Clone)]
pub struct RgbFrame {
    /// Zero-based extracted-frame index.
    pub index: u64,
    /// Presentation ticks at [`Self::timescale`].
    pub ticks: i64,
    /// Timescale (sample fps).
    pub timescale: u32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// Packed RGB8.
    pub rgb: Vec<u8>,
}

/// Video probe summary.
#[derive(Debug, Clone)]
pub struct VideoInfo {
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// Container duration seconds when known.
    pub duration_secs: f64,
    /// True when ffprobe sees an audio stream.
    pub has_audio: bool,
}

#[derive(Debug, Deserialize)]
struct ProbeJson {
    streams: Option<Vec<ProbeStream>>,
    format: Option<ProbeFormat>,
}

#[derive(Debug, Deserialize)]
struct ProbeStream {
    width: Option<u32>,
    height: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
}

/// Probe width / height / duration via ffprobe.
///
/// # Errors
///
/// Missing ffprobe or unreadable file.
pub fn probe_video(path: &Path) -> Result<VideoInfo> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-show_entries",
            "format=duration",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .map_err(|e| HostError::Ffmpeg(format!("ffprobe spawn: {e}")))?;
    if !out.status.success() {
        return Err(HostError::Ffmpeg(format!(
            "ffprobe {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    let parsed: ProbeJson = serde_json::from_slice(&out.stdout)?;
    let stream = parsed
        .streams
        .and_then(|s| s.into_iter().next())
        .ok_or_else(|| HostError::Ffmpeg("ffprobe: no video stream".into()))?;
    let width = stream
        .width
        .ok_or_else(|| HostError::Ffmpeg("ffprobe: no width".into()))?;
    let height = stream
        .height
        .ok_or_else(|| HostError::Ffmpeg("ffprobe: no height".into()))?;
    let duration_secs = parsed
        .format
        .and_then(|f| f.duration)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);
    Ok(VideoInfo {
        width,
        height,
        duration_secs,
        has_audio: probe_has_audio(path)?,
    })
}

/// True when `path` has at least one audio stream.
///
/// # Errors
///
/// ffprobe spawn failure. Missing audio is `Ok(false)`, not an error.
pub fn probe_has_audio(path: &Path) -> Result<bool> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .map_err(|e| HostError::Ffmpeg(format!("ffprobe audio spawn: {e}")))?;
    if !out.status.success() {
        return Ok(false);
    }
    Ok(!String::from_utf8_lossy(&out.stdout).trim().is_empty())
}

/// Frames kept when a caller passes `0`.
///
/// `0` is this cap, not the whole file. A longer source is not decoded into RGB.
pub const ANALYSIS_MAX_FRAMES: u32 = 300;

/// `0` selects [`ANALYSIS_MAX_FRAMES`]. Any other value is the caller's cap.
#[must_use]
pub const fn applied_frame_cap(max_frames: u32) -> u32 {
    if max_frames == 0 {
        ANALYSIS_MAX_FRAMES
    } else {
        max_frames
    }
}

/// Extract RGB frames at `sample_fps`, stopping at [`ANALYSIS_MAX_FRAMES`].
///
/// # Errors
///
/// ffmpeg failure or unreadable PNGs.
pub fn extract_rgb_frames(video: &Path, out_dir: &Path, sample_fps: u32) -> Result<Vec<RgbFrame>> {
    extract_rgb_frames_limited(video, out_dir, sample_fps, ANALYSIS_MAX_FRAMES)
}

/// Extract at most `max_frames` RGB frames.
///
/// ffmpeg stops at the cap, then only those PNGs are decoded. A previous
/// `frame_*.png` tail is removed first. `frames.json` records `max_frames`
/// and `hit_cap` when the source is longer than the cap. `max_frames == 0`
/// is refused before any file is written.
///
/// # Errors
///
/// A zero cap, ffmpeg failure, or unreadable PNGs.
pub fn extract_rgb_frames_limited(
    video: &Path,
    out_dir: &Path,
    sample_fps: u32,
    max_frames: u32,
) -> Result<Vec<RgbFrame>> {
    if max_frames == 0 {
        return Err(HostError::Ffmpeg(
            "extract refuses an unbounded frame list".into(),
        ));
    }
    if sample_fps == 0 {
        return Err(HostError::Ffmpeg("sample_fps must be > 0".into()));
    }
    let hit_cap =
        expected_samples(video, sample_fps).is_some_and(|count| count > u64::from(max_frames));
    std::fs::create_dir_all(out_dir)?;
    clear_frame_pngs(out_dir)?;
    let pattern = out_dir.join("frame_%06d.png");
    let fps = format!("fps={sample_fps}");
    let limit = max_frames.to_string();
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(video)
        .args(["-vf", &fps, "-start_number", "0", "-frames:v", &limit])
        .arg(&pattern)
        .status()
        .map_err(|e| HostError::Ffmpeg(format!("ffmpeg spawn: {e}")))?;
    if !status.success() {
        return Err(HostError::Ffmpeg(format!(
            "ffmpeg extract frames failed ({status})"
        )));
    }

    let mut frames = Vec::new();
    let mut index = 0_u64;
    let cap = u64::from(max_frames);
    while index < cap {
        let path = out_dir.join(format!("frame_{index:06}.png"));
        if !path.is_file() {
            break;
        }
        let bytes = std::fs::read(&path)?;
        let decoded = sightloom_host::decode_encoded_rgb(&bytes)
            .map_err(|e| HostError::Ffmpeg(format!("decode {}: {e}", path.display())))?;
        frames.push(RgbFrame {
            index,
            ticks: i64::try_from(index).unwrap_or(0),
            timescale: sample_fps,
            width: decoded.width,
            height: decoded.height,
            rgb: decoded.rgb,
        });
        index += 1;
    }
    let mut extra = index;
    loop {
        let path = out_dir.join(format!("frame_{extra:06}.png"));
        if !path.is_file() {
            break;
        }
        std::fs::remove_file(&path)?;
        extra += 1;
    }
    if frames.is_empty() {
        return Err(HostError::Ffmpeg(
            "ffmpeg wrote no frames (empty or unreadable video)".into(),
        ));
    }
    let names = frame_png_names(index);
    let manifest = serde_json::json!({
        "frames": names,
        "max_frames": max_frames,
        "hit_cap": hit_cap,
    });
    std::fs::write(out_dir.join("frames.json"), manifest.to_string())
        .map_err(|e| HostError::Ffmpeg(format!("frame manifest: {e}")))?;
    Ok(frames)
}

#[allow(clippy::cast_sign_loss)]
fn expected_samples(video: &Path, sample_fps: u32) -> Option<u64> {
    let info = probe_video(video).ok()?;
    if !(info.duration_secs.is_finite() && info.duration_secs > 0.0) {
        return None;
    }
    let count = (info.duration_secs * f64::from(sample_fps)).round();
    if count < 1.0 {
        Some(1)
    } else {
        Some(count as u64)
    }
}

/// Delete leftover `frame_*.png` so a shorter job cannot see the previous tail.
pub fn clear_frame_pngs(out_dir: &Path) -> Result<()> {
    if !out_dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(out_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("frame_") && name.ends_with(".png") {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// Names a successful extract of `count` frames writes, in order.
#[must_use]
pub fn frame_png_names(count: u64) -> Vec<String> {
    (0..count)
        .map(|index| format!("frame_{index:06}.png"))
        .collect()
}

/// True when `--video cam` / `live` should grab a camera.
#[must_use]
pub fn is_live_token(src: &str) -> bool {
    matches!(
        src.trim().to_ascii_lowercase().as_str(),
        "cam" | "camera" | "live"
    )
}

/// True when `--video lavfi:...` is a synthetic ffmpeg source.
#[must_use]
pub fn is_lavfi_token(src: &str) -> bool {
    src.trim().starts_with("lavfi:")
}

/// Grab `secs` of live/synthetic video into `dest` (mp4).
///
/// * `cam` / `live` / `camera` — first DirectShow video device (Windows)
/// * `lavfi:...` — ffmpeg lavfi graph (tests / no webcam)
///
/// # Errors
///
/// No camera, ffmpeg failure.
pub fn grab_source(src: &str, dest: &Path, secs: f64) -> Result<()> {
    let secs = secs.max(0.2);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if is_lavfi_token(src) {
        let filter = src.trim()[6..].trim();
        if filter.is_empty() {
            return Err(HostError::Ffmpeg("lavfi: empty filter".into()));
        }
        return run_ffmpeg_grab(
            &[
                "-f",
                "lavfi",
                "-i",
                filter,
                "-t",
                &format!("{secs:.2}"),
                "-pix_fmt",
                "yuv420p",
                "-c:v",
                "libx264",
                "-crf",
                "23",
                "-an",
                "-y",
            ],
            dest,
        );
    }
    if !is_live_token(src) {
        return Err(HostError::Ffmpeg(format!(
            "not a live source: {src} (use cam / live / lavfi:...)"
        )));
    }
    let device = first_dshow_video()?;
    run_ffmpeg_grab(
        &[
            "-f",
            "dshow",
            "-rtbufsize",
            "100M",
            "-i",
            &format!("video={device}"),
            "-t",
            &format!("{secs:.2}"),
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "libx264",
            "-crf",
            "23",
            "-an",
            "-y",
        ],
        dest,
    )
}

/// Resolve `cam` / `lavfi:` to a real file; files pass through.
///
/// # Errors
///
/// Grab / missing file.
pub fn materialize_video(
    src: &Path,
    work_dir: &Path,
    live_secs: f64,
) -> Result<std::path::PathBuf> {
    let token = src.to_string_lossy();
    if is_live_token(&token) || is_lavfi_token(&token) {
        let dest = work_dir.join("live.mp4");
        grab_source(&token, &dest, live_secs)?;
        return Ok(dest);
    }
    if let Some(cap) = crate::capture::capture_input(src) {
        return crate::capture::materialize_capture(&cap, work_dir);
    }
    if !src.is_file() {
        return Err(HostError::Ffmpeg(format!(
            "video not found: {} (or use cam / lavfi:… / Capture session dir / project.json)",
            src.display()
        )));
    }
    Ok(src.to_path_buf())
}

fn run_ffmpeg_grab(args: &[&str], dest: &Path) -> Result<()> {
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error"])
        .args(args)
        .arg(dest)
        .status()
        .map_err(|e| HostError::Ffmpeg(format!("ffmpeg grab spawn: {e}")))?;
    if !status.success() {
        return Err(HostError::Ffmpeg(format!("ffmpeg grab failed ({status})")));
    }
    if !dest.is_file() {
        return Err(HostError::Ffmpeg("ffmpeg grab wrote no file".into()));
    }
    Ok(())
}

fn first_dshow_video() -> Result<String> {
    let out = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-list_devices",
            "true",
            "-f",
            "dshow",
            "-i",
            "dummy",
        ])
        .output()
        .map_err(|e| HostError::Ffmpeg(format!("dshow list spawn: {e}")))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    let mut in_video = false;
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.contains("directshow video") {
            in_video = true;
            continue;
        }
        if lower.contains("directshow audio") {
            in_video = false;
            continue;
        }
        if in_video && let Some(name) = quoted_device(line) {
            return Ok(name);
        }
    }
    Err(HostError::Ffmpeg(
        "no DirectShow video device (plug in a camera, or use lavfi:testsrc=size=640x360:rate=10)"
            .into(),
    ))
}

fn quoted_device(line: &str) -> Option<String> {
    let start = line.find('"')?;
    let rest = &line[start + 1..];
    let end = rest.find('"')?;
    let name = rest[..end].trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_frame_pngs_drops_a_stale_tail() {
        let dir = std::env::temp_dir().join(format!("rf-host-frames-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("frame_000000.png"), b"red").unwrap();
        std::fs::write(dir.join("frame_000009.png"), b"old").unwrap();
        std::fs::write(dir.join("keep.txt"), b"keep").unwrap();
        clear_frame_pngs(&dir).unwrap();
        assert!(!dir.join("frame_000000.png").exists());
        assert!(!dir.join("frame_000009.png").exists());
        assert!(dir.join("keep.txt").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn frame_manifest_names_stop_at_the_new_count() {
        let names = frame_png_names(2);
        assert_eq!(
            names,
            vec!["frame_000000.png".to_string(), "frame_000001.png".into()]
        );
        assert!(!names.iter().any(|name| name == "frame_000002.png"));
    }

    #[test]
    fn extract_refuses_an_unbounded_frame_list() {
        let dir = std::env::temp_dir().join(format!("rf-host-nobound-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let err = extract_rgb_frames_limited(Path::new("no-such.mp4"), &dir, 5, 0).unwrap_err();
        assert!(err.to_string().contains("refuses"), "{err}");
        assert!(!dir.exists());
    }

    #[test]
    fn extract_stops_at_the_cap_and_clears_the_tail() {
        if !ffmpeg_present() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("rf-host-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("src.mp4");
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=16x16:r=10:d=2",
                "-frames:v",
                "20",
            ])
            .arg(&video)
            .status()
            .unwrap();
        assert!(status.success());
        let frames_dir = dir.join("frames");
        std::fs::create_dir_all(&frames_dir).unwrap();
        std::fs::write(frames_dir.join("frame_000010.png"), b"stale").unwrap();
        let frames = extract_rgb_frames_limited(&video, &frames_dir, 10, 3).unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!((frames[0].width, frames[0].height), (16, 16));
        assert_eq!(frames[0].rgb.len(), 16 * 16 * 3);
        assert!(frames_dir.join("frame_000002.png").is_file());
        assert!(!frames_dir.join("frame_000003.png").exists());
        assert!(!frames_dir.join("frame_000010.png").exists());
        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(frames_dir.join("frames.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["max_frames"], 3);
        assert_eq!(manifest["hit_cap"], true);
        assert_eq!(manifest["frames"].as_array().map(Vec::len), Some(3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn ffmpeg_present() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_ok_and(|out| out.status.success())
    }
}
