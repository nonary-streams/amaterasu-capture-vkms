use super::sdk_resource::CaptureApiV1_8;
use super::*;
use crate::{
    model::Operation,
    test_support::{FakeBackend, SourceScript, eventually, settings},
};
use amaterasu_media::{
    CAPTURE_API_V1_4_SIZE, CAPTURE_API_V1_6_SIZE, CAPTURE_DMABUF_INTERFACE_MINOR,
    CAPTURE_FRAME_CACHED_REUSE, CAPTURE_FRAME_FRESH, CaptureApiV1_4, CpuFrame, DmaBufFrame,
    MEDIA_PENDING,
};

struct TestBinding(*const CaptureApiV1_6);

impl TestBinding {
    fn bind(factory: &Factory<FakeBackend>, caller: &[u8], min_minor: u32) -> Result<Self, Status> {
        Self::bind_sized(
            factory,
            caller,
            min_minor,
            if min_minor <= 5 {
                CAPTURE_API_V1_5_SIZE
            } else {
                CAPTURE_API_V1_6_SIZE
            },
        )
    }

    fn bind_sized(
        factory: &Factory<FakeBackend>,
        caller: &[u8],
        min_minor: u32,
        min_size: u32,
    ) -> Result<Self, Status> {
        let mut interface = std::ptr::null();
        let query = NativeInterfaceQuery {
            header: AbiHeader::of::<NativeInterfaceQuery>(),
            interface_major: INTERFACE_MAJOR,
            min_minor,
            min_size,
        };
        // SAFETY: stable factory, caller bytes, query and output slot last through the call.
        let status = unsafe {
            factory.api.bind.unwrap()(
                factory.api.context,
                BorrowedBytes::new(caller),
                &query,
                &mut interface,
            )
        };
        if status == STATUS_OK {
            Ok(Self(interface.cast()))
        } else {
            assert!(interface.is_null());
            Err(status)
        }
    }

    fn api(&self) -> &CaptureApiV1_6 {
        // SAFETY: this wrapper uniquely owns the live boxed binding until Drop.
        unsafe { &*self.0 }
    }

    fn read(&self, stream: u64, sink: &mut Sink) -> Status {
        let api = self.api();
        // SAFETY: the binding and the exclusive sink outlive the synchronous call.
        unsafe {
            (api.read_dmabuf_frame)(
                api.base.context,
                stream,
                DmaBufFrameSink {
                    context: (sink as *mut Sink).cast(),
                    write: record,
                },
            )
        }
    }

    fn latest(&self, stream: u64) -> Result<PresentationStatus, Status> {
        let api = self.api();
        let mut slot = std::mem::MaybeUninit::<PresentationStatus>::uninit();
        // SAFETY: the binding is live and `slot` is a writable aligned slot.
        let status =
            unsafe { (api.latest_presentation)(api.base.context, stream, slot.as_mut_ptr()) };
        if status == STATUS_OK {
            // SAFETY: STATUS_OK means the provider initialized the slot.
            Ok(unsafe { slot.assume_init() })
        } else {
            Err(status)
        }
    }

    fn reaches(&self, stream: u64, sequence: u64) -> bool {
        eventually(|| {
            self.latest(stream)
                .is_ok_and(|latest| latest.sequence == sequence)
        })
    }
}

impl Drop for TestBinding {
    fn drop(&mut self) {
        let api = self.api();
        // SAFETY: releases this binding once after all calls finished.
        unsafe { api.base.release.unwrap()(api.base.context) };
    }
}

struct Sink {
    status: Status,
    deliveries: Vec<(DmaBufFrame, u32, bool)>,
    script: Arc<SourceScript>,
}

/// Records each delivery and whether its descriptors were still open.
unsafe extern "C" fn record(
    context: *mut c_void,
    frame: *const DmaBufFrame,
    freshness: u32,
) -> Status {
    // SAFETY: the test passes an exclusive Sink and a frame valid for this call.
    let (sink, frame) = unsafe { (&mut *context.cast::<Sink>(), *frame) };
    let open = !sink.script.all_lent_descriptors_closed();
    sink.deliveries.push((frame, freshness, open));
    sink.status
}

fn setup() -> (Box<Factory<FakeBackend>>, Arc<SourceScript>, u64) {
    let backend = FakeBackend::new();
    let script = backend.script.clone();
    let mut controller = Controller::new(backend, settings());
    let id = controller
        .handle(
            "amaterasu.video",
            Operation::Open {
                output: "kwin:Virtual-9".into(),
                operation_id: Some("video".into()),
                constant_rate: None,
            },
            0,
        )
        .unwrap()
        .created
        .unwrap();
    let factory = Factory::new(Arc::new(Mutex::new(controller)));
    (factory, script, id)
}

#[test]
fn the_table_advertises_only_the_configured_frame_lifetime() {
    let (factory, _, _) = setup();
    let binding = TestBinding::bind(&factory, b"amaterasu.video", 5).unwrap();
    let header = binding.api().base.header;
    let expected = capture_header(cfg!(feature = "driver-read-leases"));
    assert_eq!(header.minor, expected.minor);
    assert_eq!(header.size, expected.size);
    if !cfg!(feature = "driver-read-leases") {
        assert_eq!(
            TestBinding::bind(&factory, b"amaterasu.video", 6).err(),
            Some(MEDIA_UNSUPPORTED)
        );
        assert_eq!(
            TestBinding::bind_sized(&factory, b"amaterasu.video", 5, CAPTURE_API_V1_6_SIZE).err(),
            Some(MEDIA_UNSUPPORTED)
        );
    }
    assert_eq!(
        TestBinding::bind(&factory, b"amaterasu.video", 10).err(),
        Some(MEDIA_UNSUPPORTED)
    );
    assert_eq!(
        TestBinding::bind_sized(
            &factory,
            b"amaterasu.video",
            5,
            size_of::<CaptureApiV1_9>() as u32 + 8
        )
        .err(),
        Some(MEDIA_UNSUPPORTED)
    );
    assert_eq!(
        TestBinding::bind(&factory, b"", 4).err(),
        Some(STATUS_INVALID_ARGUMENT)
    );
}

#[test]
fn required_leases_cannot_fall_back_to_a_borrowed_frame() {
    let (mut factory, script, id) = setup();
    factory.header = capture_header(true);
    factory.require_lease = true;
    let binding = TestBinding::bind(&factory, b"amaterasu.video", 5).unwrap();
    let frames = factory
        .controller
        .lock()
        .unwrap()
        .reader("amaterasu.video", id)
        .unwrap()
        .frames;
    frames.store_snapshot(script.export(1, 0, false), 1);
    let mut sink = Sink {
        status: STATUS_OK,
        deliveries: Vec::new(),
        script,
    };
    assert_eq!(binding.read(id, &mut sink), MEDIA_UNSUPPORTED);
    assert!(sink.deliveries.is_empty());
}

#[test]
fn cpu_frame_callbacks_are_unsupported() {
    let (factory, _, id) = setup();
    let binding = TestBinding::bind(&factory, b"amaterasu.video", 1).unwrap();
    let api = binding.api();
    unsafe extern "C" fn never(_: *mut c_void, _: *const CpuFrame) -> Status {
        panic!("no CPU frame is delivered")
    }
    // SAFETY: the binding is live and the sink is never called.
    let status = unsafe {
        (api.read_frame)(
            api.base.context,
            id,
            FrameSink {
                context: std::ptr::null_mut(),
                write: never,
            },
        )
    };
    assert_eq!(status, MEDIA_UNSUPPORTED);
}

#[test]
fn geometry_reports_the_owned_output() {
    let (factory, _, id) = setup();
    let binding = TestBinding::bind(&factory, b"amaterasu.video", 1).unwrap();
    let api = binding.api();
    let mut geometry = std::mem::MaybeUninit::<CaptureGeometry>::uninit();
    // SAFETY: the binding is live and `geometry` is a writable aligned slot.
    let status = unsafe { (api.geometry)(api.base.context, id, geometry.as_mut_ptr()) };
    assert_eq!(status, STATUS_OK);
    // SAFETY: STATUS_OK means the provider initialized the slot.
    let geometry = unsafe { geometry.assume_init() };
    assert!(geometry.validate().is_ok());
    assert_eq!(
        (
            geometry.output.left,
            geometry.output.width,
            geometry.output.height
        ),
        (1210, 1920, 1080)
    );
    assert_eq!(geometry.virtual_desktop.left, 0);

    let other = TestBinding::bind(&factory, b"other.module", 1).unwrap();
    let api = other.api();
    let mut slot = std::mem::MaybeUninit::<CaptureGeometry>::uninit();
    // SAFETY: as above.
    let status = unsafe { (api.geometry)(api.base.context, id, slot.as_mut_ptr()) };
    assert_eq!(status, MEDIA_CLOSED);
}

#[test]
fn geometry_is_busy_while_not_ready_and_unsupported_without_wayland() {
    for (layout, expected) in [
        (Some(crate::test_support::appended_layout(1920)), MEDIA_BUSY),
        (None, MEDIA_UNSUPPORTED),
    ] {
        let backend = FakeBackend::new();
        let handles = backend.handles();
        if layout.is_none() {
            *handles.layout.lock().unwrap() = None;
        }
        let mut controller = Controller::new(backend, settings());
        let id = controller
            .handle(
                "amaterasu.video",
                Operation::Open {
                    output: "kwin:Virtual-9".into(),
                    operation_id: Some("video".into()),
                    constant_rate: None,
                },
                0,
            )
            .unwrap()
            .created
            .unwrap();
        // A scanout mode change the compositor has not announced yet.
        *handles.connectors.lock().unwrap() = Ok(vec![crate::test_support::connector(9, 41, 2560)]);
        controller.refresh(crate::controller::RESOLVE_INTERVAL_MS);
        let factory = Factory::new(Arc::new(Mutex::new(controller)));
        let binding = TestBinding::bind(&factory, b"amaterasu.video", 1).unwrap();
        let api = binding.api();
        let mut slot = std::mem::MaybeUninit::<CaptureGeometry>::uninit();
        // SAFETY: the binding is live and `slot` is a writable aligned slot.
        let status = unsafe { (api.geometry)(api.base.context, id, slot.as_mut_ptr()) };
        assert_eq!(status, expected);
    }
}

#[test]
fn a_v1_4_consumer_binds_the_same_table() {
    let (factory, script, id) = setup();
    let binding =
        TestBinding::bind_sized(&factory, b"amaterasu.video", 4, CAPTURE_API_V1_4_SIZE).unwrap();
    assert!(binding.api().base.header.minor >= CAPTURE_DMABUF_INTERFACE_MINOR);
    assert!(binding.api().base.header.size >= CAPTURE_API_V1_4_SIZE);
    // SAFETY: the v1.5 table starts with the complete v1.4 layout.
    let prefix = unsafe { &*binding.0.cast::<CaptureApiV1_4>() };
    script.present_fenced(2);
    assert!(binding.reaches(id, 2));
    let mut sink = Sink {
        status: STATUS_OK,
        deliveries: Vec::new(),
        script,
    };
    // SAFETY: the binding and the exclusive sink outlive the synchronous call.
    let status = unsafe {
        (prefix.read_dmabuf_frame)(
            prefix.base.context,
            id,
            DmaBufFrameSink {
                context: (&mut sink as *mut Sink).cast(),
                write: record,
            },
        )
    };
    assert_eq!(status, STATUS_OK);
    assert_eq!(sink.deliveries[0].0.sequence, 2);
}

#[test]
fn reads_are_pending_until_the_first_presentation() {
    let (factory, script, id) = setup();
    let binding = TestBinding::bind(&factory, b"amaterasu.video", 5).unwrap();
    let mut sink = Sink {
        status: STATUS_OK,
        deliveries: Vec::new(),
        script,
    };
    assert_eq!(binding.read(id, &mut sink), MEDIA_PENDING);
    assert_eq!(binding.latest(id), Ok(PresentationStatus::default()));
    assert!(sink.deliveries.is_empty());
}

#[test]
fn dmabuf_reads_lend_the_stored_frame_with_freshness() {
    let (factory, script, id) = setup();
    let binding = TestBinding::bind(&factory, b"amaterasu.video", 5).unwrap();
    let mut sink = Sink {
        status: STATUS_OK,
        deliveries: Vec::new(),
        script: script.clone(),
    };
    script.present_fenced(7);
    assert!(binding.reaches(id, 7));
    let requests = script.requested_crtcs.lock().unwrap().len();
    assert_eq!(binding.read(id, &mut sink), STATUS_OK);
    assert_eq!(binding.read(id, &mut sink), STATUS_OK);
    // Reads never ask the helper.
    assert_eq!(script.requested_crtcs.lock().unwrap().len(), requests);

    script.present_fenced(8);
    assert!(binding.reaches(id, 8));
    assert_eq!(binding.read(id, &mut sink), STATUS_OK);
    let freshness: Vec<u32> = sink.deliveries.iter().map(|delivery| delivery.1).collect();
    assert_eq!(
        freshness,
        vec![
            CAPTURE_FRAME_FRESH,
            CAPTURE_FRAME_CACHED_REUSE,
            CAPTURE_FRAME_FRESH
        ]
    );
    // Descriptors were open during every callback; the stream still owns
    // the newest frame's afterward and closed the replaced one.
    assert!(sink.deliveries.iter().all(|delivery| delivery.2));
    assert_eq!(sink.deliveries[2].0.sequence, 8);
    assert_eq!(script.open_descriptors(), 2);
    let latest = binding.latest(id).unwrap();
    assert_eq!(latest.sequence, 8);
    assert_eq!(latest.timestamp_us, sink.deliveries[2].0.timestamp_us);
}

#[test]
fn a_sink_rejection_is_returned_and_the_frame_stays_fresh() {
    let (factory, script, id) = setup();
    let binding = TestBinding::bind(&factory, b"amaterasu.video", 5).unwrap();
    let mut sink = Sink {
        status: MEDIA_BUSY,
        deliveries: Vec::new(),
        script: script.clone(),
    };
    script.present_fenced(3);
    assert!(binding.reaches(id, 3));
    assert_eq!(binding.read(id, &mut sink), MEDIA_BUSY);
    sink.status = STATUS_OK;
    assert_eq!(binding.read(id, &mut sink), STATUS_OK);
    assert_eq!(sink.deliveries[1].1, CAPTURE_FRAME_FRESH);
}

#[test]
fn a_failed_helper_is_a_backend_error_for_reads_and_queries() {
    let (factory, script, id) = setup();
    let binding = TestBinding::bind(&factory, b"amaterasu.video", 5).unwrap();
    script.present(1, 2_000_000);
    assert!(binding.reaches(id, 1));
    script.push_wait(crate::test_support::ScriptedWait::HelperGone);
    assert!(eventually(|| binding.latest(id) == Err(MEDIA_BACKEND_ERROR)));
    let mut sink = Sink {
        status: STATUS_OK,
        deliveries: Vec::new(),
        script,
    };
    assert_eq!(binding.read(id, &mut sink), MEDIA_BACKEND_ERROR);
    assert!(sink.deliveries.is_empty());
}

#[test]
fn presentation_queries_validate_the_output_slot() {
    let (factory, _, id) = setup();
    let binding = TestBinding::bind(&factory, b"amaterasu.video", 5).unwrap();
    let api = binding.api();
    // SAFETY: the binding is live; the provider rejects the null slot.
    let status = unsafe { (api.latest_presentation)(api.base.context, id, std::ptr::null_mut()) };
    assert_eq!(status, STATUS_INVALID_ARGUMENT);
    let other = TestBinding::bind(&factory, b"other.module", 5).unwrap();
    assert_eq!(other.latest(id), Err(MEDIA_CLOSED));
}

#[test]
fn other_callers_and_closed_streams_cannot_read() {
    let (factory, script, id) = setup();
    let mut sink = Sink {
        status: STATUS_OK,
        deliveries: Vec::new(),
        script,
    };
    let other = TestBinding::bind(&factory, b"other.module", 5).unwrap();
    assert_eq!(other.read(id, &mut sink), MEDIA_CLOSED);

    let owner = TestBinding::bind(&factory, b"amaterasu.video", 5).unwrap();
    factory
        .controller
        .lock()
        .unwrap()
        .close("amaterasu.video", id)
        .unwrap();
    assert_eq!(owner.read(id, &mut sink), MEDIA_CLOSED);
    assert_eq!(owner.latest(id), Err(MEDIA_CLOSED));
    assert!(sink.deliveries.is_empty());
}

#[test]
fn a_busy_controller_is_media_busy() {
    let (factory, script, id) = setup();
    let binding = TestBinding::bind(&factory, b"amaterasu.video", 5).unwrap();
    let mut sink = Sink {
        status: STATUS_OK,
        deliveries: Vec::new(),
        script,
    };
    let _held = factory.controller.lock().unwrap();
    assert_eq!(binding.read(id, &mut sink), MEDIA_BUSY);
    assert_eq!(binding.latest(id), Err(MEDIA_BUSY));
}

#[test]
fn immutable_native_read_rejects_mutable_scanout_and_retains_private_snapshot() {
    let _serial = crate::leases::TEST_LEASE_CALLBACKS.lock().unwrap();
    let (mut factory, script, id) = setup();
    // These tests inject private snapshots rather than starting a GPU backend.
    factory.header = capture_header(true);
    let binding = TestBinding::bind(&factory, b"amaterasu.video", 6).unwrap();
    let frames = factory
        .controller
        .lock()
        .unwrap()
        .reader("amaterasu.video", id)
        .unwrap()
        .frames;
    frames.store(script.export(1, 0, false), 1);
    struct Retained(Option<amaterasu_media::DmaBufFrameLease>);
    unsafe extern "C" fn capture(
        context: *mut c_void,
        lease: *const amaterasu_media::DmaBufFrameLease,
        _: u32,
    ) -> Status {
        // SAFETY: provider retains lease during callback, context is this test's sink.
        let lease = unsafe { &*lease };
        // SAFETY: successful retain grants exactly one reference consumed below.
        let status = unsafe { lease.retain.unwrap()(lease.context) };
        if status == STATUS_OK {
            // SAFETY: context is the exclusively borrowed test sink.
            unsafe { (*context.cast::<Retained>()).0 = Some(*lease) };
        }
        status
    }
    let mut retained = Retained(None);
    let api = binding.api();
    let sink = amaterasu_media::DmaBufLeaseSink {
        context: (&mut retained as *mut Retained).cast(),
        write: capture,
    };
    // SAFETY: live native binding and sink during this synchronous callback.
    assert_eq!(
        unsafe { (api.read_dmabuf_frame_leased)(api.base.context, id, sink) },
        MEDIA_UNSUPPORTED
    );
    assert!(retained.0.is_none());
    frames.store_snapshot(script.export(2, 0, false), 2);
    // SAFETY: same live binding and sink; this frame is independently owned.
    assert_eq!(
        unsafe { (api.read_dmabuf_frame_leased)(api.base.context, id, sink) },
        STATUS_OK
    );
    let lease = retained.0.take().unwrap();
    factory
        .controller
        .lock()
        .unwrap()
        .handle("amaterasu.video", Operation::Close { stream: id }, 0)
        .unwrap();
    drop(frames);
    assert_eq!(script.open_descriptors(), 1);
    // SAFETY: consumes the one retained reference while binding remains live.
    unsafe { lease.release.unwrap()(lease.context, -1) };
    assert!(script.all_lent_descriptors_closed());
}

#[test]
fn resource_negotiation_and_retained_dmabuf_follow_frame_lifetime() {
    use super::sdk_resource::{
        CaptureResourceFrame, CaptureResourceFrameSink, CaptureResourceOffer,
        CaptureResourceOfferSink, DmaBufResourceCapabilities, FRAME_RESOURCE_DMABUF,
        FRAME_SYNC_NONE, FrameDescriptor,
    };
    let _serial = crate::leases::TEST_LEASE_CALLBACKS.lock().unwrap();
    let (mut factory, script, id) = setup();
    // These tests inject private snapshots rather than starting a GPU backend.
    factory.header = capture_header(true);
    let binding = TestBinding::bind(&factory, b"amaterasu.video", 8).unwrap();
    let api = unsafe { &*binding.0.cast::<CaptureApiV1_8>() };
    let frames = factory
        .controller
        .lock()
        .unwrap()
        .reader("amaterasu.video", id)
        .unwrap()
        .frames;
    frames.store_snapshot(script.export(11, 11_000, false), 11);
    unsafe extern "C" fn offer(_: *mut c_void, offer: *const CaptureResourceOffer) -> Status {
        let offer = unsafe { &*offer };
        assert_eq!(offer.resource.kind, FRAME_RESOURCE_DMABUF);
        assert_eq!(
            offer.ownership,
            super::sdk_resource::FRAME_OWNERSHIP_RETAINABLE
        );
        let capability = unsafe { &*offer.resource.data.cast::<DmaBufResourceCapabilities>() };
        assert_ne!(capability.fourcc, 0);
        STATUS_OK
    }
    let mut generation = 0;
    let sink = CaptureResourceOfferSink {
        context: std::ptr::null_mut(),
        write: offer,
    };
    let context = api.base.base.base.context;
    assert_eq!(
        unsafe { (api.enumerate_resources)(context, id, sink, &mut generation) },
        STATUS_OK
    );
    assert_eq!(
        unsafe { (api.select_resource)(context, id, generation + 1, 1) },
        MEDIA_UNSUPPORTED
    );
    let mut changed = script.export(12, 12_000, false);
    changed.descriptor.width += 1;
    frames.store_snapshot(changed, 12);
    assert_eq!(
        unsafe { (api.select_resource)(context, id, generation, 1) },
        MEDIA_UNSUPPORTED
    );
    assert_eq!(
        unsafe { (api.enumerate_resources)(context, id, sink, &mut generation) },
        STATUS_OK
    );
    assert_eq!(generation, 2);
    assert_eq!(
        unsafe { (api.select_resource)(context, id, generation, 1) },
        STATUS_OK
    );
    unsafe extern "C" fn frame(context: *mut c_void, value: *const CaptureResourceFrame) -> Status {
        let value = unsafe { &*value };
        assert_eq!(value.resource.kind, FRAME_RESOURCE_DMABUF);
        assert_eq!(unsafe { (value.retain)(value.context) }, STATUS_OK);
        unsafe { *context.cast::<Option<CaptureResourceFrame>>() = Some(*value) };
        STATUS_OK
    }
    let mut retained: Option<CaptureResourceFrame> = None;
    let sink = CaptureResourceFrameSink {
        context: (&mut retained as *mut Option<CaptureResourceFrame>).cast(),
        write: frame,
    };
    assert_eq!(
        unsafe { (api.read_resource_frame)(context, id, sink) },
        STATUS_OK
    );
    frames.store_snapshot(script.export(12, 12_000, false), 12);
    assert_eq!(script.open_descriptors(), 2);
    let retained = retained.unwrap();
    let done = FrameDescriptor {
        kind: FRAME_SYNC_NONE,
        version: 0,
        size: 0,
        reserved: 0,
        namespace: [0; 16],
        data: std::ptr::null(),
    };
    unsafe { (retained.release)(retained.context, &done) };
    assert_eq!(script.open_descriptors(), 1);
}

#[test]
fn hdr_tail_tracks_the_completed_snapshot_and_preserves_old_bindings() {
    let (mut factory, script, id) = setup();
    factory.header = capture_header(true);
    let binding = TestBinding::bind_sized(
        &factory,
        b"amaterasu.video",
        9,
        size_of::<CaptureApiV1_9>() as u32,
    )
    .unwrap();
    let frames = factory
        .controller
        .lock()
        .unwrap()
        .reader("amaterasu.video", id)
        .unwrap()
        .frames;
    let mut source = script.export(1, 1000, false);
    source.descriptor.color = 2;
    source.descriptor.fourcc = u32::from_le_bytes(*b"AR30");
    source.descriptor.hdr_metadata = [
        34000, 16000, 13250, 34500, 7500, 3000, 15635, 16450, 1000, 1, 1000, 400,
    ];
    frames.store_snapshot(source, 1);
    let api = unsafe { &*binding.0.cast::<CaptureApiV1_9>() };
    let mut metadata = HdrMetadata::default();
    // SAFETY: the binding, stream and aligned writable metadata live for the call.
    assert_eq!(
        unsafe { (api.hdr_metadata)(api.base.base.base.context, id, &mut metadata) },
        STATUS_OK
    );
    assert_eq!(metadata.enabled, 1);
    assert_eq!(metadata.fields[8], 1000);
    frames.store_snapshot(script.export(2, 2000, false), 2);
    assert_eq!(
        unsafe { (api.hdr_metadata)(api.base.base.base.context, id, &mut metadata) },
        STATUS_OK
    );
    assert_eq!(metadata.enabled, 0);
    assert_eq!(metadata.fields, [0; 12]);
}
