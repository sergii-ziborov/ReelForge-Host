//! Original deterministic music and event-synchronized cues, without a provider.

use super::model::State;
use crate::Result;
use std::{io::Write, path::Path};

pub(super) fn score(state: &State, path: &Path) -> Result<()> {
    let rate = 48_000_u32;
    let frames = u32::try_from((state.request.duration * f64::from(rate)).round() as i64)
        .expect("validated duration");
    let mut file = std::fs::File::create(path)?;
    let bytes = frames * 4;
    file.write_all(b"RIFF")?;
    file.write_all(&(36 + bytes).to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16_u32.to_le_bytes())?;
    file.write_all(&1_u16.to_le_bytes())?;
    file.write_all(&2_u16.to_le_bytes())?;
    file.write_all(&rate.to_le_bytes())?;
    file.write_all(&(rate * 4).to_le_bytes())?;
    file.write_all(&4_u16.to_le_bytes())?;
    file.write_all(&16_u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&bytes.to_le_bytes())?;
    let tau = std::f64::consts::TAU;
    let events = state
        .plan
        .as_ref()
        .map(|p| p.events.as_slice())
        .unwrap_or_default();
    for frame in 0..frames {
        let t = f64::from(frame) / f64::from(rate);
        let chord = if t < state.request.duration * 0.4 {
            [130.81, 155.56, 196.0]
        } else {
            [103.83, 155.56, 207.65]
        };
        let pad: f64 = chord
            .iter()
            .map(|hz| (tau * hz * t).sin() * 0.025 + (tau * hz * 2.0 * t).sin() * 0.008)
            .sum();
        let beat = t % 0.625;
        let pulse = 0.16
            * (-beat * 18.0).exp()
            * (tau * (48.0 * t + 3.0 * (1.0 - (-beat * 12.0).exp()))).sin();
        let mut left = pad + pulse;
        let mut right = pad + pulse;
        for event in events {
            let age = t - event.at;
            if !(0.0..0.6).contains(&age) {
                continue;
            }
            let hz = if event.kind == "collect" {
                660.0
            } else {
                220.0
            };
            let cue = 0.10 * (-age * 8.0).exp() * (tau * (hz * age + 220.0 * age * age)).sin();
            left += cue;
            right += cue * 0.85;
        }
        let fade =
            (t / 0.12).clamp(0.0, 1.0) * ((state.request.duration - t) / 0.35).clamp(0.0, 1.0);
        for value in [left, right] {
            file.write_all(
                &(((value * fade).clamp(-0.98, 0.98) * 32767.0).round() as i16).to_le_bytes(),
            )?;
        }
    }
    Ok(())
}
