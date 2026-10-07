//! Host ffmpeg decode: probe + RGB frames. No libav.

use crate::error::{HostError, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// One sampled picture on disk. The RGB bytes stay in the PNG until loaded.
#[derive(Debug, Clone)]
pub struct SampledPicture {
    /// Zero-based index in the extracted sample sequence.
    pub index: u64,
    /// Decoded ordinal of the source frame whose picture this sample shows.
    pub source_index: u64,
    /// Earlier sample that already showed this source frame.
    ///
    /// `None` when this is the first sample of that frame.
    pub duplicate_of: Option<u64>,
    /// Presentation ticks at [`Self::timescale`].
    pub ticks: i64,
    /// Ticks per second. `1_000_000` for a sampled presentation time.
    pub timescale: u32,
    path: PathBuf,
}

impl SampledPicture {
    /// Decode this picture. The buffer lives only as long as the caller keeps it.
    ///
    /// # Errors
    ///
    /// The PNG is missing or not RGB.
    pub fn load(&self) -> Result<RgbFrame> {
        let bytes = std::fs::read(&self.path)
            .map_err(|err| HostError::Ffmpeg(format!("read {}: {err}", self.path.display())))?;
        let decoded = sightloom_host::decode_encoded_rgb(&bytes)
            .map_err(|err| HostError::Ffmpeg(format!("decode {}: {err}", self.path.display())))?;
        Ok(RgbFrame {
            index: self.index,
            source_index: self.source_index,
            duplicate_of: self.duplicate_of,
            ticks: self.ticks,
            timescale: self.timescale,
            width: decoded.width,
            height: decoded.height,
            rgb: decoded.rgb,
        })
    }
}

/// How many RGB frames a batch visitor holds at once.
pub const RGB_BATCH_FRAMES: usize = 8;

/// One decoded RGB8 frame with media time.
#[derive(Debug, Clone)]
pub struct RgbFrame {
    /// Zero-based index in the extracted sample sequence.
    pub index: u64,
    /// Decoded ordinal of the source frame whose picture this sample shows.
    ///
    /// This is not [`Self::index`] and not [`Self::ticks`]. A repeated picture
    /// uses the same ordinal again.
    pub source_index: u64,
    /// Earlier sample that already showed this source frame.
    ///
    /// `None` when this is the first sample of that frame.
    pub duplicate_of: Option<u64>,
    /// Presentation ticks at [`Self::timescale`].
    ///
    /// Sampled presentation time after the constant-frame-rate gate, in microseconds.
    pub ticks: i64,
    /// Ticks per second. `1_000_000` for a sampled presentation time.
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
    /// Decoded ordinal of the source frame whose picture this sample shows.
    pub source_index: u64,
    /// Earlier sample that already showed this source frame.
    ///
    /// `None` when this is the first sample of that frame.
    pub duplicate_of: Option<u64>,
    /// Sampled presentation ticks.
    pub pts_ticks: i64,
    /// Ticks per second for [`Self::pts_ticks`].
    pub timescale: u32,
}

/// Which source frames an analysis sample actually observed.
///
/// A lower sample rate or a frame cap is coverage, not a claim that the missing
/// source frames were seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SampleCoverage {
    /// Requested analysis rate. This is not the source frame rate.
    pub sample_fps: u32,
    /// Applied frame cap.
    pub max_frames: u32,
    /// The source would have produced more samples than [`Self::max_frames`].
    pub hit_cap: bool,
    /// Source frame numbers in sample order. A gap was not observed.
    pub source_index: Vec<u64>,
    /// True when every source frame in the probed duration was observed.
    ///
    /// False when the duration is unknown, the cap stopped the extract, or
    /// [`Self::source_index`] is not `0, 1, 2, …` with no gaps.
    pub covers_every_source_frame: bool,
}

/// Coverage of `pictures` against a probed source duration.
///
/// `duration_secs` that is not finite and positive leaves the cap unknown and
/// does not claim full coverage.
#[must_use]
pub fn sample_coverage(
    pictures: &[SampledPicture],
    sample_fps: u32,
    max_frames: u32,
    duration_secs: f64,
) -> SampleCoverage {
    let source_index: Vec<u64> = pictures
        .iter()
        .map(|picture| picture.source_index)
        .collect();
    let expected = expected_sample_count(duration_secs, sample_fps);
    let hit_cap = expected.is_some_and(|count| count > u64::from(max_frames));
    let contiguous = source_index
        .iter()
        .enumerate()
        .all(|(offset, source)| u64::try_from(offset).ok() == Some(*source));
    let covers_every_source_frame = expected
        .is_some_and(|count| u64::try_from(source_index.len()).ok() == Some(count))
        && !hit_cap
        && contiguous
        && !source_index.is_empty();
    SampleCoverage {
        sample_fps,
        max_frames,
        hit_cap,
        source_index,
        covers_every_source_frame,
    }
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
        Self::from_timed(frames.iter().map(|frame| {
            (
                frame.index,
                frame.source_index,
                frame.duplicate_of,
                frame.ticks,
                frame.timescale,
            )
        }))
    }

    /// Build the map from pictures that are still stored as PNG.
    #[must_use]
    pub fn from_pictures(pictures: &[SampledPicture]) -> Self {
        Self::from_timed(pictures.iter().map(|picture| {
            (
                picture.index,
                picture.source_index,
                picture.duplicate_of,
                picture.ticks,
                picture.timescale,
            )
        }))
    }

    fn from_timed(timed: impl Iterator<Item = (u64, u64, Option<u64>, i64, u32)>) -> Self {
        Self {
            samples: timed
                .map(
                    |(index, source_index, duplicate_of, pts_ticks, timescale)| ProxySample {
                        index,
                        source_index,
                        duplicate_of,
                        pts_ticks,
                        timescale,
                    },
                )
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

    /// Source frame number of the sample selected by [`Self::index_at`].
    ///
    /// `timescale == 0`, or a time before every sample, returns `None`.
    #[must_use]
    pub fn source_index_at(&self, ticks: i64, timescale: u32) -> Option<u64> {
        let index = self.index_at(ticks, timescale)?;
        self.samples
            .iter()
            .find(|sample| sample.index == index)
            .map(|sample| sample.source_index)
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
    time_base: Option<String>,
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

/// Microseconds. Sampled presentation times use this clock.
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
/// Cancelling kills only the ffmpeg and ffprobe processes this extract owns.
/// Another extract, and files outside that extract's directory, stay in place.
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

    /// Clear the flag so a later request is not born cancelled.
    pub fn reset(&self) {
        self.flag.store(false, Ordering::Relaxed);
    }
}

impl Default for ExtractCancel {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract at most `max_frames` RGB frames into one vector.
///
/// Analysis callers should use [`extract_sampled_pictures`] and
/// [`visit_rgb_batches`] so the RGB buffers stay in batches of
/// [`RGB_BATCH_FRAMES`]. This function still decodes every kept picture.
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
    let pictures =
        extract_sampled_pictures_cancellable(video, out_dir, sample_fps, max_frames, cancel)?;
    let mut frames = Vec::with_capacity(pictures.len());
    let batch = pictures.len().max(1);
    visit_rgb_batches_cancellable(&pictures, batch, cancel, |chunk| {
        frames.extend(chunk.iter().cloned());
        Ok(())
    })?;
    Ok(frames)
}

/// List capped pictures without decoding them into one RGB vector.
///
/// The sampler keeps at most `max_frames` pictures. PNG tails are removed.
/// `frames.json` records the cap. Call [`visit_rgb_batches`] to decode a few
/// pictures at a time.
///
/// # Errors
///
/// A zero cap, ffmpeg failure, or a presentation time that does not match the pictures.
pub fn extract_sampled_pictures(
    video: &Path,
    out_dir: &Path,
    sample_fps: u32,
    max_frames: u32,
) -> Result<Vec<SampledPicture>> {
    extract_sampled_pictures_cancellable(video, out_dir, sample_fps, max_frames, None)
}

/// Like [`extract_sampled_pictures`], but `cancel` kills this extract's ffmpeg.
///
/// # Errors
///
/// Cancellation, a zero cap, ffmpeg failure, or a presentation time that does not match the pictures.
pub fn extract_sampled_pictures_cancellable(
    video: &Path,
    out_dir: &Path,
    sample_fps: u32,
    max_frames: u32,
    cancel: Option<&ExtractCancel>,
) -> Result<Vec<SampledPicture>> {
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
    let _ = std::fs::remove_file(out_dir.join("source-index.rgb"));
    let mut pictures = sample_decoded_pictures(video, out_dir, sample_fps, max_frames, cancel)?;
    let indexes: Vec<u64> = pictures
        .iter()
        .map(|picture| picture.source_index)
        .collect();
    for (picture, repeat) in pictures.iter_mut().zip(repeat_of(&indexes)) {
        picture.duplicate_of = repeat;
    }
    write_frame_manifest(out_dir, max_frames, hit_cap, sample_fps, &pictures)?;
    Ok(pictures)
}

/// Decode `pictures` a few at a time. Each slice passed to `visit` has at most `batch` frames.
///
/// The RGB buffers are dropped before the next slice is decoded.
///
/// # Errors
///
/// `batch == 0`, a PNG that cannot be decoded, or an error from `visit`.
pub fn visit_rgb_batches(
    pictures: &[SampledPicture],
    batch: usize,
    visit: impl FnMut(&[RgbFrame]) -> Result<()>,
) -> Result<()> {
    visit_rgb_batches_cancellable(pictures, batch, None, visit)
}

/// Like [`visit_rgb_batches`], but `cancel` stops the walk before the next PNG is decoded.
///
/// A flag that is already cancelled does not open a picture.
///
/// # Errors
///
/// Cancellation, `batch == 0`, a PNG that cannot be decoded, or an error from `visit`.
pub fn visit_rgb_batches_cancellable(
    pictures: &[SampledPicture],
    batch: usize,
    cancel: Option<&ExtractCancel>,
    mut visit: impl FnMut(&[RgbFrame]) -> Result<()>,
) -> Result<()> {
    if batch == 0 {
        return Err(HostError::Ffmpeg(
            "extract refuses an empty rgb batch".into(),
        ));
    }
    let mut held = Vec::with_capacity(batch.min(pictures.len()));
    for chunk in pictures.chunks(batch) {
        if cancel.is_some_and(ExtractCancel::is_cancelled) {
            return Err(HostError::Ffmpeg("extract cancelled".into()));
        }
        held.clear();
        for picture in chunk {
            if cancel.is_some_and(ExtractCancel::is_cancelled) {
                return Err(HostError::Ffmpeg("extract cancelled".into()));
            }
            held.push(picture.load()?);
        }
        visit(&held)?;
        held.clear();
    }
    Ok(())
}

fn write_frame_manifest(
    out_dir: &Path,
    max_frames: u32,
    hit_cap: bool,
    sample_fps: u32,
    pictures: &[SampledPicture],
) -> Result<()> {
    let proxy = ProxyMap::from_pictures(pictures);
    let names = frame_png_names(u64::try_from(pictures.len()).unwrap_or(u64::MAX));
    let manifest = serde_json::json!({
        "frames": names,
        "max_frames": max_frames,
        "hit_cap": hit_cap,
        "sample_fps": sample_fps,
        "timescale": pictures[0].timescale,
        "pts_ticks": pictures.iter().map(|picture| picture.ticks).collect::<Vec<_>>(),
        "source_index": pictures.iter().map(|picture| picture.source_index).collect::<Vec<_>>(),
        "duplicate_of": pictures.iter().map(|picture| picture.duplicate_of).collect::<Vec<_>>(),
        "proxy": proxy.samples.iter().map(|sample| serde_json::json!({
            "index": sample.index,
            "source_index": sample.source_index,
            "duplicate_of": sample.duplicate_of,
            "pts_ticks": sample.pts_ticks,
            "timescale": sample.timescale,
        })).collect::<Vec<_>>(),
    });
    std::fs::write(out_dir.join("frames.json"), manifest.to_string())
        .map_err(|err| HostError::Ffmpeg(format!("frame manifest: {err}")))
}

struct InputClock {
    width: u32,
    height: u32,
    time_num: i64,
    time_den: i64,
    duration_secs: f64,
}

struct HeldFrame {
    source_index: u64,
    out_pts: i64,
    rgb: Vec<u8>,
}

#[derive(Default)]
struct FpsGate {
    held: [Option<HeldFrame>; 2],
    count: usize,
    next_pts: Option<i64>,
}

impl FpsGate {
    fn push_held(
        &mut self,
        frame: HeldFrame,
        room: usize,
        emit: &mut impl FnMut(u64, i64, &[u8]) -> Result<()>,
    ) -> Result<()> {
        if room == 0 || self.count >= 2 {
            return Ok(());
        }
        self.held[self.count] = Some(frame);
        self.count += 1;
        self.pump(false, 0, room, emit)
    }

    fn finish(
        &mut self,
        eof_pts: i64,
        room: usize,
        emit: &mut impl FnMut(u64, i64, &[u8]) -> Result<()>,
    ) -> Result<()> {
        self.pump(true, eof_pts, room, emit)
    }

    fn pump(
        &mut self,
        eof: bool,
        eof_pts: i64,
        room: usize,
        emit: &mut impl FnMut(u64, i64, &[u8]) -> Result<()>,
    ) -> Result<()> {
        let mut emitted = 0_usize;
        while emitted < room {
            if self.count == 0 || (self.count < 2 && !eof) {
                break;
            }
            let Some(first_pts) = self.held[0].as_ref().map(|frame| frame.out_pts) else {
                break;
            };
            let next = *self.next_pts.get_or_insert(first_pts);
            let later_due = self
                .held
                .get(1)
                .and_then(|frame| frame.as_ref())
                .is_some_and(|frame| self.count == 2 && frame.out_pts <= next);
            if later_due || (eof && eof_pts <= next) {
                self.shift();
                continue;
            }
            let Some(held) = self.held[0].as_ref() else {
                break;
            };
            let source = held.source_index;
            emit(source, next, held.rgb.as_slice())?;
            emitted += 1;
            self.next_pts = Some(next.saturating_add(1));
        }
        Ok(())
    }

    fn shift(&mut self) {
        if self.count == 0 {
            return;
        }
        self.held[0] = self.held[1].take();
        self.count -= 1;
    }
}

fn probe_input_clock(path: &Path) -> Result<InputClock> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,time_base",
            "-show_entries",
            "format=duration",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .map_err(|err| HostError::Ffmpeg(format!("ffprobe spawn: {err}")))?;
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
        .and_then(|streams| streams.into_iter().next())
        .ok_or_else(|| HostError::Ffmpeg("ffprobe: no video stream".into()))?;
    let width = stream
        .width
        .ok_or_else(|| HostError::Ffmpeg("ffprobe: no width".into()))?;
    let height = stream
        .height
        .ok_or_else(|| HostError::Ffmpeg("ffprobe: no height".into()))?;
    let (time_num, time_den) = parse_time_base(stream.time_base.as_deref())?;
    let duration_secs = parsed
        .format
        .and_then(|format| format.duration)
        .and_then(|text| text.parse().ok())
        .unwrap_or(0.0);
    Ok(InputClock {
        width,
        height,
        time_num,
        time_den,
        duration_secs,
    })
}

fn parse_time_base(raw: Option<&str>) -> Result<(i64, i64)> {
    let Some(raw) = raw else {
        return Err(HostError::Ffmpeg("ffprobe: no time base".into()));
    };
    let Some((num, den)) = raw.split_once('/') else {
        return Err(HostError::Ffmpeg("ffprobe: no time base".into()));
    };
    let Ok(num) = num.parse::<i64>() else {
        return Err(HostError::Ffmpeg("ffprobe: no time base".into()));
    };
    let Ok(den) = den.parse::<i64>() else {
        return Err(HostError::Ffmpeg("ffprobe: no time base".into()));
    };
    if num <= 0 || den <= 0 {
        return Err(HostError::Ffmpeg("ffprobe: no time base".into()));
    }
    Ok((num, den))
}

fn frame_bytes(width: u32, height: u32) -> Result<usize> {
    let Ok(width) = usize::try_from(width) else {
        return Err(HostError::Ffmpeg("ffprobe: no width".into()));
    };
    let Ok(height) = usize::try_from(height) else {
        return Err(HostError::Ffmpeg("ffprobe: no height".into()));
    };
    width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or_else(|| HostError::Ffmpeg("ffprobe: no width".into()))
}

/// `vf_fps` nearest rescale. `num` and `den` are the combined rational.
fn rescale_near(value: i64, num: i64, den: i64) -> Option<i64> {
    if den <= 0 || num < 0 || value == i64::MAX || value == i64::MIN {
        return (value == i64::MAX || value == i64::MIN).then_some(value);
    }
    let negative = value < 0;
    let magnitude = i128::from(value.unsigned_abs());
    let scaled = magnitude
        .checked_mul(i128::from(num))?
        .checked_add(i128::from(den) / 2)?
        / i128::from(den);
    let signed = if negative { -scaled } else { scaled };
    i64::try_from(signed).ok()
}

fn to_output_pts(pts: i64, time_num: i64, time_den: i64, sample_fps: u32) -> Result<i64> {
    let Some(num) = time_num.checked_mul(i64::from(sample_fps)) else {
        return Err(HostError::Ffmpeg(
            "extract refuses a presentation time that does not match the pictures".into(),
        ));
    };
    rescale_near(pts, num, time_den).ok_or_else(|| {
        HostError::Ffmpeg(
            "extract refuses a presentation time that does not match the pictures".into(),
        )
    })
}

fn output_ticks(out_pts: i64, sample_fps: u32) -> Result<i64> {
    if out_pts < 0 || sample_fps == 0 {
        return Err(HostError::Ffmpeg(
            "extract refuses a presentation time that does not match the pictures".into(),
        ));
    }
    let fps = i128::from(sample_fps);
    let ticks = (i128::from(out_pts) * 1_000_000 + fps / 2) / fps;
    i64::try_from(ticks).map_err(|_| {
        HostError::Ffmpeg(
            "extract refuses a presentation time that does not match the pictures".into(),
        )
    })
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn eof_output_pts(clock: &InputClock, sample_fps: u32) -> Option<i64> {
    if !(clock.duration_secs.is_finite() && clock.duration_secs > 0.0) {
        return None;
    }
    let ticks = (clock.duration_secs * clock.time_den as f64 / clock.time_num as f64).round();
    if !ticks.is_finite() || ticks < 0.0 || ticks > i64::MAX as f64 {
        return None;
    }
    to_output_pts(ticks as i64, clock.time_num, clock.time_den, sample_fps).ok()
}

fn write_rgb_png(path: &Path, rgb: &[u8], width: u32, height: u32) -> Result<()> {
    image::save_buffer_with_format(
        path,
        rgb,
        width,
        height,
        image::ExtendedColorType::Rgb8,
        image::ImageFormat::Png,
    )
    .map_err(|err| HostError::Ffmpeg(format!("frame png: {err}")))
}

type RawFrame = std::result::Result<Option<Vec<u8>>, String>;
type RawPts = std::result::Result<Option<i64>, String>;

fn read_raw_frames(mut input: impl Read, len: usize, tx: mpsc::SyncSender<RawFrame>) {
    loop {
        let mut buf = vec![0_u8; len];
        let mut filled = 0_usize;
        loop {
            if filled == len {
                break;
            }
            match input.read(&mut buf[filled..]) {
                Ok(0) if filled == 0 => {
                    let _ = tx.send(Ok(None));
                    return;
                }
                Ok(0) => {
                    let _ = tx.send(Err(
                        "extract refuses a presentation time that does not match the pictures"
                            .to_string(),
                    ));
                    return;
                }
                Ok(n) => filled += n,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => {
                    let _ = tx.send(Err(err.to_string()));
                    return;
                }
            }
        }
        if tx.send(Ok(Some(buf))).is_err() {
            return;
        }
    }
}

fn read_pts_lines(input: impl Read, tx: mpsc::SyncSender<RawPts>) {
    let mismatched = "extract refuses a presentation time that does not match the pictures";
    for line in BufReader::new(input).lines() {
        let line = match line {
            Ok(line) => line,
            Err(err) => {
                let _ = tx.send(Err(err.to_string()));
                return;
            }
        };
        let line = line.trim();
        // csv=p=0 can print a trailing comma on the first h264 timestamp (`0,`).
        let Some(field) = line
            .split(',')
            .map(str::trim)
            .find(|field| !field.is_empty())
        else {
            continue;
        };
        if field == "N/A" {
            if tx.send(Ok(None)).is_err() {
                return;
            }
            continue;
        }
        if let Ok(pts) = field.parse::<i64>() {
            if tx.send(Ok(Some(pts))).is_err() {
                return;
            }
        } else {
            let _ = tx.send(Err(mismatched.to_string()));
            return;
        }
    }
}

fn recv_matching(
    rx: &mpsc::Receiver<RawPts>,
    cancel: Option<&ExtractCancel>,
) -> Result<Option<i64>> {
    let started = std::time::Instant::now();
    loop {
        if cancel.is_some_and(ExtractCancel::is_cancelled) {
            return Err(HostError::Ffmpeg("extract cancelled".into()));
        }
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(Ok(pts)) => return Ok(pts),
            Ok(Err(err)) => return Err(HostError::Ffmpeg(err)),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(HostError::Ffmpeg(
                    "extract refuses a presentation time that does not match the pictures".into(),
                ));
            }
            Err(mpsc::RecvTimeoutError::Timeout) if started.elapsed() > Duration::from_secs(10) => {
                return Err(HostError::Ffmpeg(
                    "extract refuses a presentation time that does not match the pictures".into(),
                ));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

fn stop_child(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn sample_decoded_pictures(
    video: &Path,
    out_dir: &Path,
    sample_fps: u32,
    max_frames: u32,
    cancel: Option<&ExtractCancel>,
) -> Result<Vec<SampledPicture>> {
    let clock = probe_input_clock(video)?;
    let frame_len = frame_bytes(clock.width, clock.height)?;
    let mut ffmpeg = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-threads",
            "1",
            "-filter_threads",
            "1",
            "-nostats",
            "-i",
        ])
        .arg(video)
        .args([
            "-vf",
            "format=rgb24",
            "-an",
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| HostError::Ffmpeg(format!("ffmpeg spawn: {err}")))?;
    let mut ffprobe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "frame=best_effort_timestamp",
            "-of",
            "csv=p=0",
        ])
        .arg(video)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| HostError::Ffmpeg(format!("ffprobe spawn: {err}")))?;
    let raw_out = ffmpeg
        .stdout
        .take()
        .ok_or_else(|| HostError::Ffmpeg("ffmpeg spawn: no stdout".into()))?;
    let raw_err = ffmpeg
        .stderr
        .take()
        .ok_or_else(|| HostError::Ffmpeg("ffmpeg spawn: no stderr".into()))?;
    let pts_out = ffprobe
        .stdout
        .take()
        .ok_or_else(|| HostError::Ffmpeg("ffprobe spawn: no stdout".into()))?;
    let (frame_tx, frame_rx) = mpsc::sync_channel(2);
    let (pts_tx, pts_rx) = mpsc::sync_channel(8);
    let frames = thread::spawn(move || read_raw_frames(raw_out, frame_len, frame_tx));
    let pts = thread::spawn(move || read_pts_lines(pts_out, pts_tx));
    let logs = thread::spawn(move || {
        let _ = std::io::copy(&mut BufReader::new(raw_err), &mut std::io::sink());
    });
    let mut gate = FpsGate::default();
    let mut pictures = Vec::new();
    let mut source_index = 0_u64;
    let cap = u64::from(max_frames);
    let outcome = collect_samples(
        &clock,
        sample_fps,
        cap,
        cancel,
        &frame_rx,
        &pts_rx,
        &mut gate,
        out_dir,
        &mut pictures,
        &mut source_index,
    );
    stop_child(&mut ffmpeg);
    stop_child(&mut ffprobe);
    drop(frame_rx);
    drop(pts_rx);
    let _ = frames.join();
    let _ = pts.join();
    let _ = logs.join();
    outcome?;
    if pictures.is_empty() {
        return Err(HostError::Ffmpeg(
            "ffmpeg wrote no frames (empty or unreadable video)".into(),
        ));
    }
    Ok(pictures)
}

#[allow(clippy::too_many_arguments)]
fn collect_samples(
    clock: &InputClock,
    sample_fps: u32,
    cap: u64,
    cancel: Option<&ExtractCancel>,
    frame_rx: &mpsc::Receiver<RawFrame>,
    pts_rx: &mpsc::Receiver<RawPts>,
    gate: &mut FpsGate,
    out_dir: &Path,
    pictures: &mut Vec<SampledPicture>,
    source_index: &mut u64,
) -> Result<()> {
    loop {
        if cancel.is_some_and(ExtractCancel::is_cancelled) {
            return Err(HostError::Ffmpeg("extract cancelled".into()));
        }
        let stored = u64::try_from(pictures.len()).unwrap_or(u64::MAX);
        if stored >= cap {
            break;
        }
        let frame = match frame_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(Ok(Some(frame))) => frame,
            Ok(Ok(None)) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Err(err)) => return Err(HostError::Ffmpeg(err)),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
        };
        let Some(raw_pts) = recv_matching(pts_rx, cancel)? else {
            *source_index = source_index.saturating_add(1);
            continue;
        };
        let ordinal = *source_index;
        *source_index = source_index.saturating_add(1);
        let out_pts = to_output_pts(raw_pts, clock.time_num, clock.time_den, sample_fps)?;
        let room = usize::try_from(cap.saturating_sub(stored)).unwrap_or(usize::MAX);
        gate.push_held(
            HeldFrame {
                source_index: ordinal,
                out_pts,
                rgb: frame,
            },
            room,
            &mut |source, pts, rgb| {
                store_sample(out_dir, clock, sample_fps, pictures, source, pts, rgb)
            },
        )?;
    }
    let stored = u64::try_from(pictures.len()).unwrap_or(u64::MAX);
    if stored < cap {
        let Some(eof_pts) = eof_output_pts(clock, sample_fps) else {
            return Ok(());
        };
        let room = usize::try_from(cap.saturating_sub(stored)).unwrap_or(usize::MAX);
        gate.finish(eof_pts, room, &mut |source, pts, rgb| {
            store_sample(out_dir, clock, sample_fps, pictures, source, pts, rgb)
        })?;
    }
    Ok(())
}

fn store_sample(
    out_dir: &Path,
    clock: &InputClock,
    sample_fps: u32,
    pictures: &mut Vec<SampledPicture>,
    source: u64,
    out_pts: i64,
    rgb: &[u8],
) -> Result<()> {
    let Ok(index) = u64::try_from(pictures.len()) else {
        return Err(HostError::Ffmpeg(
            "extract refuses a presentation time that does not match the pictures".into(),
        ));
    };
    let path = out_dir.join(format!("frame_{index:06}.png"));
    write_rgb_png(&path, rgb, clock.width, clock.height)?;
    pictures.push(SampledPicture {
        index,
        source_index: source,
        duplicate_of: None,
        ticks: output_ticks(out_pts, sample_fps)?,
        timescale: SOURCE_PTS_TIMESCALE,
        path,
    });
    Ok(())
}

#[cfg(test)]
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

#[cfg(test)]
#[derive(Clone, Copy)]
struct ShowFrame {
    pts_time: f64,
}

/// `-frames:v` stops the pictures. `showinfo` can keep logging frames after that.
/// The written pictures are the first logged samples, in order.
#[cfg(test)]
fn written_showinfo(notes: &[ShowFrame], written: usize) -> Result<Vec<ShowFrame>> {
    if notes.len() < written {
        return Err(HostError::Ffmpeg(format!(
            "extract refuses a presentation time that does not match the pictures: logged {}, wrote {written}",
            notes.len()
        )));
    }
    Ok(notes.iter().take(written).copied().collect())
}

#[cfg(test)]
fn sampled_showinfo(stderr: &str) -> Result<Vec<ShowFrame>> {
    let groups = showinfo_groups(stderr);
    let mut groups = groups.into_iter();
    let Some(sampled) = groups.next() else {
        return Err(HostError::Ffmpeg(
            "extract refuses a frame log with no source index".into(),
        ));
    };
    if groups.next().is_some() || sampled.is_empty() {
        return Err(HostError::Ffmpeg(
            "extract refuses a frame log with no source index".into(),
        ));
    }
    Ok(sampled)
}

#[cfg(test)]
fn showinfo_groups(stderr: &str) -> Vec<Vec<ShowFrame>> {
    let mut order = Vec::new();
    let mut groups = Vec::new();
    for line in stderr.lines() {
        let Some(id) = showinfo_instance(line) else {
            continue;
        };
        let slot = if let Some(pos) = order.iter().position(|seen| *seen == id) {
            pos
        } else {
            order.push(id);
            groups.push(Vec::new());
            order.len() - 1
        };
        if let Some(frame) = showinfo_frame(line) {
            groups[slot].push(frame);
        }
    }
    groups
}

#[cfg(test)]
fn showinfo_instance(line: &str) -> Option<u32> {
    let pos = line.find("showinfo_")?;
    let rest = &line[pos + "showinfo_".len()..];
    let end = rest
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

#[cfg(test)]
fn showinfo_frame(line: &str) -> Option<ShowFrame> {
    let n_at = line.find("n:")?;
    let n_token = line[n_at + 2..].split_whitespace().next()?;
    n_token.parse::<u64>().ok()?;
    let time_at = line.find("pts_time:")?;
    let time_token = line[time_at + "pts_time:".len()..]
        .split_whitespace()
        .next()?;
    let pts_time = time_token.parse::<f64>().ok()?;
    if !pts_time.is_finite() || pts_time < 0.0 {
        return None;
    }
    Some(ShowFrame { pts_time })
}

fn repeat_of(indexes: &[u64]) -> Vec<Option<u64>> {
    let mut first_sample = std::collections::HashMap::<u64, u64>::new();
    let mut repeats = Vec::with_capacity(indexes.len());
    for (sample, source) in indexes.iter().enumerate() {
        let Ok(sample) = u64::try_from(sample) else {
            repeats.push(None);
            continue;
        };
        if let Some(first) = first_sample.get(source) {
            repeats.push(Some(*first));
        } else {
            first_sample.insert(*source, sample);
            repeats.push(None);
        }
    }
    repeats
}

#[cfg(test)]
trait MutableTiming {
    fn write_ticks(&mut self, ticks: i64, timescale: u32);
    fn write_source_index(&mut self, index: u64);
}

#[cfg(test)]
impl MutableTiming for RgbFrame {
    fn write_ticks(&mut self, ticks: i64, timescale: u32) {
        self.ticks = ticks;
        self.timescale = timescale;
    }

    fn write_source_index(&mut self, index: u64) {
        self.source_index = index;
    }
}

#[cfg(test)]
impl MutableTiming for SampledPicture {
    fn write_ticks(&mut self, ticks: i64, timescale: u32) {
        self.ticks = ticks;
        self.timescale = timescale;
    }

    fn write_source_index(&mut self, index: u64) {
        self.source_index = index;
    }
}

#[cfg(test)]
fn apply_source_index<T: MutableTiming>(frames: &mut [T], indexes: &[u64]) -> Result<()> {
    if indexes.len() != frames.len() {
        return Err(HostError::Ffmpeg(
            "extract refuses a sample count that does not match the pictures".into(),
        ));
    }
    for (frame, index) in frames.iter_mut().zip(indexes) {
        frame.write_source_index(*index);
    }
    Ok(())
}

#[cfg(test)]
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

#[cfg(test)]
fn apply_source_pts<T: MutableTiming>(frames: &mut [T], source_pts: &[f64]) -> Result<()> {
    if source_pts.len() != frames.len() {
        return Err(HostError::Ffmpeg(
            "extract refuses a presentation time that does not match the pictures".into(),
        ));
    }
    let mut timed = Vec::with_capacity(frames.len());
    for secs in source_pts {
        let Some(ticks) = pts_ticks(*secs) else {
            return Err(HostError::Ffmpeg(
                "extract refuses a presentation time that does not match the pictures".into(),
            ));
        };
        timed.push(ticks);
    }
    for (frame, ticks) in frames.iter_mut().zip(timed) {
        frame.write_ticks(ticks, SOURCE_PTS_TIMESCALE);
    }
    Ok(())
}

fn expected_samples(video: &Path, sample_fps: u32) -> Option<u64> {
    let info = probe_video(video).ok()?;
    expected_sample_count(info.duration_secs, sample_fps)
}

#[allow(clippy::cast_sign_loss)]
fn expected_sample_count(duration_secs: f64, sample_fps: u32) -> Option<u64> {
    if !(duration_secs.is_finite() && duration_secs > 0.0) {
        return None;
    }
    let count = (duration_secs * f64::from(sample_fps)).round();
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
        assert_eq!(
            frames.iter().map(|frame| frame.index).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.source_index)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
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
            source_index: index,
            duplicate_of: None,
            ticks,
            timescale: 1_000_000,
            width: 1,
            height: 1,
            rgb: vec![0, 0, 0],
        }
    }

    #[test]
    fn pts_csv_keeps_the_first_timestamp_field() {
        let (tx, rx) = mpsc::sync_channel(4);
        read_pts_lines(std::io::Cursor::new("0,\n1024\nN/A\n,\n"), tx);
        assert_eq!(rx.recv().unwrap().unwrap(), Some(0));
        assert_eq!(rx.recv().unwrap().unwrap(), Some(1024));
        assert_eq!(rx.recv().unwrap().unwrap(), None);
        assert!(rx.recv().is_err());
    }

    #[test]
    fn fps_keeps_the_frame_ffmpeg_keeps() {
        let thirty: Vec<i64> = (0..15).map(|n| n * 512).collect();
        assert_eq!(kept_sources(10, &thirty, 5), vec![1, 4, 7, 10, 13]);
        let ten: Vec<i64> = (0..5).map(|n| n * 1536).collect();
        assert_eq!(kept_sources(20, &ten, 8), vec![0, 0, 1, 1, 2, 2, 3, 3]);
    }

    fn kept_sources(sample_fps: u32, pts: &[i64], room: usize) -> Vec<u64> {
        let mut gate = FpsGate::default();
        let mut kept = Vec::new();
        for (ordinal, raw) in pts.iter().copied().enumerate() {
            let left = room.saturating_sub(kept.len());
            if left == 0 {
                break;
            }
            let out_pts = to_output_pts(raw, 1, 15_360, sample_fps).unwrap();
            let source = u64::try_from(ordinal).unwrap();
            gate.push_held(
                HeldFrame {
                    source_index: source,
                    out_pts,
                    rgb: vec![0],
                },
                left,
                &mut |source, _out, _rgb| {
                    kept.push(source);
                    Ok(())
                },
            )
            .unwrap();
        }
        kept
    }

    #[test]
    fn showinfo_zip_is_not_a_source_map() {
        let paired = "\
[Parsed_showinfo_0 @ 1] n:   0 pts: 0 pts_time:0
[Parsed_showinfo_2 @ 1] n:   0 pts: 0 pts_time:0
";
        let Err(err) = sampled_showinfo(paired) else {
            panic!("two showinfo logs were accepted as a source map");
        };
        assert!(err.to_string().contains("no source index"), "{err}");
        assert_eq!(repeat_of(&[1, 4, 7]), vec![None, None, None]);
        assert_eq!(repeat_of(&[0, 0, 1, 1]), vec![None, Some(0), None, Some(2)]);
        let one = "\
[Parsed_showinfo_1 @ 1] n:   0 pts: 0 pts_time:0
[Parsed_showinfo_1 @ 1] n:   1 pts: 1 pts_time:0.1
[Parsed_showinfo_1 @ 1] n:   2 pts: 2 pts_time:0.2
";
        let notes = sampled_showinfo(one).unwrap();
        let Err(err) = written_showinfo(&notes, 4) else {
            panic!("a short showinfo log was accepted");
        };
        assert!(err.to_string().contains("logged 3"), "{err}");
        assert!(err.to_string().contains("wrote 4"), "{err}");
        let mut frames = [
            proxy_frame(0, 0),
            proxy_frame(1, 100_000),
            proxy_frame(2, 200_000),
        ];
        apply_source_pts(
            &mut frames,
            &[notes[0].pts_time, notes[1].pts_time, notes[2].pts_time],
        )
        .unwrap();
        assert_eq!(frames[1].ticks, 100_000);
        let Err(err) = apply_source_pts(&mut frames, &[0.0, -0.1, 0.2]) else {
            panic!("a negative presentation time was accepted");
        };
        assert!(err.to_string().contains("does not match"), "{err}");
        let mut pictures = [SampledPicture {
            index: 0,
            source_index: 0,
            duplicate_of: None,
            ticks: 0,
            timescale: 1,
            path: PathBuf::from("unused.png"),
        }];
        apply_source_index(&mut pictures, &[4]).unwrap();
        apply_source_pts(&mut pictures, &[0.2]).unwrap();
        assert_eq!(pictures[0].source_index, 4);
        assert_eq!(pictures[0].ticks, 200_000);
        apply_source_index(&mut frames, &[1, 4, 7]).unwrap();
        let map = ProxyMap::from_frames(&frames);
        assert_eq!(map.index_at(150_000, 1_000_000), Some(1));
        assert_eq!(map.source_index_at(150_000, 1_000_000), Some(4));
        assert_ne!(map.source_index_at(150_000, 1_000_000), Some(1));
    }

    #[test]
    fn sampled_source_index_matches_the_kept_picture() {
        if !ffmpeg_present() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("rf-host-srcidx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let thirty = numbered_clip(&dir, "thirty.mov", 30, 15, "");
        let frames_dir = dir.join("frames30");
        let frames = extract_rgb_frames_limited(&thirty, &frames_dir, 10, 5).unwrap();
        let sources = source_of(&frames);
        assert_resampled(&frames);
        assert_duplicates(&frames);
        assert_output_ticks(&frames, 10);
        assert!(frames.iter().all(|frame| frame.duplicate_of.is_none()));
        assert!(
            frames
                .iter()
                .all(|frame| frame.width == 16 && frame.height == 16)
        );
        assert!(!frames_dir.join("source-index.rgb").exists());
        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(frames_dir.join("frames.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["source_index"], serde_json::json!(sources));
        let duplicates: Vec<Option<u64>> = frames.iter().map(|frame| frame.duplicate_of).collect();
        assert_eq!(manifest["duplicate_of"], serde_json::json!(duplicates));

        let twenty_four = numbered_clip(&dir, "twentyfour.mov", 24, 12, "");
        let from_twenty_four =
            extract_rgb_frames_limited(&twenty_four, &dir.join("frames24"), 10, 5).unwrap();
        source_of(&from_twenty_four);
        assert_resampled(&from_twenty_four);
        assert_duplicates(&from_twenty_four);
        assert_output_ticks(&from_twenty_four, 10);

        let upsampled = numbered_clip(&dir, "ten.mov", 10, 5, "");
        let doubled = extract_rgb_frames_limited(&upsampled, &dir.join("frames20"), 20, 8).unwrap();
        source_of(&doubled);
        assert_resampled(&doubled);
        assert_duplicates(&doubled);
        assert_output_ticks(&doubled, 20);
        assert!(
            doubled.iter().any(|frame| frame.duplicate_of.is_some()),
            "10 to 20 fps did not repeat a source frame"
        );

        let variable = numbered_clip(&dir, "vfr.mov", 30, 15, "setpts=N*N*0.02/TB");
        let varied = extract_rgb_frames_limited(&variable, &dir.join("framesvfr"), 10, 8).unwrap();
        source_of(&varied);
        assert_resampled(&varied);
        assert_duplicates(&varied);
        assert_output_ticks(&varied, 10);

        let shifted = numbered_clip(&dir, "neg.mov", 30, 30, "setpts=PTS-0.5/TB");
        let after_negative =
            extract_rgb_frames_limited(&shifted, &dir.join("framesneg"), 10, 3).unwrap();
        let burned = burned_reds(&shifted);
        assert_resampled(&after_negative);
        assert_duplicates(&after_negative);
        assert_output_ticks(&after_negative, 10);
        for frame in &after_negative {
            let stored = usize::try_from(frame.source_index).unwrap();
            let kept = burned.get(stored).copied().unwrap_or_else(|| {
                panic!(
                    "sample {} source {} is outside the {} stored frames",
                    frame.index,
                    frame.source_index,
                    burned.len()
                )
            });
            assert_eq!(
                frame.rgb[0], kept,
                "sample {} kept a different picture than source {}",
                frame.index, frame.source_index
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn assert_resampled(frames: &[RgbFrame]) {
        assert!(
            frames.iter().any(|frame| frame.source_index != frame.index),
            "fps kept the sample index instead of a source frame"
        );
    }

    fn assert_duplicates(frames: &[RgbFrame]) {
        let sources: Vec<u64> = frames.iter().map(|frame| frame.source_index).collect();
        let repeats: Vec<Option<u64>> = frames.iter().map(|frame| frame.duplicate_of).collect();
        assert_eq!(repeats, repeat_of(&sources));
    }

    fn assert_output_ticks(frames: &[RgbFrame], sample_fps: u32) {
        assert!(sample_fps > 0);
        assert!(1_000_000_u32.is_multiple_of(sample_fps));
        let step = i64::from(1_000_000 / sample_fps);
        let ticks: Vec<i64> = frames.iter().map(|frame| frame.ticks).collect();
        let expected: Vec<i64> = (0..frames.len())
            .map(|index| i64::try_from(index).unwrap() * step)
            .collect();
        assert_eq!(ticks, expected);
        assert!(frames.iter().all(|frame| frame.timescale == 1_000_000));
    }

    fn burned_reds(video: &Path) -> Vec<u8> {
        let raw = video.with_extension("raw");
        let status = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(video)
            .args(["-f", "rawvideo", "-pix_fmt", "rgb24"])
            .arg(&raw)
            .status()
            .unwrap();
        assert!(status.success());
        let bytes = std::fs::read(&raw).unwrap();
        bytes.chunks(16 * 16 * 3).map(|chunk| chunk[0]).collect()
    }

    fn source_of(frames: &[RgbFrame]) -> Vec<u64> {
        frames
            .iter()
            .map(|frame| {
                assert_eq!(
                    u64::from(frame.rgb[0]),
                    frame.source_index,
                    "sample {} pixel {} source {}",
                    frame.index,
                    frame.rgb[0],
                    frame.source_index
                );
                frame.source_index
            })
            .collect()
    }

    fn numbered_clip(dir: &Path, name: &str, rate: u32, frames: u32, setpts: &str) -> PathBuf {
        let video = dir.join(name);
        let mut filter = String::from("format=rgb24,geq=r='N':g='0':b='0'");
        if !setpts.is_empty() {
            filter.push(',');
            filter.push_str(setpts);
        }
        let count = frames.to_string();
        let source = format!("color=c=black:s=16x16:r={rate}:d=2");
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                &source,
                "-vf",
                &filter,
                "-frames:v",
                &count,
                "-pix_fmt",
                "rgb24",
                "-c:v",
                "png",
            ])
            .arg(&video)
            .status()
            .unwrap();
        assert!(status.success(), "{name}");
        video
    }

    #[test]
    fn coverage_names_the_source_frames_that_were_observed() {
        let gapped = [
            coverage_picture(0, 1),
            coverage_picture(1, 4),
            coverage_picture(2, 7),
        ];
        let capped = sample_coverage(&gapped, 10, 3, 1.0);
        assert!(capped.hit_cap);
        assert!(!capped.covers_every_source_frame);
        assert_eq!(capped.source_index, vec![1, 4, 7]);

        let partial = sample_coverage(&gapped, 10, 30, 1.0);
        assert!(!partial.hit_cap);
        assert!(!partial.covers_every_source_frame);

        let every = [
            coverage_picture(0, 0),
            coverage_picture(1, 1),
            coverage_picture(2, 2),
        ];
        let full = sample_coverage(&every, 10, 3, 0.3);
        assert!(!full.hit_cap);
        assert!(full.covers_every_source_frame);

        let stopped = sample_coverage(&every, 10, 3, 1.0);
        assert!(stopped.hit_cap);
        assert!(!stopped.covers_every_source_frame);

        let unknown = sample_coverage(&every, 10, 3, f64::NAN);
        assert!(!unknown.hit_cap);
        assert!(!unknown.covers_every_source_frame);
    }

    fn coverage_picture(index: u64, source_index: u64) -> SampledPicture {
        SampledPicture {
            index,
            source_index,
            duplicate_of: None,
            ticks: 0,
            timescale: 1,
            path: PathBuf::from("unused.png"),
        }
    }

    #[test]
    fn reset_clears_extract_cancel() {
        let cancel = ExtractCancel::new();
        cancel.cancel();
        assert!(cancel.is_cancelled());
        cancel.reset();
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn cancelled_batch_does_not_open_pictures() {
        let missing = std::env::temp_dir().join(format!(
            "rf-host-missing-picture-{}-{}.png",
            std::process::id(),
            "cancel"
        ));
        let _ = std::fs::remove_file(&missing);
        let pictures = [SampledPicture {
            index: 0,
            source_index: 0,
            duplicate_of: None,
            ticks: 0,
            timescale: 1,
            path: missing.clone(),
        }];
        let cancel = ExtractCancel::new();
        cancel.cancel();
        let err = visit_rgb_batches_cancellable(&pictures, 1, Some(&cancel), |_| {
            panic!("visitor ran after cancel");
        })
        .unwrap_err();
        assert!(err.to_string().contains("cancel"), "{err}");
        assert!(!missing.exists());
    }

    #[test]
    fn cancel_between_batches_does_not_decode_the_next_picture() {
        if !ffmpeg_present() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("rf-host-batch-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let first = dir.join("one.png");
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=gray:s=8x8:r=1",
                "-frames:v",
                "1",
            ])
            .arg(&first)
            .status()
            .unwrap();
        assert!(status.success());
        let pictures = [
            SampledPicture {
                index: 0,
                source_index: 0,
                duplicate_of: None,
                ticks: 0,
                timescale: 1,
                path: first,
            },
            SampledPicture {
                index: 1,
                source_index: 1,
                duplicate_of: None,
                ticks: 1,
                timescale: 1,
                path: dir.join("missing.png"),
            },
        ];
        let cancel = ExtractCancel::new();
        let flag = cancel.clone();
        let mut seen = 0_usize;
        let err = visit_rgb_batches_cancellable(&pictures, 1, Some(&cancel), |chunk| {
            seen += chunk.len();
            flag.cancel();
            Ok(())
        });
        let Err(err) = err else {
            panic!("the picture after cancel was decoded");
        };
        assert!(err.to_string().contains("cancel"), "{err}");
        assert_eq!(seen, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn visit_refuses_an_empty_rgb_batch() {
        let err = visit_rgb_batches(&[], 0, |_| Ok(())).unwrap_err();
        assert!(err.to_string().contains("refuses"), "{err}");
    }

    #[test]
    fn rgb_batches_drop_each_chunk() {
        if !ffmpeg_present() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("rf-host-batch-{}", std::process::id()));
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
                "color=c=green:s=16x16:r=10:d=1",
                "-frames:v",
                "3",
            ])
            .arg(&video)
            .status()
            .unwrap();
        assert!(status.success());
        let frames_dir = dir.join("frames");
        let pictures = extract_sampled_pictures(&video, &frames_dir, 10, 3).unwrap();
        assert_eq!(pictures.len(), 3);
        let mut lengths = Vec::new();
        visit_rgb_batches(&pictures, 2, |chunk| {
            lengths.push(chunk.len());
            assert!(chunk.len() <= 2);
            Ok(())
        })
        .unwrap();
        assert_eq!(lengths, vec![2, 1]);
        let mut indexes = Vec::new();
        let mut source = Vec::new();
        let mut ticks = Vec::new();
        visit_rgb_batches(&pictures, 2, |chunk| {
            for frame in chunk {
                indexes.push(frame.index);
                source.push(frame.source_index);
                ticks.push(frame.ticks);
                assert_eq!(frame.rgb.len(), 16 * 16 * 3);
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(indexes, vec![0, 1, 2]);
        assert_eq!(source, vec![0, 1, 2]);
        assert_eq!(ticks, vec![0, 100_000, 200_000]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn ffmpeg_present() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_ok_and(|out| out.status.success())
    }
}
