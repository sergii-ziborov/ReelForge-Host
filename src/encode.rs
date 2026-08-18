//! ReelForge encode: `run_render_graph` + optional MaskPackage host.

use crate::error::{HostError, Result};
use reelforge::{
    GraphRunOptions, RenderGraph, SightloomPackageHost, WriteControl, run_render_graph_with,
};
use std::path::Path;
use std::sync::Arc;

/// Load graph JSON, optionally attach a mask package, encode.
///
/// # Errors
///
/// Parse, package open, encode, or missing output file.
pub fn run_graph(
    graph_path: &Path,
    mask_package: Option<&Path>,
    output: Option<&Path>,
) -> Result<String> {
    let text = std::fs::read_to_string(graph_path)?;
    let mut graph = RenderGraph::from_json(&text).map_err(|e| HostError::Encode(e.to_string()))?;
    if let Some(out) = output {
        let slot = graph
            .outputs
            .first_mut()
            .ok_or_else(|| HostError::Encode("RenderGraph has no outputs".into()))?;
        slot.uri = Some(out.to_string_lossy().into_owned());
        if let Some(parent) = out.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
    }

    let mut options = GraphRunOptions::new();
    if let Some(dir) = mask_package {
        let host = SightloomPackageHost::open(dir).map_err(|e| HostError::Encode(e.to_string()))?;
        options = options.with_adapter_host(Arc::new(host));
    }
    run_render_graph_with(&graph, &WriteControl::default(), &options)
        .map_err(|e| HostError::Encode(e.to_string()))?;

    let written = graph
        .outputs
        .iter()
        .filter_map(|o| o.uri.as_deref())
        .map(Path::new)
        .find(|p| p.is_file())
        .ok_or_else(|| HostError::Encode("encode produced no output file".into()))?;
    Ok(written.to_string_lossy().into_owned())
}
