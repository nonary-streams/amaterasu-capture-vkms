use super::*;
use amaterasu_vkms_frame_helper::protocol::{
    Opcode, REQUEST_LEN, RequestRecord, decode_response, encode_request, encode_response,
};
use std::{
    cell::{Cell, RefCell},
    fs::File,
    os::fd::{AsRawFd, FromRawFd},
    rc::Rc,
};

/// A pipe read end plus its write end, which reports when every copy of the
/// read end has been closed.
fn tracked_pipe() -> (OwnedFd, File) {
    let mut fds = [-1; 2];
    // SAFETY: `fds` is a writable two-element array as pipe2 requires.
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    // SAFETY: pipe2 succeeded and returned two new descriptors owned here.
    unsafe { (OwnedFd::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
}

fn pipe_end() -> OwnedFd {
    tracked_pipe().0
}

fn reader_closed(writer: &File) -> bool {
    let mut poll = libc::pollfd {
        fd: writer.as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: one initialized pollfd is passed with a zero timeout.
    assert_eq!(unsafe { libc::poll(&mut poll, 1, 0) }, 1);
    poll.revents & libc::POLLERR != 0
}

#[derive(Default)]
struct Script {
    ioctls: Cell<usize>,
    frame: RefCell<Option<io::Result<DriverFrame>>>,
    presentation: RefCell<Option<io::Result<DriverPresentation>>>,
}

struct FakeCard(Rc<Script>);

impl PresentationCard for FakeCard {
    fn export_frame(&self, _crtc_id: u32) -> io::Result<DriverFrame> {
        self.0.ioctls.set(self.0.ioctls.get() + 1);
        self.0.frame.borrow_mut().take().expect("scripted frame")
    }
    fn wait_present(&self, _: u32, _: u64, _: u32) -> io::Result<DriverPresentation> {
        self.0.ioctls.set(self.0.ioctls.get() + 1);
        self.0
            .presentation
            .borrow_mut()
            .take()
            .expect("scripted presentation")
    }
}

struct FakeOpener {
    script: Rc<Script>,
    opens: Cell<usize>,
    refuse: Option<OpenFailure>,
}

impl CardOpener for &FakeOpener {
    type Card = FakeCard;
    fn open(&self, _: u32, _: u32) -> Result<FakeCard, OpenFailure> {
        self.opens.set(self.opens.get() + 1);
        match self.refuse {
            Some(failure) => Err(failure),
            None => Ok(FakeCard(self.script.clone())),
        }
    }
}

struct FakePrivilege {
    permitted: bool,
    raised: Cell<bool>,
    raises: Cell<usize>,
}

impl Privilege for &FakePrivilege {
    fn admin_permitted(&self) -> bool {
        self.permitted
    }
    fn with_admin<T>(&self, operation: impl FnOnce() -> T) -> Option<T> {
        if !self.permitted {
            return None;
        }
        self.raises.set(self.raises.get() + 1);
        self.raised.set(true);
        let value = operation();
        self.raised.set(false);
        Some(value)
    }
}

struct Fixture {
    script: Rc<Script>,
    opener: FakeOpener,
    privilege: FakePrivilege,
}

impl Fixture {
    fn new(permitted: bool) -> Self {
        let script = Rc::new(Script::default());
        Self {
            opener: FakeOpener {
                script: script.clone(),
                opens: Cell::new(0),
                refuse: None,
            },
            privilege: FakePrivilege {
                permitted,
                raised: Cell::new(false),
                raises: Cell::new(0),
            },
            script,
        }
    }

    fn service(&self) -> Service<&FakeOpener, &FakePrivilege> {
        Service::new(&self.opener, &self.privilege)
    }
}

fn send(
    service: &mut Service<&FakeOpener, &FakePrivilege>,
    request: Request,
) -> (ResponseRecord, Vec<OwnedFd>) {
    let bytes = encode_request(&RequestRecord {
        request_id: 77,
        request,
    });
    let (record, descriptors) = service.respond(&bytes, false);
    // Every reply the service builds must survive the strict client decoder.
    assert_eq!(decode_response(&encode_response(&record)), Ok(record));
    (record, descriptors)
}

fn failure_of(record: &ResponseRecord) -> Failure {
    match record.reply {
        Reply::Failure { failure, .. } => failure,
        reply => panic!("expected failure, got {reply:?}"),
    }
}

fn opened(fixture: &Fixture) -> Service<&FakeOpener, &FakePrivilege> {
    let mut service = fixture.service();
    let (record, _) = send(
        &mut service,
        Request::OpenCard {
            major: 226,
            minor: 3,
        },
    );
    assert_eq!(record.reply, Reply::CardOpened);
    service
}

fn ready_frame(planes: u32, fenced: bool) -> DriverFrame {
    let mut frame = DriverFrame {
        abi_version: DRIVER_FRAME_ABI_VERSION,
        crtc_id: 41,
        sequence: 12,
        timestamp_ns: 5_000,
        flags: DRIVER_FRAME_READY,
        color: 1,
        width: 1920,
        height: 1080,
        fourcc: u32::from_le_bytes(*b"XR24"),
        modifier: 0,
        plane_count: planes,
        reserved_zero: true,
        ..DriverFrame::default()
    };
    for plane in 0..planes as usize {
        frame.dma_bufs[plane] = Some(pipe_end());
        if cfg!(feature = "driver-read-leases") {
            frame.read_guards[plane] = Some(pipe_end());
        }
        frame.pitches[plane] = 7680;
        if fenced {
            frame.sync_files[plane] = Some(pipe_end());
        }
    }
    frame
}

#[test]
fn hello_reports_whether_admin_is_permitted() {
    for permitted in [true, false] {
        let fixture = Fixture::new(permitted);
        let (record, _) = send(&mut fixture.service(), Request::Hello);
        assert_eq!(record.request_id, 77);
        assert_eq!(record.opcode, Opcode::Hello as u16);
        assert_eq!(
            record.reply,
            Reply::Hello {
                capabilities: if permitted { HELPER_ADMIN_PERMITTED } else { 0 }
            }
        );
    }
}

#[test]
fn malformed_requests_never_reach_the_device() {
    let fixture = Fixture::new(true);
    let mut service = opened(&fixture);
    let mut bytes = encode_request(&RequestRecord {
        request_id: 5,
        request: Request::Frame { crtc_id: 41 },
    });
    bytes[REQUEST_LEN - 2] = 1;
    let (record, descriptors) = service.respond(&bytes, false);
    assert_eq!(failure_of(&record), Failure::MalformedRequest);
    assert_eq!(record.request_id, 5);
    assert!(descriptors.is_empty());

    let short = &bytes[..10];
    assert_eq!(
        failure_of(&service.respond(short, false).0),
        Failure::MalformedRequest
    );

    let valid = encode_request(&RequestRecord {
        request_id: 6,
        request: Request::Frame { crtc_id: 41 },
    });
    let (record, _) = service.respond(&valid, true);
    assert_eq!(failure_of(&record), Failure::MalformedRequest);
    assert_eq!(fixture.script.ioctls.get(), 0);
    assert_eq!(fixture.privilege.raises.get(), 0);
}

#[test]
fn frames_require_an_open_card() {
    let fixture = Fixture::new(true);
    let (record, _) = send(&mut fixture.service(), Request::Frame { crtc_id: 41 });
    assert_eq!(failure_of(&record), Failure::CardNotOpen);
    assert_eq!(fixture.script.ioctls.get(), 0);
}

#[test]
fn missing_admin_is_permission_denied_without_an_ioctl() {
    let fixture = Fixture::new(false);
    let mut service = opened(&fixture);
    for request in [
        Request::Frame { crtc_id: 41 },
        Request::Wait {
            crtc_id: 41,
            last_sequence: 0,
            timeout_ms: 0,
        },
    ] {
        let (record, _) = send(&mut service, request);
        assert_eq!(failure_of(&record), Failure::PermissionDenied);
    }
    assert_eq!(fixture.script.ioctls.get(), 0);
}

#[test]
fn the_card_is_opened_once_and_cannot_be_swapped() {
    let fixture = Fixture::new(true);
    let mut service = opened(&fixture);
    let (again, _) = send(
        &mut service,
        Request::OpenCard {
            major: 226,
            minor: 3,
        },
    );
    assert_eq!(again.reply, Reply::CardOpened);
    let (other, _) = send(
        &mut service,
        Request::OpenCard {
            major: 226,
            minor: 4,
        },
    );
    assert_eq!(failure_of(&other), Failure::CardAlreadyOpen);
    assert_eq!(fixture.opener.opens.get(), 1);
}

#[test]
fn open_failures_are_forwarded() {
    let mut fixture = Fixture::new(true);
    fixture.opener.refuse = Some(OpenFailure {
        failure: Failure::WrongDriver,
        os_error: 0,
    });
    let (record, _) = send(
        &mut fixture.service(),
        Request::OpenCard {
            major: 226,
            minor: 0,
        },
    );
    assert_eq!(failure_of(&record), Failure::WrongDriver);
}

#[test]
fn a_ready_frame_sends_buffers_then_fences_with_admin_raised_only_for_the_ioctl() {
    let fixture = Fixture::new(true);
    let mut service = opened(&fixture);
    let mut frame = ready_frame(2, false);
    frame.sync_files[1] = Some(pipe_end());
    let mut expected_order = vec![
        frame.dma_bufs[0].as_ref().unwrap().as_raw_fd(),
        frame.dma_bufs[1].as_ref().unwrap().as_raw_fd(),
        frame.sync_files[1].as_ref().unwrap().as_raw_fd(),
    ];
    expected_order.extend(frame.read_guards.iter().flatten().map(AsRawFd::as_raw_fd));
    *fixture.script.frame.borrow_mut() = Some(Ok(frame));

    let (record, descriptors) = send(&mut service, Request::Frame { crtc_id: 41 });
    let Reply::FrameReady(descriptor) = record.reply else {
        panic!("expected a frame, got {:?}", record.reply);
    };
    assert_eq!(descriptor.plane_count, 2);
    assert_eq!(descriptor.fence_mask, 0b10);
    assert_eq!(
        descriptor.descriptor_count(),
        if cfg!(feature = "driver-read-leases") {
            5
        } else {
            3
        }
    );
    let order: Vec<_> = descriptors.iter().map(AsRawFd::as_raw_fd).collect();
    assert_eq!(order, expected_order);
    assert_eq!(fixture.privilege.raises.get(), 1);
    assert!(!fixture.privilege.raised.get());
}

#[test]
fn an_empty_presentation_is_reported_without_descriptors() {
    let fixture = Fixture::new(true);
    let mut service = opened(&fixture);
    *fixture.script.frame.borrow_mut() = Some(Ok(DriverFrame {
        abi_version: DRIVER_FRAME_ABI_VERSION,
        crtc_id: 41,
        flags: DRIVER_FRAME_EMPTY,
        reserved_zero: true,
        ..DriverFrame::default()
    }));
    let (record, descriptors) = send(&mut service, Request::Frame { crtc_id: 41 });
    assert!(matches!(
        record.reply,
        Reply::FrameEmpty { crtc_id: 41, .. }
    ));
    assert!(descriptors.is_empty());
}

#[test]
fn malformed_driver_frames_close_every_descriptor() {
    let mutations: [fn(&mut DriverFrame, OwnedFd); 6] = [
        |frame, _| frame.abi_version = DRIVER_FRAME_ABI_VERSION + 1,
        |frame, _| frame.crtc_id = 40,
        |frame, _| frame.reserved_zero = false,
        |frame, _| frame.flags = DRIVER_FRAME_READY | DRIVER_FRAME_EMPTY,
        |frame, extra| frame.dma_bufs[3] = Some(extra),
        |frame, _| frame.pitches[0] = 0,
    ];
    for mutate in mutations {
        let fixture = Fixture::new(true);
        let mut service = opened(&fixture);
        let (buffer, buffer_writer) = tracked_pipe();
        let (fence, fence_writer) = tracked_pipe();
        let (extra, extra_writer) = tracked_pipe();
        let mut frame = ready_frame(1, false);
        frame.dma_bufs[0] = Some(buffer);
        frame.sync_files[0] = Some(fence);
        mutate(&mut frame, extra);
        *fixture.script.frame.borrow_mut() = Some(Ok(frame));
        let (record, descriptors) = send(&mut service, Request::Frame { crtc_id: 41 });
        assert_eq!(failure_of(&record), Failure::InvalidDriverResponse);
        assert!(descriptors.is_empty());
        assert!(reader_closed(&buffer_writer), "plane buffer leaked");
        assert!(reader_closed(&fence_writer), "fence leaked");
        assert!(reader_closed(&extra_writer), "stray descriptor leaked");
    }
}

#[test]
fn a_ready_frame_missing_a_plane_buffer_is_rejected() {
    let fixture = Fixture::new(true);
    let mut service = opened(&fixture);
    let mut frame = ready_frame(2, false);
    frame.dma_bufs[1] = None;
    *fixture.script.frame.borrow_mut() = Some(Ok(frame));
    let (record, _) = send(&mut service, Request::Frame { crtc_id: 41 });
    assert_eq!(failure_of(&record), Failure::InvalidDriverResponse);
}

#[test]
fn driver_errors_map_to_distinct_failures() {
    for (errno, expected) in [
        (libc::EACCES, Failure::PermissionDenied),
        (libc::EPERM, Failure::PermissionDenied),
        (libc::ENOENT, Failure::CrtcNotFound),
        (libc::EBUSY, Failure::Busy),
        (libc::EINTR, Failure::Interrupted),
        (libc::EINVAL, Failure::DriverError),
    ] {
        let fixture = Fixture::new(true);
        let mut service = opened(&fixture);
        *fixture.script.frame.borrow_mut() = Some(Err(io::Error::from_raw_os_error(errno)));
        let (record, _) = send(&mut service, Request::Frame { crtc_id: 41 });
        assert_eq!(
            record.reply,
            Reply::Failure {
                failure: expected,
                os_error: errno
            }
        );
    }
}

#[test]
fn waits_validate_the_driver_echo() {
    let presentation = DriverPresentation {
        abi_version: DRIVER_ABI_VERSION,
        crtc_id: 41,
        sequence: 13,
        timestamp_ns: 9,
        timeout_ms: 16,
        flags: PRESENT_CHANGED | PRESENT_PENDING,
        reserved_zero: true,
    };
    let fixture = Fixture::new(true);
    let mut service = opened(&fixture);
    *fixture.script.presentation.borrow_mut() = Some(Ok(presentation));
    let wait = Request::Wait {
        crtc_id: 41,
        last_sequence: 12,
        timeout_ms: 16,
    };
    let (record, _) = send(&mut service, wait);
    assert_eq!(
        record.reply,
        Reply::Presentation {
            crtc_id: 41,
            sequence: 13,
            timestamp_ns: 9,
            flags: PRESENT_CHANGED | PRESENT_PENDING,
        }
    );

    for broken in [
        DriverPresentation {
            timeout_ms: 15,
            ..presentation
        },
        DriverPresentation {
            flags: PRESENT_CHANGED | PRESENT_TIMEOUT,
            ..presentation
        },
        DriverPresentation {
            reserved_zero: false,
            ..presentation
        },
    ] {
        *fixture.script.presentation.borrow_mut() = Some(Ok(broken));
        let (record, _) = send(&mut service, wait);
        assert_eq!(failure_of(&record), Failure::InvalidDriverResponse);
    }
}

#[test]
#[cfg(feature = "driver-read-leases")]
fn unleased_driver_frame_is_rejected() {
    let mut frame = ready_frame(1, false);
    frame.flags = 1;
    let outcome = frame_outcome(41, frame);
    assert!(matches!(
        outcome.reply,
        Reply::Failure {
            failure: Failure::InvalidDriverResponse,
            ..
        }
    ));
}
