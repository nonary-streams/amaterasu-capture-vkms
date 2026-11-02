use super::*;
use crate::test_support::{
    FakeBackend, HELPER, ScriptedWait, appended_layout, connector, eventually, settings,
};
use amaterasu_media::DesktopRect;

const VIDEO: &str = "amaterasu.video";

fn controller() -> (Controller<FakeBackend>, FakeBackend) {
    let backend = FakeBackend::new();
    let handles = backend.handles();
    (Controller::new(backend, settings()), handles)
}

fn open(output: &str, operation: &str) -> Operation {
    Operation::Open {
        output: output.into(),
        operation_id: Some(operation.into()),
        constant_rate: None,
        resource_capture_clock: None,
        resource_callback_contract: None,
    }
}

fn opened(controller: &mut Controller<FakeBackend>) -> u64 {
    controller
        .handle(VIDEO, open("kwin:Virtual-9", "video-1"), 0)
        .unwrap()
        .created
        .unwrap()
}

#[test]
fn open_resolves_the_crtc_and_reports_compositor_geometry() {
    let (mut controller, backend) = controller();
    let reply = controller
        .handle(VIDEO, open("kwin:Virtual-9", "video-1"), 0)
        .unwrap();
    let id = reply.created.unwrap();
    let payload = reply.payload;
    assert_eq!(payload["ok"], true);
    assert_eq!(payload["stream"], id);
    assert_eq!(payload["output"], "kwin:Virtual-9");
    assert_eq!(payload["crtc_id"], 41);
    assert_eq!(payload["geometry_supported"], true);
    let output: DesktopRect =
        serde_json::from_value(payload["desktop_geometry"]["output"].clone()).unwrap();
    let desktop: DesktopRect =
        serde_json::from_value(payload["desktop_geometry"]["virtual_desktop"].clone()).unwrap();
    // Appended right of a 3024x1890 primary at scale 2.5 (1210x756 logical).
    assert_eq!(output, DesktopRect::new(1210, 0, 1920, 1080).unwrap());
    assert_eq!(desktop, DesktopRect::new(0, 0, 5050, 1080).unwrap());
    assert_eq!(
        controller.reader(VIDEO, id).unwrap().geometry,
        StreamGeometry::Ready(CaptureGeometry::new(output, desktop).unwrap())
    );
    assert_eq!(
        *backend.connects.lock().unwrap(),
        vec![(HELPER.to_owned(), 41)]
    );
}

#[test]
fn open_replays_lookup_and_close_are_caller_scoped() {
    let (mut controller, backend) = controller();
    let id = opened(&mut controller);
    let replay = controller
        .handle(VIDEO, open("kwin:Virtual-9", "video-1"), 1)
        .unwrap();
    assert_eq!(replay.created, None);
    assert_eq!(replay.payload["stream"], id);
    assert_eq!(backend.connects.lock().unwrap().len(), 1);
    assert_eq!(
        controller
            .handle(VIDEO, open("kwin:Virtual-10", "video-1"), 1)
            .err()
            .unwrap()
            .code,
        "operation_conflict"
    );
    let lookup = controller
        .handle(
            VIDEO,
            Operation::Lookup {
                operation_id: "video-1".into(),
            },
            1,
        )
        .unwrap();
    assert_eq!(lookup.payload["stream"], id);

    assert_eq!(
        controller.close("other.module", id).unwrap_err().code,
        "not_found"
    );
    let frames = controller.reader(VIDEO, id).unwrap().frames;
    controller.close(VIDEO, id).unwrap();
    assert!(frames.admission.enter().is_none());
    controller.close(VIDEO, id).unwrap();
    assert_eq!(
        controller.reader(VIDEO, id).err().unwrap().code,
        "not_found"
    );
}

#[test]
fn definite_refusals_are_error_replies_with_codes() {
    let (mut controller, backend) = controller();
    let code = |controller: &mut Controller<FakeBackend>, operation| {
        controller.handle(VIDEO, operation, 0).err().unwrap().code
    };
    assert_eq!(
        code(&mut controller, open("kwin:Virtual-0", "a")),
        "invalid_named_output"
    );
    assert_eq!(
        code(
            &mut controller,
            Operation::Open {
                output: "kwin:Virtual-9".into(),
                operation_id: None,
                constant_rate: None,
                resource_capture_clock: None,
                resource_callback_contract: None,
            }
        ),
        "operation_id_required"
    );
    assert_eq!(
        code(
            &mut controller,
            Operation::Open {
                output: "kwin:Virtual-9".into(),
                operation_id: Some("a".into()),
                constant_rate: Some(true),
                resource_capture_clock: None,
                resource_callback_contract: None,
            }
        ),
        "unsupported_constant_rate"
    );
    assert_eq!(
        code(&mut controller, open("kwin:Virtual-3", "a")),
        "output_not_found"
    );
    *backend.helper.lock().unwrap() = Err(CaptureError::permission_denied(HELPER));
    assert_eq!(
        code(&mut controller, open("kwin:Virtual-9", "a")),
        "permission_denied"
    );
    *backend.connectors.lock().unwrap() = Err(CaptureError::driver_unavailable("none"));
    assert_eq!(
        code(&mut controller, open("kwin:Virtual-9", "a")),
        "driver_unavailable"
    );
    let payload = CaptureError::code("output_not_found").payload();
    assert_eq!(payload["ok"], false);
    assert_eq!(payload["error"]["code"], "output_not_found");
}

#[test]
fn a_missing_helper_setting_is_frame_helper_unavailable() {
    let mut without_helper = settings();
    without_helper.frame_helper = None;
    let mut controller = Controller::new(FakeBackend::new(), without_helper);
    let error = controller
        .handle(VIDEO, open("kwin:Virtual-9", "a"), 0)
        .err()
        .unwrap();
    assert_eq!(error.code, "frame_helper_unavailable");

    let enumeration = controller
        .handle(
            VIDEO,
            Operation::Enumerate {
                offset: None,
                limit: None,
            },
            0,
        )
        .unwrap()
        .payload;
    assert_eq!(enumeration["availability"]["available"], false);
    assert_eq!(
        enumeration["availability"]["reason"],
        "frame_helper_unavailable"
    );
    assert_eq!(enumeration["checks"]["driver"]["ok"], true);
    assert_eq!(enumeration["outputs"].as_array().unwrap().len(), 2);
}

#[test]
fn policy_gates_open_and_enumerate() {
    let mut disabled = settings();
    disabled.enabled = false;
    let mut controller = Controller::new(FakeBackend::new(), disabled);
    assert_eq!(
        controller
            .handle(VIDEO, open("kwin:Virtual-9", "a"), 0)
            .err()
            .unwrap()
            .code,
        "capture_disabled"
    );
    let (mut controller, _) = self::controller();
    let enumerate = Operation::Enumerate {
        offset: None,
        limit: None,
    };
    assert_eq!(
        controller
            .handle("stranger", enumerate.clone(), 0)
            .err()
            .unwrap()
            .code,
        "capture_permission_denied"
    );
    assert_eq!(
        controller
            .handle(
                VIDEO,
                Operation::Enumerate {
                    offset: Some(1),
                    limit: None
                },
                0
            )
            .err()
            .unwrap()
            .code,
        "invalid_request"
    );
}

#[test]
fn enumeration_reports_permission_and_driver_reasons() {
    let (mut controller, backend) = controller();
    let enumerate = Operation::Enumerate {
        offset: None,
        limit: None,
    };
    let ready = controller
        .handle(VIDEO, enumerate.clone(), 0)
        .unwrap()
        .payload;
    assert_eq!(ready["availability"]["available"], true);
    assert_eq!(ready["outputs"][0]["id"], "kwin:Virtual-9");
    assert_eq!(ready["outputs"][0]["attached"], true);

    *backend.helper.lock().unwrap() = Err(CaptureError::permission_denied(HELPER));
    let denied = controller
        .handle(VIDEO, enumerate.clone(), 0)
        .unwrap()
        .payload;
    assert_eq!(denied["availability"]["reason"], "permission_denied");
    assert!(
        denied["availability"]["message"]
            .as_str()
            .unwrap()
            .contains("setcap cap_sys_admin+p")
    );

    *backend.helper.lock().unwrap() = Ok(());
    *backend.connectors.lock().unwrap() = Err(CaptureError::driver_unavailable("gone"));
    let missing = controller.handle(VIDEO, enumerate, 0).unwrap().payload;
    assert_eq!(missing["availability"]["reason"], "driver_unavailable");
    assert!(missing["outputs"].as_array().unwrap().is_empty());

    let status = controller
        .handle(VIDEO, Operation::Status, 0)
        .unwrap()
        .payload;
    assert_eq!(status["availability"]["reason"], "driver_unavailable");
}

#[test]
fn refresh_follows_a_crtc_change_and_a_mode_change() {
    let (mut controller, backend) = controller();
    let id = opened(&mut controller);
    *backend.connectors.lock().unwrap() = Ok(vec![connector(9, 55, 2560)]);
    controller.refresh(RESOLVE_INTERVAL_MS - 1);
    assert_eq!(controller.reader(VIDEO, id).unwrap().frames.crtc_id(), 41);

    // The scanout changed but the compositor has not announced it yet.
    controller.refresh(RESOLVE_INTERVAL_MS);
    let reader = controller.reader(VIDEO, id).unwrap();
    assert_eq!(reader.frames.crtc_id(), 55);
    assert_eq!(reader.geometry, StreamGeometry::NotReady);
    assert_eq!(reader.frames.fault(), None);
    let status = controller
        .handle(VIDEO, Operation::Status, 0)
        .unwrap()
        .payload;
    assert!(status["streams"][0].get("desktop_geometry").is_none());

    // The compositor's done event triggers an immediate refresh.
    *backend.layout.lock().unwrap() = Some(appended_layout(2560));
    *backend.layout_changed.lock().unwrap() = true;
    controller.refresh(RESOLVE_INTERVAL_MS + 1);
    let StreamGeometry::Ready(geometry) = controller.reader(VIDEO, id).unwrap().geometry else {
        panic!("geometry should be ready again");
    };
    assert_eq!(
        geometry.output,
        DesktopRect::new(1210, 0, 2560, 1080).unwrap()
    );
    assert_eq!(geometry.virtual_desktop.width, 1210 + 2560 + 1920);
}

#[test]
fn without_a_wayland_display_geometry_is_unsupported() {
    let (mut controller, backend) = controller();
    *backend.layout.lock().unwrap() = None;
    let reply = controller
        .handle(VIDEO, open("kwin:Virtual-9", "video-1"), 0)
        .unwrap();
    assert_eq!(reply.payload["geometry_supported"], false);
    assert!(reply.payload.get("desktop_geometry").is_none());
    let id = reply.created.unwrap();
    assert_eq!(
        controller.reader(VIDEO, id).unwrap().geometry,
        StreamGeometry::Unsupported
    );
    // A display that appears later does not change what open reported.
    *backend.layout.lock().unwrap() = Some(appended_layout(1920));
    controller.refresh(RESOLVE_INTERVAL_MS);
    assert_eq!(
        controller.reader(VIDEO, id).unwrap().geometry,
        StreamGeometry::Unsupported
    );
    let enumeration = controller
        .handle(
            VIDEO,
            Operation::Enumerate {
                offset: None,
                limit: None,
            },
            0,
        )
        .unwrap()
        .payload;
    assert_eq!(enumeration["geometry_supported"], true);
}

#[test]
fn an_inconsistent_layout_refuses_open_instead_of_guessing() {
    let (mut controller, backend) = controller();
    *backend.layout.lock().unwrap() = Some(appended_layout(2560));
    let error = controller
        .handle(VIDEO, open("kwin:Virtual-9", "video-1"), 0)
        .err()
        .unwrap();
    assert_eq!(error.code, "output_geometry_unavailable");
    assert!(error.message.contains("mode differs"));

    let mut missing = appended_layout(1920);
    missing.retain(|output| output.name != "Virtual-9");
    *backend.layout.lock().unwrap() = Some(missing);
    assert_eq!(
        controller
            .handle(VIDEO, open("kwin:Virtual-9", "video-1"), 0)
            .err()
            .unwrap()
            .code,
        "output_geometry_unavailable"
    );
    assert!(backend.connects.lock().unwrap().is_empty());
}

#[test]
fn a_crtc_lookup_failure_triggers_an_immediate_refresh() {
    let (mut controller, backend) = controller();
    let id = opened(&mut controller);
    let frames = controller.reader(VIDEO, id).unwrap().frames;
    backend.script.present(4, 2_000_000);
    assert!(eventually(|| frames
        .latest_presentation()
        .unwrap()
        .sequence
        == 4));
    *backend.connectors.lock().unwrap() = Ok(vec![connector(9, 60, 1920)]);
    backend.script.push_wait(ScriptedWait::Refuse(
        amaterasu_vkms_frame_helper::protocol::Failure::CrtcNotFound,
    ));
    // Long before the periodic re-resolve is due, the thread's request
    // makes the next refresh follow the new CRTC.
    assert!(eventually(|| {
        controller.refresh(1);
        frames.crtc_id() == 60
    }));
    assert_eq!(frames.fault(), None);
    let status = controller
        .handle(VIDEO, Operation::Status, 0)
        .unwrap()
        .payload;
    assert_eq!(status["streams"][0]["crtc_id"], 60);

    // The woken thread waits on the new CRTC and delivers from it.
    backend.script.present(1, 3_000_000);
    assert!(eventually(|| frames
        .latest_presentation()
        .unwrap()
        .sequence
        == 5));
    assert!(backend.script.waits.lock().unwrap().contains(&(60, 0)));
}

#[test]
fn close_stops_the_presentation_thread_promptly() {
    let (mut controller, backend) = controller();
    let id = opened(&mut controller);
    let frames = controller.reader(VIDEO, id).unwrap().frames;
    backend.script.present_fenced(1);
    assert!(eventually(|| frames
        .latest_presentation()
        .unwrap()
        .sequence
        == 1));
    let started = std::time::Instant::now();
    controller.close(VIDEO, id).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_millis(100));
    let waits = backend.script.waits.lock().unwrap().len();
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(backend.script.waits.lock().unwrap().len(), waits);
    // The last frame closes when the stream's state is released.
    drop(frames);
    assert!(backend.script.all_lent_descriptors_closed());
}

#[test]
fn a_disconnected_or_replaced_output_faults_the_stream() {
    let (mut controller, backend) = controller();
    let id = opened(&mut controller);
    let mut gone = connector(9, 41, 1920);
    gone.connection = crate::discovery::Connection::Disconnected;
    *backend.connectors.lock().unwrap() = Ok(vec![gone]);
    controller.refresh(RESOLVE_INTERVAL_MS);
    let frames = controller.reader(VIDEO, id).unwrap().frames;
    assert_eq!(
        frames.fault().map(StreamFault::code),
        Some("output_disconnected")
    );
    let status = controller
        .handle(VIDEO, Operation::Status, 0)
        .unwrap()
        .payload;
    assert_eq!(status["streams"][0]["fault"], "output_disconnected");

    let (mut controller, backend) = self::controller();
    let id = opened(&mut controller);
    let mut replaced = connector(9, 41, 1920);
    replaced.connector_id = 999;
    *backend.connectors.lock().unwrap() = Ok(vec![replaced]);
    controller.refresh(RESOLVE_INTERVAL_MS);
    assert_eq!(
        controller
            .reader(VIDEO, id)
            .unwrap()
            .frames
            .fault()
            .map(StreamFault::code),
        Some("output_replaced")
    );
}

#[test]
fn stream_limits_and_rollback() {
    let (mut controller, _) = controller();
    for index in 0..MAX_STREAMS {
        controller
            .handle(VIDEO, open("kwin:Virtual-9", &format!("op-{index}")), 0)
            .unwrap();
    }
    assert_eq!(
        controller
            .handle(VIDEO, open("kwin:Virtual-9", "one-more"), 0)
            .err()
            .unwrap()
            .code,
        "resource_exhausted"
    );
    let (mut controller, _) = self::controller();
    let reply = controller
        .handle(VIDEO, open("kwin:Virtual-9", "lost"), 0)
        .unwrap();
    controller.rollback(VIDEO, reply.created);
    let status = controller
        .handle(VIDEO, Operation::Status, 0)
        .unwrap()
        .payload;
    assert_eq!(status["active"], 0);
    assert_eq!(status["closed"], 1);
}

#[test]
fn shutdown_closes_admission_for_every_stream() {
    let (mut controller, _) = controller();
    let id = opened(&mut controller);
    let frames = controller.reader(VIDEO, id).unwrap().frames;
    controller.shutdown();
    assert!(frames.admission.enter().is_none());
    assert_eq!(
        controller
            .handle(VIDEO, open("kwin:Virtual-9", "after"), 0)
            .err()
            .unwrap()
            .code,
        "capture_disabled"
    );
}
