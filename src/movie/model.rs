//! Production intent and durable execution receipts; SceneDocument stays native.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Create {
    pub id: String,
    pub prompt: String,
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub max_images: u32,
    #[serde(default)]
    pub images_already_used: u32,
    #[serde(default)]
    pub references: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Plan {
    pub title: String,
    pub synopsis: String,
    pub assets: Vec<Asset>,
    pub layers: Vec<Layer>,
    pub events: Vec<ActionEvent>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Asset {
    pub id: String,
    pub kind: String,
    pub prompt: String,
    pub transparent: bool,
    pub reference_ids: Vec<String>,
    pub color: [u8; 4],
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Layer {
    pub id: String,
    pub asset: String,
    pub role: String,
    pub width: f64,
    pub order: i32,
    pub path: Vec<Pose>,
    pub follow: Option<Follow>,
    pub trail_samples: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pose {
    pub at: f64,
    pub x: f64,
    pub y: f64,
    pub scale: f64,
    pub rotation: f64,
    pub opacity: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Follow {
    pub target: String,
    pub delay: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ActionEvent {
    pub id: String,
    pub kind: String,
    pub at: f64,
    pub actor: String,
    pub target: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Receipt {
    pub recipe: String,
    pub path: String,
    pub sha256: String,
    pub width: u32,
    pub height: u32,
    pub provider_events: Option<String>,
    #[serde(default)]
    pub provider_evidence: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct PendingGeneration {
    pub asset_id: String,
    pub recipe: String,
    pub job: String,
    pub reference_hashes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct State {
    pub request: Create,
    pub revision: u64,
    pub status: String,
    pub stage: String,
    pub error: Option<String>,
    pub images_used: u32,
    pub plan: Option<Plan>,
    pub assets: BTreeMap<String, Receipt>,
    pub changed_nodes: Vec<String>,
    pub invalidated_nodes: Vec<String>,
    pub output: Option<String>,
    pub qc: Option<serde_json::Value>,
    pub graph: Option<weavatrix_graph::Graph>,
    #[serde(default)]
    pub pending_generation: Option<PendingGeneration>,
}
