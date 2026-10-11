//! Turn object trajectories, delayed followers and events into native scene keys.

use super::model::{Layer, Plan, Pose, State};
use crate::{HostError, Result};
use reelforge::{
    MediaTime, MotionChannel, MotionKey, MotionProperty, SceneDocument, ScenePart, ScenePoint,
    SourceLink,
};

pub(super) fn sample(path: &[Pose], at: f64) -> Pose {
    let first = path[0];
    if at <= first.at {
        return Pose { at, ..first };
    }
    let last = path[path.len() - 1];
    for pair in path.windows(2) {
        if at <= pair[1].at {
            let a = pair[0];
            let b = pair[1];
            let u = (at - a.at) / (b.at - a.at);
            let mix = |x: f64, y: f64| x + (y - x) * u;
            return Pose {
                at,
                x: mix(a.x, b.x),
                y: mix(a.y, b.y),
                scale: mix(a.scale, b.scale),
                rotation: mix(a.rotation, b.rotation),
                opacity: mix(a.opacity, b.opacity),
            };
        }
    }
    Pose { at, ..last }
}

pub(super) fn pose(plan: &Plan, layer: &Layer, at: f64) -> Pose {
    let mut own = sample(&layer.path, at);
    if let Some(follow) = &layer.follow {
        let target = plan
            .layers
            .iter()
            .find(|l| l.id == follow.target)
            .expect("validated target");
        let earlier = sample(&target.path, at - follow.delay);
        own.x = earlier.x;
        own.y = earlier.y;
        own.opacity *= ((at - follow.delay) / 0.12).clamp(0.0, 1.0);
    }
    for event in &plan.events {
        if event.kind == "collect" && event.target == layer.id && at >= event.at {
            let u = ((at - event.at) / 0.16).clamp(0.0, 1.0);
            own.scale *= 1.0 + u * 1.5;
            own.opacity *= 1.0 - u;
        }
        if event.kind == "portal_open" && event.target == layer.id {
            let u = ((at - event.at) / 0.35).clamp(0.0, 1.0);
            own.opacity *= 0.14 + u * 0.86;
            own.scale *= 0.6 + 0.4 * u;
        }
        if event.kind == "exit" && event.actor == layer.id && at >= event.at {
            let u = ((at - event.at) / 0.35).clamp(0.0, 1.0);
            own.scale *= 1.0 - 0.97 * u;
            own.opacity *= 1.0 - u;
        }
    }
    own
}

pub(super) fn compile(state: &State) -> Result<SceneDocument> {
    let plan = state
        .plan
        .as_ref()
        .ok_or_else(|| HostError::message("plan missing"))?;
    let mut scene = SceneDocument::new(&state.request.id, &plan.title);
    for layer in &plan.layers {
        let receipt = state
            .assets
            .get(&layer.asset)
            .ok_or_else(|| HostError::message("asset not accepted"))?;
        let mut part = ScenePart::new(&layer.id, SourceLink::media(&layer.asset));
        part.pivot = ScenePoint::new(
            f64::from(receipt.width) / 2.0,
            f64::from(receipt.height) / 2.0,
        );
        part.order = layer.order;
        let scale = layer.width / f64::from(receipt.width);
        append(&mut scene, part, scale, state.request.duration, |t| {
            pose(plan, layer, t)
        });
        for index in 0..layer.trail_samples {
            let age = (f64::from(index) + 1.0) / f64::from(layer.trail_samples);
            let mut puff = ScenePart::new(
                format!("{}-tail-{index}", layer.id),
                SourceLink::media(&layer.asset),
            );
            puff.pivot = ScenePoint::new(
                f64::from(receipt.width) / 2.0,
                f64::from(receipt.height) / 2.0,
            );
            puff.order = layer.order - 1;
            append(&mut scene, puff, scale, state.request.duration, |t| {
                let lag = age * 0.65;
                let mut p = pose(plan, layer, (t - lag).max(0.0));
                p.at = t;
                p.scale *= 0.70 * (1.0 - age * 0.75);
                p.opacity *= 0.30 * (1.0 - age).powf(1.3) * ((t - lag) / 0.10).clamp(0.0, 1.0);
                p
            });
        }
    }
    bursts(&mut scene, state, plan);
    scene
        .validate()
        .map_err(|e| HostError::message(e.to_string()))?;
    Ok(scene)
}

fn bursts(scene: &mut SceneDocument, state: &State, plan: &Plan) {
    for event in plan
        .events
        .iter()
        .filter(|e| e.kind == "collect" || e.kind == "exit")
    {
        let target = plan
            .layers
            .iter()
            .find(|l| l.id == event.target)
            .expect("validated target");
        let center = pose(plan, target, event.at);
        let receipt = &state.assets[&target.asset];
        for index in 0..8 {
            let angle = f64::from(index) * std::f64::consts::TAU / 8.0;
            let mut part = ScenePart::new(
                format!("{}-burst-{index}", event.id),
                SourceLink::media(&target.asset),
            );
            part.pivot = ScenePoint::new(
                f64::from(receipt.width) / 2.0,
                f64::from(receipt.height) / 2.0,
            );
            part.order = target.order + 5;
            let scale = target.width * 0.12 / f64::from(receipt.width);
            append(scene, part, scale, state.request.duration, |t| {
                let age = t - event.at;
                let u = (age / 0.5).clamp(0.0, 1.0);
                let reach = 120.0 * (1.0 - (1.0 - u).powi(2));
                Pose {
                    at: t,
                    x: center.x + reach * angle.cos(),
                    y: center.y + reach * angle.sin(),
                    scale: 1.0 - 0.6 * u,
                    rotation: u * 160.0,
                    opacity: if age < 0.0 { 0.0 } else { (1.0 - u).powi(2) },
                }
            });
        }
    }
}

fn append(
    scene: &mut SceneDocument,
    part: ScenePart,
    scale: f64,
    duration: f64,
    at: impl Fn(f64) -> Pose,
) {
    let id = part.id.clone();
    // Native placement translates the source origin, then rotates/scales about
    // its unscaled pivot. Movie poses specify the pivot's canvas position.
    let pivot = part.pivot;
    scene.parts.push(part);
    // A complete deterministic 60 Hz motion lattice is independent of export fps.
    // Native interpolation remains available between these saved control keys.
    let count = u32::try_from((duration * 60.0).ceil() as i64).expect("validated duration");
    let samples: Vec<_> = (0..=count)
        .map(|i| {
            let t = (f64::from(i) / 60.0).min(duration);
            at(t)
        })
        .collect();
    for property in [
        MotionProperty::PositionX,
        MotionProperty::PositionY,
        MotionProperty::Scale,
        MotionProperty::Rotation,
        MotionProperty::Opacity,
    ] {
        scene.motion.channels.push(MotionChannel {
            part: id.clone(),
            property,
            keys: samples
                .iter()
                .map(|p| MotionKey {
                    t: MediaTime::new((p.at * 1_000_000.0).round() as i64, 1_000_000)
                        .expect("bounded clock"),
                    value: match property {
                        MotionProperty::PositionX => p.x - pivot.x,
                        MotionProperty::PositionY => p.y - pivot.y,
                        MotionProperty::Scale => p.scale * scale,
                        MotionProperty::Rotation => p.rotation,
                        MotionProperty::Opacity => p.opacity,
                    },
                })
                .collect(),
        });
    }
}

pub(super) fn action_report(state: &State) -> Result<serde_json::Value> {
    let plan = state
        .plan
        .as_ref()
        .ok_or_else(|| HostError::message("plan missing"))?;
    let mut report = Vec::new();
    for event in &plan.events {
        let actor = plan
            .layers
            .iter()
            .find(|l| l.id == event.actor)
            .expect("validated actor");
        let target = plan
            .layers
            .iter()
            .find(|l| l.id == event.target)
            .expect("validated target");
        let a = pose(plan, actor, event.at);
        let b = pose(plan, target, event.at);
        let distance = (a.x - b.x).hypot(a.y - b.y);
        let limit = (actor.width * a.scale + target.width * b.scale) * 0.5;
        if ["collect", "exit"].contains(&event.kind.as_str()) && distance > limit {
            return Err(HostError::message(format!(
                "event {} has no physical contact: {distance:.1} > {limit:.1}",
                event.id
            )));
        }
        report.push(serde_json::json!({"id":event.id,"kind":event.kind,"at":event.at,"distance":distance,"contact_limit":limit}));
    }
    let actors:Vec<_>=plan.layers.iter().filter(|l| l.role=="actor").map(|layer| {
        let (travel, visible) = visible_motion(plan, layer, state);
        serde_json::json!({"id":layer.id,"visible_travel_pixels":travel,"visible_samples":visible,"delayed_follower":layer.follow})
    }).collect();
    Ok(serde_json::json!({"events":report,"actors":actors,"static_slideshow_allowed":false}))
}

pub(super) fn visible_motion(plan: &Plan, layer: &Layer, state: &State) -> (f64, usize) {
    let samples: Vec<_> = (0..=100)
        .map(|i| pose(plan, layer, state.request.duration * f64::from(i) / 100.0))
        .collect();
    let visible = |p: &Pose| {
        p.opacity > 0.1
            && layer.width * p.scale >= 8.0
            && p.x >= 0.0
            && p.x < f64::from(state.request.width)
            && p.y >= 0.0
            && p.y < f64::from(state.request.height)
    };
    let count = samples.iter().filter(|p| visible(p)).count();
    let travel = samples
        .windows(2)
        .filter(|p| visible(&p[0]) && visible(&p[1]))
        .map(|p| (p[1].x - p[0].x).hypot(p[1].y - p[0].y))
        .sum();
    (if count >= 10 { travel } else { 0.0 }, count)
}
