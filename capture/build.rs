//! The optional `linux-kwin-geometry` feature links a minimal Wayland client
//! (wl_output and xdg-output only) for desktop geometry. Without it the
//! provider reports geometry as unsupported.
use std::{env, path::PathBuf, process::Command};

const ADAPTER: &str = "src/platform/wayland_outputs.c";
const HEADER: &str = "src/platform/wayland_outputs.h";
const XDG_OUTPUT: &str = "protocols/xdg-output-unstable-v1.xml";

fn main() {
    // The published SDK pin predates typed registration. The isolated draft
    // SDK build opts in explicitly; normal/all-feature published builds keep
    // their existing registration until that pin can be advanced atomically.
    println!("cargo:rustc-check-cfg=cfg(amaterasu_typed_native)");
    println!("cargo:rerun-if-env-changed=AMATERASU_CAPTURE_TYPED_NATIVE");
    if env::var("AMATERASU_CAPTURE_TYPED_NATIVE").as_deref() == Ok("1") {
        println!("cargo:rustc-cfg=amaterasu_typed_native");
    }
    build_gpu_snapshot();
    println!("cargo:rustc-check-cfg=cfg(amaterasu_kwin_geometry)");
    println!("cargo:rerun-if-changed={ADAPTER}");
    println!("cargo:rerun-if-changed={HEADER}");
    println!("cargo:rerun-if-changed={XDG_OUTPUT}");
    let enabled = env::var_os("CARGO_FEATURE_LINUX_KWIN_GEOMETRY").is_some();
    let linux = env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux");
    if !enabled || !linux {
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let mut build = cc::Build::new();
    for (mode, extension) in [("client-header", "h"), ("private-code", "c")] {
        let destination = out.join(format!("xdg-output-unstable-v1.{extension}"));
        let generated = Command::new("wayland-scanner")
            .arg(mode)
            .arg(XDG_OUTPUT)
            .arg(&destination)
            .status()
            .expect("linux-kwin-geometry requires wayland-scanner");
        assert!(generated.success(), "xdg-output protocol generation failed");
    }
    let flags = Command::new("pkg-config")
        .args(["--cflags", "--libs", "wayland-client"])
        .output()
        .expect("linux-kwin-geometry requires pkg-config");
    assert!(
        flags.status.success(),
        "linux-kwin-geometry requires Wayland client development files"
    );
    for flag in String::from_utf8(flags.stdout)
        .expect("pkg-config UTF-8 flags")
        .split_whitespace()
    {
        if let Some(path) = flag.strip_prefix("-I") {
            build.include(path);
        } else if let Some(library) = flag.strip_prefix("-l") {
            println!("cargo:rustc-link-lib={library}");
        } else if let Some(path) = flag.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={path}");
        } else {
            build.flag(flag);
        }
    }
    build
        .include(&out)
        .file(out.join("xdg-output-unstable-v1.c"))
        .file(ADAPTER)
        .flag("-std=c11")
        .warnings(true)
        .compile("amaterasu_vkms_wayland_outputs");
    println!("cargo:rustc-cfg=amaterasu_kwin_geometry");
}

fn build_gpu_snapshot() {
    println!("cargo:rustc-check-cfg=cfg(amaterasu_vkms_gpu_snapshot)");
    println!("cargo:rerun-if-changed=src/platform/gpu_snapshot.c");
    println!("cargo:rerun-if-changed=src/platform/gpu_snapshot.h");
    if env::var_os("CARGO_FEATURE_LINUX_GPU_SNAPSHOT").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
    {
        return;
    }
    let output = Command::new("pkg-config")
        .args(["--cflags", "--libs", "libdrm", "gbm", "egl", "glesv2"])
        .output()
        .expect("linux-gpu-snapshot requires pkg-config");
    assert!(
        output.status.success(),
        "linux-gpu-snapshot requires libdrm, GBM, EGL, and GLES development libraries"
    );
    let mut build = cc::Build::new();
    build.file("src/platform/gpu_snapshot.c").flag("-std=c11");
    for flag in String::from_utf8(output.stdout)
        .expect("pkg-config UTF-8 flags")
        .split_whitespace()
    {
        if let Some(path) = flag.strip_prefix("-I") {
            build.include(path);
        } else if let Some(library) = flag.strip_prefix("-l") {
            println!("cargo:rustc-link-lib={library}");
        } else if let Some(path) = flag.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={path}");
        } else {
            build.flag(flag);
        }
    }
    build.warnings(true).compile("amaterasu_vkms_gpu_snapshot");
    println!("cargo:rustc-cfg=amaterasu_vkms_gpu_snapshot");
}
