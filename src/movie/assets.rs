//! Asset generation/reuse with recipe and exact-byte receipts.

use super::{
    model::{Asset, PendingGeneration, Receipt, State},
    provider, store,
};
use crate::{HostError, Result};
use reelforge::{Duration, Rgba8, TextClip, TextClipOptions};
use std::path::{Path, PathBuf};

pub(super) fn prepare(dir: &Path, state: &mut State) -> Result<()> {
    let assets = super::graph::ordered_assets(state)?;
    std::fs::create_dir_all(dir.join("assets"))?;
    for asset in assets {
        if dir.join("cancel.requested").exists() {
            return Err(HostError::message("production cancelled"));
        }
        let references = references(&asset, state)?;
        let reference_hashes = references
            .iter()
            .map(|p| store::file_digest(p))
            .collect::<Result<Vec<_>>>()?;
        let recipe = store::digest(&serde_json::to_vec(&(&asset, &reference_hashes))?);
        if let Some(receipt) = state.assets.get(&asset.id)
            && receipt.recipe == recipe
            && Path::new(&receipt.path).is_file()
            && store::file_digest(Path::new(&receipt.path))? == receipt.sha256
        {
            qualify_existing(dir, state, &asset.id)?;
            continue;
        }
        state.stage = format!("asset/{}", asset.id);
        let destination = dir
            .join("assets")
            .join(format!("{}-{}.png", asset.id, &recipe[..16]));
        let events = if asset.kind == "image" {
            let (source, events) =
                generate(dir, state, &asset, &recipe, &references, &reference_hashes)?;
            let after = references
                .iter()
                .map(|p| store::file_digest(p))
                .collect::<Result<Vec<_>>>()?;
            if after != reference_hashes {
                return Err(HostError::message(
                    "generation read set changed; output not accepted",
                ));
            }
            std::fs::copy(&source, &destination)?;
            Some(events.to_string_lossy().into_owned())
        } else if asset.kind == "existing" {
            if references.len() != 1 {
                return Err(HostError::message(
                    "existing image requires one exact reference",
                ));
            }
            std::fs::copy(&references[0], &destination)?;
            None
        } else {
            raster(&asset, &destination)?;
            None
        };
        let (width, height) =
            image::image_dimensions(&destination).map_err(|e| HostError::message(e.to_string()))?;
        let decoded = image::open(&destination)
            .map_err(|e| HostError::message(e.to_string()))?
            .to_rgba8();
        if asset.transparent && !decoded.pixels().any(|p| p.0[3] == 0) {
            return Err(HostError::message(
                "sprite requested transparency but output is opaque",
            ));
        }
        if !decoded.pixels().any(|p| p.0[3] > 16) {
            return Err(HostError::message("asset contains no visible pixels"));
        }
        let receipt = Receipt {
            recipe,
            path: destination.to_string_lossy().into_owned(),
            sha256: store::file_digest(&destination)?,
            width,
            height,
            provider_events: events,
            provider_evidence: state
                .pending_generation
                .as_ref()
                .filter(|p| p.asset_id == asset.id)
                .map(|p| {
                    dir.join(format!("{}.receipt.json", p.job))
                        .to_string_lossy()
                        .into_owned()
                }),
        };
        state.assets.insert(asset.id, receipt);
        if state
            .pending_generation
            .as_ref()
            .is_some_and(|p| state.assets.contains_key(&p.asset_id))
        {
            state.pending_generation = None;
        }
        store::commit(dir, state, "asset.accepted")?;
    }
    Ok(())
}

pub(super) fn references(asset: &Asset, state: &State) -> Result<Vec<PathBuf>> {
    asset
        .reference_ids
        .iter()
        .map(|id| {
            state
                .request
                .references
                .get(id)
                .or_else(|| state.assets.get(id).map(|r| &r.path))
                .map(PathBuf::from)
                .ok_or_else(|| HostError::message("reference not accepted"))
        })
        .collect()
}

fn generate(
    dir: &Path,
    state: &mut State,
    asset: &Asset,
    recipe: &str,
    refs: &[PathBuf],
    hashes: &[String],
) -> Result<(PathBuf, PathBuf)> {
    if let Some(pending) = &state.pending_generation {
        if pending.asset_id != asset.id
            || pending.recipe != recipe
            || pending.reference_hashes != hashes
        {
            return Err(HostError::message(
                "pending generation conflicts with current dependency read set",
            ));
        }
        return provider::recover_image(dir, &pending.job);
    }
    if state.images_used >= state.request.max_images {
        return Err(HostError::message(
            "per-video image budget exhausted; generation not launched",
        ));
    }
    state.images_used += 1;
    let job = format!("image-{}-r{}", asset.id, state.revision + 1);
    state.pending_generation = Some(PendingGeneration {
        asset_id: asset.id.clone(),
        recipe: recipe.into(),
        job: job.clone(),
        reference_hashes: hashes.to_vec(),
    });
    store::commit(dir, state, "image.reserved")?;
    provider::image(dir, &job, &asset.prompt, refs, asset.transparent)
}

fn qualify_existing(dir: &Path, state: &mut State, id: &str) -> Result<()> {
    let receipt = state.assets.get(id).expect("accepted asset");
    if receipt.provider_evidence.is_none()
        && let Some(events) = &receipt.provider_events
    {
        let job = Path::new(events)
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".events.jsonl"))
            .ok_or_else(|| HostError::message("provider job receipt invalid"))?;
        let (source, _) = provider::recover_image(dir, job)?;
        if store::file_digest(&source)? != receipt.sha256 {
            return Err(HostError::message(
                "native output differs from accepted asset",
            ));
        }
        let path = dir
            .join(format!("{job}.receipt.json"))
            .to_string_lossy()
            .into_owned();
        state
            .assets
            .get_mut(id)
            .expect("accepted asset")
            .provider_evidence = Some(path);
        store::commit(dir, state, "provider.qualified")?;
    }
    Ok(())
}

fn raster(asset: &Asset, path: &Path) -> Result<()> {
    if asset.kind == "text" {
        let mut options = TextClipOptions::new(&asset.text, 80, Duration::from_secs(1.0))
            .with_padding(8)
            .with_color(Rgba8::new(
                asset.color[0],
                asset.color[1],
                asset.color[2],
                asset.color[3],
            ));
        let configured = std::env::var("REELFORGE_FONT").ok();
        let fonts = [
            configured.as_deref().unwrap_or(""),
            "C:/Windows/Fonts/seguisb.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        ];
        if let Some(font) = fonts.iter().find(|p| Path::new(p).is_file()) {
            options = options.with_font_path(*font);
        }
        let clip = TextClip::new(&options).map_err(|e| HostError::message(e.to_string()))?;
        let frame = clip.frame();
        image::save_buffer(
            path,
            frame.data(),
            frame.size().width,
            frame.size().height,
            image::ColorType::Rgba8,
        )
        .map_err(|e| HostError::message(e.to_string()))?;
        return Ok(());
    }
    let mut picture = image::RgbaImage::new(128, 128);
    for (x, y, pixel) in picture.enumerate_pixels_mut() {
        let dx = f64::from(x) - 63.5;
        let dy = f64::from(y) - 63.5;
        let radius = dx.hypot(dy);
        let alpha = match asset.kind.as_str() {
            "glow" => (-radius * radius / 520.0).exp() * (1.0 - radius / 64.0).max(0.0),
            "spark" => {
                let diamond = (1.0 - (dx.abs() + dy.abs()) / 29.0).clamp(0.0, 1.0);
                diamond + 0.2 * (-radius * radius / 450.0).exp()
            }
            "ring" => {
                (-((radius - 37.0) / 2.3).powi(2)).exp()
                    + 0.16 * (-((radius - 37.0) / 8.0).powi(2)).exp()
            }
            _ => return Err(HostError::message("unsupported native asset kind")),
        };
        *pixel = image::Rgba([
            asset.color[0],
            asset.color[1],
            asset.color[2],
            u8::try_from((alpha.clamp(0.0, 1.0) * f64::from(asset.color[3])).round() as i64)
                .expect("bounded alpha"),
        ]);
    }
    picture
        .save(path)
        .map_err(|e| HostError::message(e.to_string()))?;
    Ok(())
}
