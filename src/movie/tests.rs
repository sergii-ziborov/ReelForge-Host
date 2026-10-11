use super::{graph, model::*, motion, planner, store};
use std::collections::BTreeMap;

fn pose(at: f64, x: f64, y: f64) -> Pose {
    Pose {
        at,
        x,
        y,
        scale: 1.0,
        rotation: 0.0,
        opacity: 1.0,
    }
}

fn state() -> State {
    let asset = Asset {
        id: "orb".into(),
        kind: "glow".into(),
        prompt: String::new(),
        transparent: true,
        reference_ids: vec![],
        color: [90, 220, 255, 255],
        text: String::new(),
    };
    let actor = Layer {
        id: "hero".into(),
        asset: "orb".into(),
        role: "actor".into(),
        width: 24.0,
        order: 1,
        path: vec![
            pose(0.0, 30.0, 50.0),
            pose(1.0, 120.0, 30.0),
            pose(2.0, 240.0, 140.0),
        ],
        follow: None,
        trail_samples: 0,
    };
    let echo = Layer {
        id: "echo".into(),
        asset: "orb".into(),
        role: "actor".into(),
        width: 24.0,
        order: 2,
        path: vec![pose(0.0, 0.0, 0.0), pose(2.0, 0.0, 0.0)],
        follow: Some(Follow {
            target: "hero".into(),
            delay: 0.25,
        }),
        trail_samples: 4,
    };
    State {
        request: Create {
            id: "fixture".into(),
            prompt: "Two actors chase".into(),
            duration: 2.0,
            width: 320,
            height: 180,
            fps: 24,
            max_images: 10,
            images_already_used: 0,
            references: BTreeMap::new(),
        },
        revision: 0,
        status: "running".into(),
        stage: "assets".into(),
        error: None,
        images_used: 0,
        plan: Some(Plan {
            title: "test".into(),
            synopsis: "chase".into(),
            assets: vec![asset],
            layers: vec![actor, echo],
            events: vec![],
        }),
        assets: BTreeMap::new(),
        changed_nodes: vec![],
        invalidated_nodes: vec![],
        output: None,
        qc: None,
        graph: None,
        pending_generation: None,
    }
}

#[test]
fn follower_replays_the_actual_earlier_path_at_fractional_times() {
    let state = state();
    let plan = state.plan.as_ref().unwrap();
    for t in [0.31, 0.517, 1.02, 1.375, 1.91] {
        let lead = motion::pose(plan, &plan.layers[0], t - 0.25);
        let follower = motion::pose(plan, &plan.layers[1], t);
        assert!((lead.x - follower.x).abs() < 1e-12);
        assert!((lead.y - follower.y).abs() < 1e-12);
    }
    assert!(motion::pose(plan, &plan.layers[1], 0.1).opacity.abs() < f64::EPSILON);
}

#[test]
fn asset_change_invalidates_its_parts_scene_export_and_quality() {
    let mut state = state();
    let old = graph::build(&state).unwrap();
    state.plan.as_mut().unwrap().assets[0].color[0] = 11;
    let new = graph::build(&state).unwrap();
    let (changed, affected) = graph::changes(&old, &new);
    assert!(changed.contains(&"asset/orb".into()));
    for id in ["layer/hero", "layer/echo", "scene", "render", "qc"] {
        assert!(affected.contains(&id.into()), "{id}");
    }
    assert!(!affected.contains(&"prompt".into()));
}

#[test]
fn slideshow_and_impossible_contact_are_rejected() {
    let mut state = state();
    for layer in &mut state.plan.as_mut().unwrap().layers {
        layer.follow = None;
        layer.path = vec![pose(0.0, 50.0, 50.0), pose(2.0, 50.0, 50.0)];
    }
    assert!(
        planner::validate(state.plan.as_ref().unwrap(), &state)
            .unwrap_err()
            .to_string()
            .contains("slideshow")
    );
    let mut state = super::tests::state();
    state.plan.as_mut().unwrap().events.push(ActionEvent {
        id: "bad-contact".into(),
        kind: "collect".into(),
        at: 1.9,
        actor: "hero".into(),
        target: "echo".into(),
    });
    assert!(
        motion::action_report(&state)
            .unwrap_err()
            .to_string()
            .contains("physical contact")
    );
}

#[test]
fn journal_reopens_exact_revision_and_refuses_a_stale_writer() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = state();
    store::commit(dir.path(), &mut state, "production.created").unwrap();
    let mut previous = store::load(dir.path()).unwrap();
    state.images_used = 1;
    store::commit(dir.path(), &mut state, "image.reserved").unwrap();
    assert!(store::commit(dir.path(), &mut previous, "invalid.stale").is_err());
    let reopened = store::load(dir.path()).unwrap();
    assert_eq!(reopened.revision, 2);
    assert_eq!(reopened.images_used, 1);
}

#[test]
fn native_animation_renders_and_reuses_assets_without_provider_calls() {
    let dir = tempfile::Builder::new()
        .prefix("fixture-")
        .tempdir()
        .unwrap();
    let mut state = state();
    store::commit(dir.path(), &mut state, "production.created").unwrap();
    super::assets::prepare(dir.path(), &mut state).unwrap();
    let receipts = serde_json::to_value(&state.assets).unwrap();
    let revision = state.revision;
    super::assets::prepare(dir.path(), &mut state).unwrap();
    assert_eq!(state.revision, revision);
    assert_eq!(serde_json::to_value(&state.assets).unwrap(), receipts);
    assert_eq!(state.images_used, 0);
    super::render::render(dir.path(), &mut state).unwrap();
    let frame = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-ss", "1", "-i"])
        .arg(state.output.as_ref().unwrap())
        .args(["-frames:v", "1", "-f", "image2pipe", "-vcodec", "png", "-"])
        .output()
        .unwrap();
    assert!(frame.status.success());
    let picture = image::load_from_memory(&frame.stdout).unwrap().to_rgb8();
    // Delivered pixels, not a motion manifest: the glow must land at its
    // authored center after source-pivot placement and downscaling.
    let pixel = picture.get_pixel(120, 30).0;
    assert!(
        pixel[2] > 100 && pixel[1] > 80,
        "actor missed its authored center: {pixel:?}"
    );
    assert_eq!(state.qc.as_ref().unwrap()["frames"], 48);
    assert_eq!(
        state.qc.as_ref().unwrap()["actions"]["actors"][0]["id"],
        "hero"
    );
    assert!(
        state.qc.as_ref().unwrap()["actions"]["actors"][0]["visible_travel_pixels"]
            .as_f64()
            .unwrap()
            > 200.0
    );
    state.graph = Some(graph::build(&state).unwrap());
    state.status = "completed".into();
    store::commit(dir.path(), &mut state, "production.finished").unwrap();
    let root = dir.path().parent().unwrap();
    let id = dir.path().file_name().unwrap().to_str().unwrap();
    super::video(root, id).unwrap();
    std::fs::write(&state.assets["orb"].path, b"changed bytes").unwrap();
    assert!(
        super::video(root, id)
            .unwrap_err()
            .to_string()
            .contains("changed")
    );
}

#[test]
fn hidden_or_offscreen_motion_does_not_qualify_as_animation() {
    let mut state = state();
    for layer in &mut state.plan.as_mut().unwrap().layers {
        for key in &mut layer.path {
            key.opacity = 0.0;
        }
    }
    assert!(planner::validate(state.plan.as_ref().unwrap(), &state).is_err());
    for layer in &mut state.plan.as_mut().unwrap().layers {
        for key in &mut layer.path {
            key.opacity = 1.0;
            key.y = -2000.0;
        }
    }
    assert!(planner::validate(state.plan.as_ref().unwrap(), &state).is_err());
}

#[test]
fn exhausted_budget_rejects_plan_before_provider_launch() {
    let mut state = state();
    state.images_used = 10;
    state.plan.as_mut().unwrap().assets[0].kind = "image".into();
    assert!(
        planner::validate(state.plan.as_ref().unwrap(), &state)
            .unwrap_err()
            .to_string()
            .contains("budget")
    );
}

#[test]
fn unknown_generation_resume_keeps_reservation_and_does_not_launch_a_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = state();
    state.plan.as_mut().unwrap().assets[0].kind = "image".into();
    state.images_used = 1;
    let asset = &state.plan.as_ref().unwrap().assets[0];
    let recipe = store::digest(&serde_json::to_vec(&(asset, Vec::<String>::new())).unwrap());
    state.pending_generation = Some(PendingGeneration {
        asset_id: "orb".into(),
        recipe,
        job: "unresolved".into(),
        reference_hashes: vec![],
    });
    store::commit(dir.path(), &mut state, "image.reserved").unwrap();
    let version = state.revision;
    let error = super::assets::prepare(dir.path(), &mut state).unwrap_err();
    assert!(error.to_string().contains("unknown"));
    assert_eq!(state.images_used, 1);
    assert_eq!(state.revision, version);
    assert!(state.pending_generation.is_some());
    assert!(!dir.path().join("unresolved.events.jsonl").exists());
}

#[test]
fn generated_references_are_ordered_and_invalidate_dependent_assets() {
    let mut state = state();
    let mut dependent = state.plan.as_ref().unwrap().assets[0].clone();
    dependent.id = "dependent".into();
    dependent.reference_ids = vec!["orb".into()];
    state.plan.as_mut().unwrap().assets.insert(0, dependent);
    let ordered = graph::ordered_assets(&state).unwrap();
    assert_eq!(ordered[0].id, "orb");
    assert_eq!(ordered[1].id, "dependent");
    let old = graph::build(&state).unwrap();
    state.plan.as_mut().unwrap().assets[1].color[0] = 12;
    let new = graph::build(&state).unwrap();
    assert!(
        graph::changes(&old, &new)
            .1
            .contains(&"asset/dependent".into())
    );
    state.plan.as_mut().unwrap().assets[1].reference_ids = vec!["dependent".into()];
    assert!(graph::ordered_assets(&state).is_err());
}

#[test]
fn invalid_create_is_rejected_before_a_project_or_provider_is_started() {
    let dir = tempfile::tempdir().unwrap();
    let mut request = state().request;
    request.duration = 1.01;
    assert!(
        super::dispatch(
            dir.path(),
            "movie_create",
            &serde_json::to_value(&request).unwrap()
        )
        .is_err()
    );
    assert!(!dir.path().join(&request.id).exists());
    request.duration = 2.0;
    request.images_already_used = 11;
    assert!(
        super::dispatch(
            dir.path(),
            "movie_create",
            &serde_json::to_value(&request).unwrap()
        )
        .is_err()
    );
    assert!(!dir.path().join(&request.id).exists());
}
