//! Decode the delivered file; export success alone is insufficient qualification.

use super::{model::State, store};
use crate::{HostError, Result};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path, process::Command};

pub(super) fn inspect(state: &State, path: &Path, actions: Value) -> Result<Value> {
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_frames",
            "-show_streams",
            "-show_format",
            "-of",
            "json",
        ])
        .arg(path)
        .output()?;
    if !probe.status.success() {
        return Err(HostError::message("output probe failed"));
    }
    let info: Value = serde_json::from_slice(&probe.stdout)?;
    let streams = info["streams"]
        .as_array()
        .ok_or_else(|| HostError::message("output streams missing"))?;
    let video = streams
        .iter()
        .find(|s| s["codec_type"] == "video")
        .ok_or_else(|| HostError::message("output has no video"))?;
    let audio = streams
        .iter()
        .find(|s| s["codec_type"] == "audio")
        .ok_or_else(|| HostError::message("output has no sound"))?;
    let expected =
        u64::try_from((state.request.duration * f64::from(state.request.fps)).round() as i64)
            .expect("validated duration");
    let frames = video["nb_read_frames"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let duration = info["format"]["duration"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    if frames != expected
        || (duration - state.request.duration).abs() > 1.0 / f64::from(state.request.fps)
        || video["width"] != state.request.width
        || video["height"] != state.request.height
    {
        return Err(HostError::message(
            "output clock, frame count or canvas failed qualification",
        ));
    }
    let decoded = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-f", "framemd5", "-"])
        .output()?;
    if !decoded.status.success() {
        return Err(HostError::message("output video decode failed"));
    }
    let text = String::from_utf8_lossy(&decoded.stdout);
    let hashes: BTreeSet<_> = text
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.rsplit(',').next())
        .collect();
    if hashes.len() < usize::try_from(expected / 2).unwrap_or(usize::MAX) {
        return Err(HostError::message("too many repeated output frames"));
    }
    let pcm = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:a:0",
            "-t",
            &state.request.duration.to_string(),
            "-f",
            "f32le",
            "-ac",
            "2",
            "-ar",
            "48000",
            "-",
        ])
        .output()?;
    if !pcm.status.success() {
        return Err(HostError::message("output audio decode failed"));
    }
    let (peak, rms) = audio_levels(&pcm.stdout)?;
    Ok(
        json!({"status":"passed","duration":duration,"frames":frames,"unique_frame_hashes":hashes.len(),"video":video,"audio":audio,"decoded_audio_peak":peak,"decoded_audio_rms":rms,"actions":actions,"sha256":store::file_digest(path)?,"perceptual_review":"not_automatically_proven"}),
    )
}

fn audio_levels(bytes: &[u8]) -> Result<(f32, f64)> {
    let mut peak = 0.0_f32;
    let mut sum = 0.0_f64;
    let mut count = 0_u64;
    for bytes in bytes.chunks_exact(4) {
        let value = f32::from_le_bytes(bytes.try_into().expect("four bytes"));
        if !value.is_finite() {
            return Err(HostError::message("nonfinite decoded audio"));
        }
        peak = peak.max(value.abs());
        sum += f64::from(value).powi(2);
        count += 1;
    }
    let rms = (sum / (count.max(1) as f64)).sqrt();
    if peak >= 1.0 || rms < 0.005 {
        return Err(HostError::message("sound clips or is effectively silent"));
    }
    Ok((peak, rms))
}
