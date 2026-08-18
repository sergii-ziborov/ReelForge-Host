//! Live / lavfi grab without a webcam.

use reelforge_host::{
    grab_source, is_capture_token, is_lavfi_token, is_live_token, materialize_video,
};

#[test]
fn tokens() {
    assert!(is_live_token("cam"));
    assert!(is_live_token("LIVE"));
    assert!(is_lavfi_token("lavfi:testsrc=size=64x64:rate=10"));
    assert!(is_capture_token("capture:ses_1"));
    assert!(!is_live_token("scene.mp4"));
}

#[test]
fn lavfi_grab_writes_mp4() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("live.mp4");
    grab_source("lavfi:testsrc=size=160x120:rate=10", &dest, 0.4).unwrap();
    assert!(dest.is_file());
    assert!(dest.metadata().unwrap().len() > 0);
}

#[test]
fn materialize_lavfi_token() {
    let dir = tempfile::tempdir().unwrap();
    let src = std::path::Path::new("lavfi:color=c=red:s=80x60:r=10");
    let out = materialize_video(src, dir.path(), 0.3).unwrap();
    assert_eq!(out.file_name().unwrap(), "live.mp4");
    assert!(out.is_file());
}
