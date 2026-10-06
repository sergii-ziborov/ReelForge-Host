//! Host ffmpeg decode: probe + RGB frames. No libav.

use crate::error::{HostError, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// One decoded RGB8 frame with media time.
#[derive(Debug, Clone)]
pub struct RgbFrame {
    /// Zero-based extracted-frame index.
    pub index: u64,
    /// Presentation ticks at [`Self::timescale`].
    ///
    /// Source time from ffmpeg `showinfo` when that log is present, in
    /// microseconds. Otherwise the sample index at `sample_fps`.
    pub ticks: i64,
    /// Ticks per second. `1_000_000` for a source presentation time.
    pub timescale: u32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// Packed RGB8.
    pub rgb: Vec<u8>,
}

/// One analysis sample and the source time it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxySample {
    /// Index in the extracted sequence.
    pub index: u64,
    /// Source presentation ticks.
    pub pts_ticks: i64,
    /// Ticks per second for [`Self::pts_ticks`].
    pub timescale: u32,
}

/// Source time to the extracted frame that covers it.
///
/// A time between samples holds the earlier sample. The map is the analysis
/// coverage, not every frame of the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyMap {
    /// Samples in extract order.
    pub samples: Vec<ProxySample>,
}

impl ProxyMap {
    /// Build the map from extracted frames. The RGB buffers are not copied.
    #[must_use]
    pub fn from_frames(frames: &[RgbFrame]) -> Self {
        Self {
            samples: frames
                .iter()
                .map(|frame| ProxySample {
                    index: frame.index,
                    pts_ticks: frame.ticks,
                    timescale: frame.timescale,
                })
                .collect(),
        }
    }

    /// Extracted index of the last sample at or before `ticks`.
    ///
    /// `timescale == 0`, or a time before every sample, returns `None`.
    #[must_use]
    pub fn index_at(&self, ticks: i64, timescale: u32) -> Option<u64> {
        if timescale == 0 {
            return None;
        }
        let mut best: Option<(i128, u64)> = None;
        for sample in &self.samples {
            let Some(sample_ticks) = rescale_ticks(sample.pts_ticks, sample.timescale, timescale)
            else {
                continue;
            };
            if sample_ticks > i128::from(ticks) {
                continue;
            }
            match best {
                Some((prev, _)) if prev > sample_ticks => {}
                _ => best = Some((sample_ticks, sample.index)),
            }
        }
        best.map(|(_, index)| index)
    }
}

fn rescale_ticks(ticks: i64, from: u32, to: u32) -> Option<i128> {
    if from == 0 || to == 0 {
        return None;
    }
    i128::from(ticks)
        .checked_mul(i128::from(to))?
        .checked_div(i128::from(from))
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

/// Microseconds. Source presentation times from `showinfo` use this clock.
const SOURCE_PTS_TIMESCALE: u32 = 1_000_000;

/// A private directory for one extract. Two calls do not share a path.
///
/// [`clear_frame_pngs`] removes `frame_*.png` only inside the directory it is
/// given, so one job cannot delete another job's frames.
#[must_use]
pub fn fresh_frames_dir(parent: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    parent.join(format!("frames-{nanos}-{n}"))
}

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

/// Flag a running extract can poll. Cloning shares the same flag.
///
/// Cancelling kills only the ffmpeg process that extract owns. Another
/// extract, and files outside that extract's directory, stay in place.
#[derive(Clone, Debug)]
pub struct ExtractCancel {
    flag: Arc<AtomicBool>,
}

impl ExtractCancel {
    /// A flag that is not cancelled.
    #[must_use]
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Ask the extract that holds this flag to kill its ffmpeg process.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }

    /// Whether [`Self::cancel`] has been called.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

impl Default for ExtractCancel {
    fn default() -> Self {
        Self::new()
    }
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
    extract_rgb_frames_cancellable(video, out_dir, sample_fps, max_frames, None)
}

/// Like [`extract_rgb_frames_limited`], but `cancel` kills this extract's ffmpeg.
///
/// A flag that is already cancelled returns before any directory is created.
///
/// # Errors
///
/// Cancellation, a zero cap, ffmpeg failure, or unreadable PNGs.
pub fn extract_rgb_frames_cancellable(
    video: &Path,
    out_dir: &Path,
    sample_fps: u32,
    max_frames: u32,
    cancel: Option<&ExtractCancel>,
) -> Result<Vec<RgbFrame>> {
    if cancel.is_some_and(ExtractCancel::is_cancelled) {
        return Err(HostError::Ffmpeg("extract cancelled".into()));
    }
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
    let fps = format!("fps={sample_fps},showinfo");
    let limit = max_frames.to_string();
    let stderr = run_ffmpeg_extract(video, &pattern, &fps, &limit, cancel)?;
    let source_pts = pts_times(&stderr);

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
    apply_source_pts(&mut frames, &source_pts);
    let proxy = ProxyMap::from_frames(&frames);
    let names = frame_png_names(index);
    let manifest = serde_json::json!({
        "frames": names,
        "max_frames": max_frames,
        "hit_cap": hit_cap,
        "sample_fps": sample_fps,
        "timescale": frames[0].timescale,
        "pts_ticks": frames.iter().map(|frame| frame.ticks).collect::<Vec<_>>(),
        "proxy": proxy.samples.iter().map(|sample| serde_json::json!({
            "index": sample.index,
            "pts_ticks": sample.pts_ticks,
            "timescale": sample.timescale,
        })).collect::<Vec<_>>(),
    });
    std::fs::write(out_dir.join("frames.json"), manifest.to_string())
        .map_err(|e| HostError::Ffmpeg(format!("frame manifest: {e}")))?;
    Ok(frames)
}

fn run_ffmpeg_extract(
    video: &Path,
    pattern: &Path,
    fps: &str,
    limit: &str,
    cancel: Option<&ExtractCancel>,
) -> Result<String> {
    if cancel.is_some_and(ExtractCancel::is_cancelled) {
        return Err(HostError::Ffmpeg("extract cancelled".into()));
    }
    let log_path = pattern.with_file_name(".extract-stderr.txt");
    let log_file = std::fs::File::create(&log_path)
        .map_err(|err| HostError::Ffmpeg(format!("extract log: {err}")))?;
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "info", "-y", "-i"])
        .arg(video)
        .args(["-vf", fps, "-start_number", "0", "-frames:v", limit])
        .arg(pattern)
        .stderr(Stdio::from(log_file))
        .stdout(Stdio::null())
        .spawn()
        .map_err(|err| HostError::Ffmpeg(format!("ffmpeg spawn: {err}")))?;
    let status = match wait_ffmpeg(&mut child, cancel) {
        Ok(status) => status,
        Err(err) => {
            let _ = std::fs::remove_file(&log_path);
            return Err(err);
        }
    };
    let stderr = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_file(&log_path);
    if !status.success() {
        let tail: String = stderr.chars().rev().take(400).collect();
        let tail: String = tail.chars().rev().collect();
        return Err(HostError::Ffmpeg(format!(
            "ffmpeg extract frames failed ({status}): {tail}"
        )));
    }
    Ok(stderr)
}

fn wait_ffmpeg(
    child: &mut std::process::Child,
    cancel: Option<&ExtractCancel>,
) -> Result<std::process::ExitStatus> {
    loop {
        if cancel.is_some_and(ExtractCancel::is_cancelled) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(HostError::Ffmpeg("extract cancelled".into()));
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(err) => return Err(HostError::Ffmpeg(format!("ffmpeg wait: {err}"))),
        }
    }
}

fn pts_times(stderr: &str) -> Vec<f64> {
    let mut times = Vec::new();
    let mut rest = stderr;
    while let Some(pos) = rest.find("pts_time:") {
        let after = &rest[pos + "pts_time:".len()..];
        let token = after.split_whitespace().next().unwrap_or("");
        if let Ok(value) = token.parse::<f64>()
            && value.is_finite()
            && value >= 0.0
        {
            times.push(value);
        }
        rest = if token.is_empty() {
            ""
        } else {
            &after[token.len()..]
        };
    }
    times
}

#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn pts_ticks(secs: f64) -> Option<i64> {
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    let ticks = (secs * f64::from(SOURCE_PTS_TIMESCALE)).round();
    if !ticks.is_finite() || ticks < 0.0 || ticks > i64::MAX as f64 {
        return None;
    }
    Some(ticks as i64)
}

fn apply_source_pts(frames: &mut [RgbFrame], source_pts: &[f64]) {
    if source_pts.len() < frames.len() {
        return;
    }
    let mut timed = Vec::with_capacity(frames.len());
    for secs in source_pts.iter().take(frames.len()) {
        let Some(ticks) = pts_ticks(*secs) else {
            return;
        };
        timed.push(ticks);
    }
    for (frame, ticks) in frames.iter_mut().zip(timed) {
        frame.ticks = ticks;
        frame.timescale = SOURCE_PTS_TIMESCALE;
    }
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
        assert_eq!(frames[0].timescale, 1_000_000);
        assert_eq!(
            frames.iter().map(|frame| frame.ticks).collect::<Vec<_>>(),
            vec![0, 100_000, 200_000]
        );
        let proxy = ProxyMap::from_frames(&frames);
        assert_eq!(proxy.index_at(150_000, 1_000_000), Some(1));
        assert_eq!(manifest["proxy"].as_array().map(Vec::len), Some(3));
        assert_eq!(manifest["sample_fps"], 10);
        let other = fresh_frames_dir(&dir);
        let again = extract_rgb_frames_limited(&video, &other, 10, 3).unwrap();
        assert_eq!(again.len(), 3);
        assert!(frames_dir.join("frame_000000.png").is_file());
        assert!(other.join("frame_000000.png").is_file());
        assert_ne!(frames_dir, other);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fresh_frames_dirs_do_not_share_a_path() {
        let parent = std::env::temp_dir().join(format!("rf-host-dirs-{}", std::process::id()));
        let first = fresh_frames_dir(&parent);
        let second = fresh_frames_dir(&parent);
        assert_ne!(first, second);
        assert!(first.starts_with(&parent));
        assert!(second.starts_with(&parent));
    }

    #[test]
    fn cancelled_extract_does_not_create_a_directory() {
        let dir = std::env::temp_dir().join(format!("rf-host-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cancel = ExtractCancel::new();
        cancel.cancel();
        let err =
            extract_rgb_frames_cancellable(Path::new("no-such.mp4"), &dir, 5, 3, Some(&cancel))
                .unwrap_err();
        assert!(err.to_string().contains("cancel"), "{err}");
        assert!(!dir.exists());
    }

    #[test]
    fn cancel_kills_one_ffmpeg_and_leaves_the_other() {
        if !ffmpeg_present() {
            return;
        }
        let Ok(mut cancelled) = paced_silence() else {
            return;
        };
        let Ok(mut other) = paced_silence() else {
            let _ = cancelled.kill();
            let _ = cancelled.wait();
            return;
        };
        let cancel = ExtractCancel::new();
        cancel.cancel();
        let err = wait_ffmpeg(&mut cancelled, Some(&cancel)).unwrap_err();
        assert!(err.to_string().contains("cancel"), "{err}");
        assert!(
            other.try_wait().unwrap().is_none(),
            "cancel must not kill the other ffmpeg"
        );
        let _ = other.kill();
        let _ = other.wait();
    }

    fn paced_silence() -> std::io::Result<std::process::Child> {
        Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=8000:cl=mono",
                "-af",
                "arealtime",
                "-t",
                "30",
                "-f",
                "null",
                "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    }

    #[test]
    fn proxy_map_holds_the_previous_sample() {
        let frames = [
            proxy_frame(0, 0),
            proxy_frame(1, 100_000),
            proxy_frame(2, 200_000),
        ];
        let map = ProxyMap::from_frames(&frames);
        assert_eq!(map.index_at(-1, 1_000_000), None);
        assert_eq!(map.index_at(0, 1_000_000), Some(0));
        assert_eq!(map.index_at(150_000, 1_000_000), Some(1));
        assert_eq!(map.index_at(200_000, 1_000_000), Some(2));
        assert_eq!(map.index_at(1, 10), Some(1));
        assert_eq!(map.index_at(0, 0), None);
        assert!(ProxyMap::from_frames(&[]).index_at(0, 1_000_000).is_none());
    }

    fn proxy_frame(index: u64, ticks: i64) -> RgbFrame {
        RgbFrame {
            index,
            ticks,
            timescale: 1_000_000,
            width: 1,
            height: 1,
            rgb: vec![0, 0, 0],
        }
    }

    fn ffmpeg_present() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_ok_and(|out| out.status.success())
    }
}
