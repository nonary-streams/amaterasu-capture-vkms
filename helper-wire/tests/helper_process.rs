//! Runs the real helper binary without capabilities. These tests never ask it
//! to open a DRM device: the only OPEN_CARD uses a non-DRM major number,
//! which the helper rejects before touching the filesystem.
#![cfg(target_os = "linux")]

use amaterasu_vkms_frame_helper::{client::ClientError, process::HelperProcess, protocol::Failure};
use std::fs::File;

fn running_as_root() -> bool {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() == 0 }
}

fn spawn_helper() -> HelperProcess {
    let executable = File::open(env!("CARGO_BIN_EXE_amaterasu-vkms-frame-helper")).unwrap();
    HelperProcess::spawn(executable).unwrap()
}

#[test]
fn an_unprivileged_helper_reports_missing_admin_and_refuses_frames() {
    if running_as_root() {
        return;
    }
    let mut helper = spawn_helper();
    let hello = helper.client().hello().unwrap();
    assert!(
        !hello.admin_permitted,
        "test runner should hold no file capability"
    );

    let not_open = helper.client().frame(41).unwrap_err();
    assert_eq!(not_open.failure(), Some(Failure::CardNotOpen));

    let rejected = helper.client().open_card(8, 0).unwrap_err();
    assert_eq!(rejected.failure(), Some(Failure::DeviceRejected));
    assert!(helper.client().is_usable());
}

#[test]
fn a_program_that_exits_at_startup_is_explained() {
    let mut helper = HelperProcess::spawn(File::open("/bin/false").unwrap()).unwrap();
    assert!(matches!(helper.client().hello(), Err(ClientError::Closed)));
    assert_eq!(
        helper.startup_refusal(),
        "the frame helper exited during startup"
    );
}
