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
    })
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
