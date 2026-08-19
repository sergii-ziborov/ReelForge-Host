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
