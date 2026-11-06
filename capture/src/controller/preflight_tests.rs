use super::*;
use crate::test_support::{FakeBackend, settings};
use amaterasu_module_sdk::capture::{SourceClass, SourceMode, SourcePreflightRequest};
use std::sync::atomic::Ordering;

fn request(route: Option<&str>) -> SourcePreflightRequest {
    SourcePreflightRequest {
        source_class: SourceClass::ManagedOutput,
        route: route.map(str::to_owned),
        mode: SourceMode {
            width: 1920,
            height: 1080,
            refresh_millihz: 60_000,
            hdr: false,
            bit_depth: Some(8),
        },
    }
}
fn approved() -> Settings {
    Settings {
        authority_channel: true,
        authority_precheck_supported: true,
        authority_channel_fd: Some(198),
        frame_helper: None,
        ..settings()
    }
}

#[test]
fn class_query_accepts_disconnected_pool_without_starting_helpers_or_capture() {
    let backend = FakeBackend::new();
    let handles = backend.handles();
    for connector in handles.connectors.lock().unwrap().as_mut().unwrap() {
        connector.connection = crate::discovery::Connection::Disconnected;
        connector.crtc = None;
    }
    let controller = Controller::new(backend, approved());
    let response = controller.preflight("amaterasu.video", &request(None));
    assert!(response.is_ready());
    response.validate(&request(None)).unwrap();
    assert_eq!(handles.pure_prechecks.load(Ordering::SeqCst), 1);
    assert_eq!(handles.helper_starts.load(Ordering::SeqCst), 0);
    assert!(handles.connects.lock().unwrap().is_empty());
    assert!(controller.streams.is_empty());
    assert_eq!(controller.opened, 0);
    assert!(
        !controller
            .preflight("amaterasu.video", &request(Some("kwin:Virtual-9")))
            .is_ready()
    );
}

#[test]
fn exact_query_checks_frozen_mode_and_no_legacy_helper_fallback_exists() {
    let backend = FakeBackend::new();
    let handles = backend.handles();
    let controller = Controller::new(backend, approved());
    let mut query = request(Some("kwin:Virtual-9"));
    assert!(controller.preflight("amaterasu.video", &query).is_ready());
    query.mode.width = 1280;
    assert!(!controller.preflight("amaterasu.video", &query).is_ready());
    query.mode.width = 1920;
    query.source_class = SourceClass::DesktopOutput;
    assert!(!controller.preflight("amaterasu.video", &query).is_ready());
    assert!(!controller.preflight("other", &request(None)).is_ready());
    let starts = handles.helper_starts.load(Ordering::SeqCst);
    let legacy = Controller::new(handles.handles(), settings());
    assert_eq!(
        legacy
            .preflight("amaterasu.video", &request(None))
            .reason
            .as_deref(),
        Some("legacy_preflight_unsupported")
    );
    assert_eq!(handles.helper_starts.load(Ordering::SeqCst), starts);
    assert!(handles.connects.lock().unwrap().is_empty());
}

#[test]
fn source_hdr_requires_the_real_guarded_color_path() {
    let backend = FakeBackend::new();
    let handles = backend.handles();
    let controller = Controller::new(backend, approved());
    let mut query = request(None);
    query.mode.hdr = true;
    query.mode.bit_depth = Some(10);
    assert!(!controller.preflight("amaterasu.video", &query).is_ready());
    assert_eq!(handles.pure_prechecks.load(Ordering::SeqCst), 0);
    assert_eq!(handles.helper_starts.load(Ordering::SeqCst), 0);
}

#[test]
fn old_protected_launcher_is_rejected_without_sending_an_unknown_opcode() {
    let backend = FakeBackend::new();
    let handles = backend.handles();
    let mut config = approved();
    config.authority_precheck_supported = false;
    let controller = Controller::new(backend, config);
    assert_eq!(
        controller
            .preflight("amaterasu.video", &request(None))
            .reason
            .as_deref(),
        Some("authority_precheck_unsupported")
    );
    assert_eq!(handles.pure_prechecks.load(Ordering::SeqCst), 0);
    assert_eq!(handles.helper_starts.load(Ordering::SeqCst), 0);
    assert!(handles.connects.lock().unwrap().is_empty());
}
