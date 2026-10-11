//! MCP protocol — no ONNX, no ffmpeg.

use reelforge_host::{
    HostService, MCP_PROTOCOL_VERSION, METHODS, dispatch, handle_jsonrpc, list_methods,
};

#[test]
fn methods_include_ingest_photo_run() {
    let listed = list_methods();
    for need in [
        "ingest_video",
        "enroll_photo",
        "search_photo",
        "rewrite_plan",
        "resolve_bridge",
        "compile_scene",
        "run_graph",
        "privacy_except",
    ] {
        assert!(listed.contains(&need), "missing {need}");
    }
    assert!(!listed.contains(&"compile_plan"));
    assert_eq!(listed, METHODS);
}

#[test]
fn jsonrpc_initialize_and_tools_list() {
    let mut svc = HostService::new();
    let init = handle_jsonrpc(
        &mut svc,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    )
    .unwrap();
    assert_eq!(init["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
    assert_eq!(init["result"]["serverInfo"]["name"], "reelforge-host");

    let listed = handle_jsonrpc(
        &mut svc,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    )
    .unwrap();
    let tools = listed["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"ingest_video"));
    assert!(names.contains(&"search_photo"));
    assert!(names.contains(&"run_graph"));
    assert!(names.contains(&"compile_scene"));
    assert!(names.contains(&"privacy_except"));
    let privacy = tools
        .iter()
        .find(|t| t["name"] == "privacy_except")
        .unwrap();
    let required = privacy["inputSchema"]["required"].as_array().unwrap();
    for need in ["video", "photo", "output"] {
        assert!(
            required.iter().any(|v| v == need),
            "privacy_except schema missing {need}: {privacy}"
        );
    }
    assert_eq!(
        privacy["inputSchema"]["properties"]["style"]["default"],
        "pixelate"
    );

    let unknown = handle_jsonrpc(
        &mut svc,
        r#"{"jsonrpc":"2.0","id":3,"method":"compile_plan"}"#,
    )
    .unwrap();
    assert_eq!(unknown["error"]["code"], -32601);
}

fn scene_request() -> serde_json::Value {
    serde_json::json!({
        "scene": {
            "id": "ad-intro", "name": "Intro",
            "parts": [{
                "id": "picture", "source": {"media": "accepted-art"},
                "pivot": {"x": 12.0, "y": 6.0}
            }],
            "motion": {"channels": [{
                "part": "picture", "property": "position_x",
                "keys": [
                    {"t": {"ticks": 0, "timescale": 24}, "value": 0.0},
                    {"t": {"ticks": 72, "timescale": 24}, "value": 120.0}
                ]
            }]}
        },
        "media": [{"id": "accepted-art", "uri": "not-opened-by-compiler.png"}],
        "canvas": {"width": 1280, "height": 720,
            "duration": {"ticks": 72, "timescale": 24}}
    })
}

#[test]
fn scene_compile_through_mcp_preserves_editable_motion_and_declared_hold() {
    let request = serde_json::json!({
        "jsonrpc": "2.0", "id": 17, "method": "tools/call",
        "params": {"name": "compile_scene", "arguments": scene_request()}
    });
    let reply = handle_jsonrpc(&mut HostService::new(), &request.to_string()).unwrap();
    assert!(reply.get("error").is_none(), "{reply}");
    let out = &reply["result"]["structuredContent"];
    assert_eq!(out["scene_id"], "ad-intro");
    assert_eq!(out["visible_parts"], 1);
    assert_eq!(out["graph"]["assets"][0]["duration"], out["duration"]);
    let graph: reelforge::RenderGraph = serde_json::from_value(out["graph"].clone()).unwrap();
    reelforge::compile_graph(&graph, &reelforge::OperationRegistry::with_builtins()).unwrap();
    let compose = graph
        .nodes
        .iter()
        .find(|node| {
            matches!(
                &node.body, reelforge::RenderNodeKind::Op {operation, ..}
                    if operation.as_str() == "rf.compose.layers"
            )
        })
        .unwrap();
    let reelforge::RenderNodeKind::Op { params, .. } = &compose.body else {
        unreachable!()
    };
    assert_eq!(params["layers"][0]["pivot"]["x"], 12.0);
    assert_eq!(params["layers"][0]["x"]["keys"][1]["value"], 120.0);
    assert!(graph.outputs[0].uri.is_none());
}

#[test]
fn scene_compile_refuses_invalid_documents_and_unimplemented_masks() {
    for (pointer, value) in [
        ("/scene/version", serde_json::json!(999)),
        ("/scene/parts/0/source/media", serde_json::json!("missing")),
        ("/canvas/width", serde_json::json!(0)),
        ("/canvas/duration/ticks", serde_json::json!(0)),
        ("/canvas/duration/timescale", serde_json::json!(0)),
    ] {
        let mut args = scene_request();
        if pointer == "/scene/version" {
            args["scene"]["version"] = value;
        } else {
            *args.pointer_mut(pointer).unwrap() = value;
        }
        assert!(
            dispatch(&mut HostService::new(), "compile_scene", &args).is_err(),
            "{args}"
        );
    }
    let mut args = scene_request();
    args["scene"]["parts"][0]["source"]["mask"] = serde_json::json!("not-bound");
    let err = dispatch(&mut HostService::new(), "compile_scene", &args).unwrap_err();
    assert!(err.to_string().contains("mask"), "{err}");
}

#[test]
fn rewrite_plan_rewrites_frame_pick() {
    let mut svc = HostService::new();
    let args = serde_json::json!({
        "plan": {
            "version": 2,
            "media": "scene.mp4",
            "edits": [{
                "type": "blur_everyone_except",
                "allowed": {
                    "kind": "frame_pick",
                    "media": "alice.jpg",
                    "frame_index": 0,
                    "box_xyxy": [0.0, 0.0, 10.0, 10.0]
                }
            }]
        },
        "bindings": [{
            "media": "alice.jpg",
            "frame_index": 0,
            "box_xyxy": [0.0, 0.0, 10.0, 10.0],
            "ids": [1]
        }]
    });
    let out = dispatch(&mut svc, "rewrite_plan", &args).unwrap();
    assert_eq!(out["edits"][0]["allowed"]["kind"], "subject_ids");
    assert_eq!(out["edits"][0]["allowed"]["ids"][0], 1);
}
