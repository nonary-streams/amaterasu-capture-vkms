//! No native helper or device is used: only scripted local protocol peers.
use super::*;
use amaterasu_vkms_frame_helper::{
    channel::SeqpacketChannel,
    protocol::{self, Reply},
};
use std::{
    collections::VecDeque,
    io,
    os::fd::{AsRawFd, BorrowedFd},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    thread::JoinHandle,
};

#[derive(Default)]
struct Receipt(AtomicBool);
impl Retirement for Receipt {
    fn poll_retired(&self) -> io::Result<bool> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

struct Session {
    client: HelperClient,
    receipt: Arc<Receipt>,
}
impl authority::ApprovedSession for Session {
    fn client(&mut self) -> &mut HelperClient {
        &mut self.client
    }
    fn retirement(&self) -> Arc<dyn Retirement> {
        self.receipt.clone()
    }
}

struct Connector {
    sessions: Mutex<VecDeque<Session>>,
    opens: Arc<AtomicUsize>,
}
impl authority::AuthorityConnector for Connector {
    fn connect(&self, _: BorrowedFd<'_>) -> io::Result<Box<dyn authority::ApprovedSession>> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(
            self.sessions
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted connection"),
        ))
    }
}

fn backend(sessions: Vec<Session>) -> (NativeBackend, Arc<AtomicUsize>) {
    let opens = Arc::new(AtomicUsize::new(0));
    let mut backend = NativeBackend::new();
    backend.authority = Box::new(Connector {
        sessions: Mutex::new(sessions.into()),
        opens: opens.clone(),
    });
    (backend, opens)
}

fn scripted(replies: Vec<Reply>) -> (Session, Arc<Receipt>, JoinHandle<()>) {
    let (client, server) = SeqpacketChannel::pair().unwrap();
    let receipt = Arc::new(Receipt::default());
    let worker = std::thread::spawn(move || {
        for reply in replies {
            let mut buffer = [0; protocol::REQUEST_LEN];
            let request = server.receive(&mut buffer).unwrap();
            let request = protocol::decode_request(&buffer[..request.len]).unwrap();
            let response = protocol::encode_response(&protocol::ResponseRecord {
                request_id: request.request_id,
                opcode: request.request.opcode() as u16,
                reply,
            });
            server.send(&response, &[]).unwrap();
        }
    });
    (
        Session {
            client: HelperClient::new(client),
            receipt: receipt.clone(),
        },
        receipt,
        worker,
    )
}

fn helper() -> (SeqpacketChannel, SeqpacketChannel) {
    SeqpacketChannel::pair().unwrap()
}

#[test]
fn failed_greeting_and_successful_probe_both_keep_the_exact_retirement_receipt() {
    for replies in [
        vec![],
        vec![Reply::Hello {
            capabilities: protocol::HELPER_ADMIN_PERMITTED,
        }],
    ] {
        let success = !replies.is_empty();
        let (session, receipt, peer) = scripted(replies);
        let (backend, _) = backend(vec![session]);
        let (control, _control_peer) = helper();
        assert_eq!(
            backend
                .probe_helper(HelperEndpoint::ApprovedChannel(control.as_fd().as_raw_fd()))
                .is_ok(),
            success
        );
        peer.join().unwrap();

        let mut controller =
            crate::controller::Controller::new(backend, crate::test_support::settings());
        assert_eq!(controller.quiesce(), Ok(false));
        receipt.0.store(true, Ordering::SeqCst);
        assert_eq!(controller.quiesce(), Ok(true));
    }
}

#[test]
fn card_open_and_initial_frame_errors_keep_their_retirement_jobs() {
    let binding = crate::test_support::FakeBackend::new()
        .resolve(OutputName::new(9).unwrap())
        .unwrap();
    for replies in [
        vec![
            Reply::Hello {
                capabilities: protocol::HELPER_ADMIN_PERMITTED,
            },
            Reply::Failure {
                failure: Failure::PermissionDenied,
                os_error: 0,
            },
        ],
        vec![
            Reply::Hello {
                capabilities: protocol::HELPER_ADMIN_PERMITTED,
            },
            Reply::CardOpened,
            Reply::Failure {
                failure: Failure::DriverError,
                os_error: libc::EIO,
            },
        ],
    ] {
        let (session, receipt, peer) = scripted(replies);
        let (backend, _) = backend(vec![session]);
        let (control, _control_peer) = helper();
        assert!(
            backend
                .connect(
                    HelperEndpoint::ApprovedChannel(control.as_fd().as_raw_fd()),
                    &binding
                )
                .is_err()
        );
        peer.join().unwrap();
        let mut controller =
            crate::controller::Controller::new(backend, crate::test_support::settings());
        assert_eq!(controller.quiesce(), Ok(false));
        receipt.0.store(true, Ordering::SeqCst);
        assert_eq!(controller.quiesce(), Ok(true));
    }
}

#[test]
fn retirement_capacity_is_reserved_before_any_authority_session_is_allocated() {
    let mut sessions = Vec::new();
    let mut receipts = Vec::new();
    let mut peers = Vec::new();
    for _ in 0..=crate::model::MAX_STREAMS {
        let (session, receipt, peer) = scripted(vec![]);
        sessions.push(session);
        receipts.push(receipt);
        peers.push(peer);
    }
    let (backend, opens) = backend(sessions);
    let (control, _control_peer) = helper();
    let endpoint = HelperEndpoint::ApprovedChannel(control.as_fd().as_raw_fd());
    for peer in peers {
        peer.join().unwrap();
        assert!(backend.probe_helper(endpoint).is_err());
    }
    assert_eq!(
        backend.probe_helper(endpoint).unwrap_err().code,
        "helper_retirement_capacity"
    );
    assert_eq!(opens.load(Ordering::SeqCst), crate::model::MAX_STREAMS + 1);
    for receipt in receipts {
        receipt.0.store(true, Ordering::SeqCst);
    }
    assert_eq!(backend.poll_retirement(), Ok(true));
}

#[test]
fn an_old_authority_is_explicitly_unsupported_without_a_legacy_fallback() {
    struct Unsupported;
    impl authority::AuthorityConnector for Unsupported {
        fn connect(&self, _: BorrowedFd<'_>) -> io::Result<Box<dyn authority::ApprovedSession>> {
            Err(io::Error::from(io::ErrorKind::Unsupported))
        }
    }
    let mut backend = NativeBackend::new();
    backend.authority = Box::new(Unsupported);
    let (control, _control_peer) = helper();
    assert_eq!(
        backend
            .probe_helper(HelperEndpoint::ApprovedChannel(control.as_fd().as_raw_fd()))
            .unwrap_err()
            .code,
        "authority_retirement_unsupported"
    );
    assert_eq!(backend.poll_retirement(), Ok(true));
}

#[test]
fn a_source_abandoned_before_worker_start_keeps_its_retirement_receipt() {
    let (session, receipt, peer) = scripted(vec![
        Reply::Hello {
            capabilities: protocol::HELPER_ADMIN_PERMITTED,
        },
        Reply::CardOpened,
        Reply::FrameEmpty {
            crtc_id: 41,
            sequence: 0,
            timestamp_ns: 0,
        },
    ]);
    let (backend, _) = backend(vec![session]);
    let binding = crate::test_support::FakeBackend::new()
        .resolve(OutputName::new(9).unwrap())
        .unwrap();
    let (control, _control_peer) = helper();
    let source = backend
        .connect(
            HelperEndpoint::ApprovedChannel(control.as_fd().as_raw_fd()),
            &binding,
        )
        .unwrap();
    let verifier = source.retirement().unwrap();
    drop(source);
    peer.join().unwrap();
    let mut controller =
        crate::controller::Controller::new(backend, crate::test_support::settings());
    assert!(!verifier.poll_retired().unwrap());
    assert_eq!(controller.quiesce(), Ok(false));
    receipt.0.store(true, Ordering::SeqCst);
    assert_eq!(controller.quiesce(), Ok(true));
}
