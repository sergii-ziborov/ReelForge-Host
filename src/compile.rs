//! Intelligence compile: rewrite FramePick → resolve → mask package → graph JSON.

use crate::error::{HostError, Result};
use reelforge_intelligence_core::{
    BridgeOptions, IntelligenceService, MaskRequest, RedactionKind, SelectorBinding, SemanticEdit,
    SemanticEditPlan, SubjectSelector, UncertaintyPolicy, rewrite_selectors,
};
use reelforge_intelligence_sightloom::{export_and_pin_mask_package, load_package};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Output of resolve + bridge.
#[derive(Debug, Clone, Serialize)]
pub struct BridgeOut {
    /// Written graph path.
    pub graph_path: PathBuf,
    /// Written mask package directory.
    pub mask_package: PathBuf,
    /// Mask package id.
    pub mask_package_id: Option<String>,
    /// Subject count in the freeze.
    pub subjects: usize,
}

/// Build the stock `blur_everyone_except` intent with a FramePick on the photo.
#[must_use]
pub fn photo_except_plan(
    video: &Path,
    photo: &Path,
    photo_box: [f32; 4],
    output: &Path,
) -> SemanticEditPlan {
    let mut plan = SemanticEditPlan::new(video.to_string_lossy().into_owned()).with_edit(
        SemanticEdit::BlurEveryoneExcept {
            allowed: SubjectSelector::FramePick {
                media: photo.to_string_lossy().into_owned(),
                frame_index: 0,
                box_xyxy: photo_box,
            },
            uncertain_identity: Some(UncertaintyPolicy::Blur),
        },
    );
    plan.target_output = Some(output.to_string_lossy().into_owned());
    plan
}

/// Parse `gaussian` / `pixelate` / `solid`. Host default is pixelate.
///
/// # Errors
///
/// Unknown token.
pub fn parse_redaction_kind(raw: Option<&str>) -> Result<RedactionKind> {
    RedactionKind::parse(raw.unwrap_or("pixelate"))
        .map_err(|e| HostError::message(e.to_string()))
}

/// Binding that maps the photo FramePick to the accepted subject id.
#[must_use]
pub fn photo_binding(photo: &Path, photo_box: [f32; 4], subject_id: u64) -> SelectorBinding {
    SelectorBinding {
        media: photo.to_string_lossy().into_owned(),
        frame_index: 0,
        box_xyxy: photo_box,
        ids: vec![subject_id],
    }
}

/// Rewrite + resolve-bridge `--mode final` + write graph / mask package.
///
/// # Errors
///
/// Package load, rewrite, resolve, or bridge.
pub fn resolve_bridge(
    package_dir: &Path,
    plan: SemanticEditPlan,
    bindings: &[SelectorBinding],
    output: Option<&Path>,
    work_dir: &Path,
    redaction: RedactionKind,
) -> Result<BridgeOut> {
    let plan = if bindings.is_empty() {
        plan
    } else {
        rewrite_selectors(plan, bindings).map_err(|e| HostError::Intelligence(e.to_string()))?
    };

    let mut loaded =
        load_package(package_dir).map_err(|e| HostError::Intelligence(format!("package: {e}")))?;
    let mask_dir = work_dir.join("mask_package");
    export_and_pin_mask_package(&mut loaded, &mask_dir)
        .map_err(|e| HostError::Intelligence(format!("mask package: {e}")))?;

    let svc = IntelligenceService::new();
    let mut resolved = svc
        .resolve_plan(&plan, &loaded.snapshot)
        .map_err(|e| HostError::Intelligence(e.to_string()))?;

    if !resolved.resolved_subjects.is_empty() {
        let subjects: Vec<_> = resolved
            .resolved_subjects
            .iter()
            .map(|s| s.id.clone())
            .collect();
        let ranges = if resolved.resolved_ranges.is_empty() {
            resolved
                .resolved_subjects
                .iter()
                .filter_map(|s| s.span)
                .collect()
        } else {
            resolved.resolved_ranges.clone()
        };
        if !ranges.is_empty() {
            let request = MaskRequest::final_subjects(subjects, ranges);
            let provider = loaded.provider();
            match svc.materialize_masks(&provider, &request) {
                Ok(artifact) => {
                    if !artifact.carries_true_geometry()
                        && matches!(
                            resolved.policy.privacy.missing_mask,
                            reelforge_intelligence_core::MissingMaskAction::Fail
                        )
                    {
                        return Err(HostError::Intelligence(
                            "final mode: true mask geometry unavailable".into(),
                        ));
                    }
                    if !artifact.regions.is_empty() || artifact.geometry.is_some() {
                        resolved = svc.with_mask_artifact(resolved, artifact);
                    }
                }
                Err(e) => return Err(HostError::Intelligence(format!("final masks: {e}"))),
            }
        }
    }

    let opts = BridgeOptions {
        output_uri: output.map(|p| p.to_string_lossy().into_owned()),
        require_approval: false,
        redaction_kind: redaction,
        ..BridgeOptions::default()
    };
    let (report, bridged) = svc
        .compile_and_bridge(&resolved, &opts)
        .map_err(|e| HostError::Intelligence(e.to_string()))?;
    if !report.ok {
        return Err(HostError::Intelligence("compile_and_bridge failed".into()));
    }

    let graph_path = work_dir.join("graph.json");
    std::fs::write(&graph_path, &bridged.graph_json)?;
    Ok(BridgeOut {
        graph_path,
        mask_package: mask_dir,
        mask_package_id: resolved.mask_package_id,
        subjects: resolved.resolved_subjects.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_style_defaults_to_pixelate() {
        assert_eq!(
            parse_redaction_kind(None).unwrap(),
            RedactionKind::Pixelate
        );
        assert_eq!(
            parse_redaction_kind(Some("gaussian")).unwrap(),
            RedactionKind::Gaussian
        );
        assert!(parse_redaction_kind(Some("swirl")).is_err());
    }
}
