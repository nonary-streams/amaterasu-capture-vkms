//! Checks an operator-installed helper against the real vibeshine_drm card.
//!
//! Run after installing and granting the helper:
//! `cargo test -p amaterasu-vkms-frame-helper --test installed_helper -- --ignored`
//! `AMATERASU_FRAME_HELPER` overrides the default installed path.
#![cfg(target_os = "linux")]

use amaterasu_vkms_frame_helper::{
    process::{HelperProcess, open_verified_helper},
    protocol::Failure,
};
use std::path::{Path, PathBuf};

const INSTALLED_HELPER: &str = "/usr/local/libexec/amaterasu/amaterasu-vkms-frame-helper";

/// The `major:minor` of the first DRM card whose device is the vibeshine faux
/// device, read from sysfs.
fn vibeshine_card() -> (u32, u32) {
    let cards = std::fs::read_dir("/sys/class/drm").expect("sysfs DRM class");
    for entry in cards.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_card = name.starts_with("card") && !name.contains('-');
        let device = std::fs::canonicalize(entry.path().join("device")).unwrap_or_default();
        if is_card && device.file_name().is_some_and(|name| name == "vibeshine") {
            let number = std::fs::read_to_string(entry.path().join("dev")).unwrap();
            let (major, minor) = number.trim().split_once(':').unwrap();
            return (major.parse().unwrap(), minor.parse().unwrap());
        }
    }
    panic!("no vibeshine_drm card is present");
}

#[test]
#[ignore = "requires the installed, setcap'd helper and the vibeshine_drm driver"]
fn installed_helper_reaches_the_privileged_driver_ioctls() {
    let path = std::env::var_os("AMATERASU_FRAME_HELPER")
        .map(PathBuf::from)
        .unwrap_or_else(|| INSTALLED_HELPER.into());
    let executable = open_verified_helper(Path::new(&path)).expect("verified helper file");
    let mut helper = HelperProcess::spawn(executable).expect("spawn helper");
    let hello = helper.client().hello().unwrap_or_else(|error| {
        panic!("{error:?}: {}", helper.startup_refusal());
    });
    assert!(
        hello.admin_permitted,
        "run: sudo setcap cap_sys_admin+p {path:?}"
    );

    let (major, minor) = vibeshine_card();
    helper
        .client()
        .open_card(major, minor)
        .expect("open vibeshine_drm card");

    // The driver checks CAP_SYS_ADMIN before it looks up the CRTC, so a
    // missing CRTC (rather than permission_denied) proves the raised
    // capability reached the private ioctl without any output being active.
    let refused = helper
        .client()
        .frame(u32::MAX)
        .expect_err("no CRTC has this id");
    assert_eq!(
        refused.failure(),
        Some(Failure::CrtcNotFound),
        "{refused:?}"
    );
}
