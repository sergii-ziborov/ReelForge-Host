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

/// Extract RGB frames at `sample_fps` into `out_dir` (`frame_000000.png` …).
///
/// # Errors
///
/// ffmpeg failure or unreadable PNGs.
pub fn extract_rgb_frames(video: &Path, out_dir: &Path, sample_fps: u32) -> Result<Vec<RgbFrame>> {
    if sample_fps == 0 {
        return Err(HostError::Ffmpeg("sample_fps must be > 0".into()));
    }
    std::fs::create_dir_all(out_dir)?;
    let pattern = out_dir.join("frame_%06d.png");
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(video)
        .args(["-vf", &format!("fps={sample_fps}"), "-start_number", "0"])
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
    loop {
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
    if frames.is_empty() {
        return Err(HostError::Ffmpeg(
            "ffmpeg wrote no frames (empty or unreadable video)".into(),
        ));
    }
    Ok(frames)
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
    if !src.is_file() {
        return Err(HostError::Ffmpeg(format!(
            "video not found: {} (or use cam / lavfi:testsrc=size=640x360:rate=10)",
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
