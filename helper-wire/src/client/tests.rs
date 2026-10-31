use super::*;
use crate::protocol::{
    Opcode, PRESENT_CHANGED, REQUEST_LEN, ResponseRecord, decode_request, encode_response,
};
use std::{
    fs::File,
    io::{Read, Write},
    os::fd::{AsFd, FromRawFd},
    thread::JoinHandle,
};

fn pipe() -> (File, File) {
    let mut fds = [-1; 2];
    // SAFETY: `fds` is a writable two-element array as pipe2 requires.
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    // SAFETY: pipe2 succeeded and returned two new descriptors owned here.
    unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
}

/// Raw reply bytes and the descriptors to attach.
type Scripted = Option<(Vec<u8>, Vec<OwnedFd>)>;
type Answer = Box<dyn FnOnce(RequestRecord) -> Scripted + Send>;

/// A scripted helper: each answer receives the decoded request and returns
/// the raw reply bytes and descriptors to send, or `None` to stay silent.
fn fake_helper(answers: Vec<Answer>) -> (HelperClient, JoinHandle<()>) {
    let (client, helper) = SeqpacketChannel::pair().unwrap();
    let thread = std::thread::spawn(move || {
        for answer in answers {
            let mut buffer = [0u8; REQUEST_LEN];
            let received = helper.receive(&mut buffer).unwrap();
            if received.is_end_of_stream() {
                return;
            }
            let request = decode_request(&buffer[..received.len]).unwrap();
            if let Some((bytes, descriptors)) = answer(request) {
                let borrowed: Vec<_> = descriptors.iter().map(AsFd::as_fd).collect();
                helper.send(&bytes, &borrowed).unwrap();
            } else {
                std::thread::sleep(FRAME_TIMEOUT * 3);
            }
        }
    });
    (HelperClient::new(client), thread)
}

fn reply(request: RequestRecord, reply: Reply) -> Vec<u8> {
    encode_response(&ResponseRecord {
        request_id: request.request_id,
        opcode: request.request.opcode() as u16,
        reply,
    })
    .to_vec()
}

fn descriptor(plane_count: u32, fence_mask: u32) -> FrameDescriptor {
    let mut pitches = [0; MAX_PLANES];
    pitches[..plane_count as usize].fill(256);
    FrameDescriptor {
        crtc_id: 41,
        sequence: 3,
        timestamp_ns: 99,
        width: 64,
        height: 64,
        fourcc: u32::from_le_bytes(*b"XR24"),
        modifier: 0,
        plane_count,
        pitches,
        offsets: [0; MAX_PLANES],
        fence_mask,
        guard_count: 0,
        color: 1,
        hdr_metadata: [0; 12],
    }
}

#[test]
fn hello_reports_admin_permission() {
    let (mut client, thread) = fake_helper(vec![Box::new(|request| {
        Some((
            reply(
                request,
                Reply::Hello {
                    capabilities: HELPER_ADMIN_PERMITTED,
                },
            ),
            vec![],
        ))
    })]);
    assert!(client.hello().unwrap().admin_permitted);
    thread.join().unwrap();
}

#[test]
fn a_frame_delivers_plane_buffers_and_fences_in_plane_order() {
    let (buffer0_read, mut buffer0_write) = pipe();
    let (buffer1_read, mut buffer1_write) = pipe();
    let (fence1_read, mut fence1_write) = pipe();
    let (mut client, thread) = fake_helper(vec![Box::new(move |request| {
        assert_eq!(request.request, Request::Frame { crtc_id: 41 });
        Some((
            reply(request, Reply::FrameReady(descriptor(2, 0b10))),
            vec![buffer0_read.into(), buffer1_read.into(), fence1_read.into()],
        ))
    })]);
    let FrameExport::Ready(frame) = client.frame(41).unwrap() else {
        panic!("expected a ready frame");
    };
    assert_eq!(frame.dma_bufs.len(), 2);
    assert!(frame.fences[0].is_none());
    assert!(frame.fences[2].is_none() && frame.fences[3].is_none());

    buffer0_write.write_all(b"0").unwrap();
    buffer1_write.write_all(b"1").unwrap();
    fence1_write.write_all(b"f").unwrap();
    let ExportedFrame {
        dma_bufs,
        mut fences,
        ..
    } = frame;
    let mut byte = [0; 1];
    let mut files = dma_bufs.into_iter().map(File::from);
    files.next().unwrap().read_exact(&mut byte).unwrap();
    assert_eq!(&byte, b"0");
    files.next().unwrap().read_exact(&mut byte).unwrap();
    assert_eq!(&byte, b"1");
    File::from(fences[1].take().unwrap())
        .read_exact(&mut byte)
        .unwrap();
    assert_eq!(&byte, b"f");
    thread.join().unwrap();
}

#[test]
fn a_frame_with_the_wrong_descriptor_count_is_a_protocol_fault() {
    let (read, _write) = pipe();
    let (mut client, thread) = fake_helper(vec![Box::new(move |request| {
        Some((
            reply(request, Reply::FrameReady(descriptor(1, 0b1))),
            vec![read.into()],
        ))
    })]);
    assert!(matches!(client.frame(41), Err(ClientError::Protocol)));
    assert!(!client.is_usable());
    assert!(matches!(client.frame(41), Err(ClientError::Unusable)));
    thread.join().unwrap();
}

#[test]
fn an_empty_presentation_is_pending() {
    let (mut client, thread) = fake_helper(vec![Box::new(|request| {
        Some((
            reply(
                request,
                Reply::FrameEmpty {
                    crtc_id: 41,
                    sequence: 0,
                    timestamp_ns: 0,
                },
            ),
            vec![],
        ))
    })]);
    assert!(matches!(client.frame(41).unwrap(), FrameExport::Empty));
    thread.join().unwrap();
}

#[test]
fn refusals_keep_the_client_usable() {
    let refuse: fn(RequestRecord) -> Scripted = |request| {
        Some((
            reply(
                request,
                Reply::Failure {
                    failure: Failure::PermissionDenied,
                    os_error: 0,
                },
            ),
            vec![],
        ))
    };
    let (mut client, thread) = fake_helper(vec![Box::new(refuse), Box::new(refuse)]);
    for _ in 0..2 {
        let error = client.frame(41).unwrap_err();
        assert_eq!(error.failure(), Some(Failure::PermissionDenied));
        assert!(client.is_usable());
    }
    thread.join().unwrap();
}

#[test]
fn mismatched_replies_are_rejected() {
    let (mut client, thread) = fake_helper(vec![Box::new(|request| {
        let mut wrong = request;
        wrong.request_id += 1;
        Some((reply(wrong, Reply::Hello { capabilities: 0 }), vec![]))
    })]);
    assert!(matches!(client.hello(), Err(ClientError::Protocol)));
    thread.join().unwrap();

    let (mut client, thread) = fake_helper(vec![Box::new(|request| {
        let bytes = encode_response(&ResponseRecord {
            request_id: request.request_id,
            opcode: Opcode::Frame as u16,
            reply: Reply::CardOpened,
        });
        Some((bytes.to_vec(), vec![]))
    })]);
    assert!(matches!(
        client.open_card(226, 3),
        Err(ClientError::Protocol)
    ));
    thread.join().unwrap();

    let (mut client, thread) = fake_helper(vec![Box::new(|request| {
        Some((
            reply(
                request,
                Reply::FrameEmpty {
                    crtc_id: 40,
                    sequence: 0,
                    timestamp_ns: 0,
                },
            ),
            vec![],
        ))
    })]);
    assert!(matches!(client.frame(41), Err(ClientError::Protocol)));
    thread.join().unwrap();
}

#[test]
fn a_silent_helper_times_out_and_poisons_the_client() {
    let (mut client, thread) = fake_helper(vec![Box::new(|_| None)]);
    assert!(matches!(client.frame(41), Err(ClientError::TimedOut)));
    assert!(!client.is_usable());
    thread.join().unwrap();
}

#[test]
fn an_exited_helper_is_closed() {
    let (client, helper) = SeqpacketChannel::pair().unwrap();
    drop(helper);
    let mut client = HelperClient::new(client);
    assert!(matches!(client.hello(), Err(ClientError::Closed)));
}

#[test]
fn waits_return_the_newest_presentation() {
    let (mut client, thread) = fake_helper(vec![Box::new(|request| {
        assert_eq!(
            request.request,
            Request::Wait {
                crtc_id: 41,
                last_sequence: 2,
                timeout_ms: 16
            }
        );
        Some((
            reply(
                request,
                Reply::Presentation {
                    crtc_id: 41,
                    sequence: 3,
                    timestamp_ns: 7,
                    flags: PRESENT_CHANGED,
                },
            ),
            vec![],
        ))
    })]);
    assert_eq!(
        client.wait(41, 2, 16).unwrap(),
        Presentation {
            sequence: 3,
            timestamp_ns: 7,
            flags: PRESENT_CHANGED
        }
    );
    thread.join().unwrap();
}

#[test]
fn a_shutdown_handle_interrupts_a_pending_wait() {
    let (client, _helper) = SeqpacketChannel::pair().unwrap();
    let mut client = HelperClient::new(client);
    let handle = client.shutdown_handle().unwrap();
    let waiter = std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let result = client.wait(41, 0, 1000);
        (result, started.elapsed(), client)
    });
    std::thread::sleep(Duration::from_millis(20));
    handle.shutdown();
    let (result, waited, client) = waiter.join().unwrap();
    assert!(matches!(result, Err(ClientError::Closed)));
    assert!(waited < Duration::from_millis(500));
    assert!(!client.is_usable());
}

#[test]
#[cfg(feature = "driver-read-leases")]
fn read_guard_survives_helper_reply_until_source_frame_is_dropped() {
    let (buffer, _) = pipe();
    let (guard, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(std::time::Duration::from_millis(10)))
        .unwrap();
    let (mut client, thread) = fake_helper(vec![Box::new(move |request| {
        let mut layout = descriptor(1, 0);
        layout.guard_count = 1;
        Some((
            reply(request, Reply::FrameReady(layout)),
            vec![buffer.into(), guard.into()],
        ))
    })]);
    let FrameExport::Ready(frame) = client.frame(41).unwrap() else {
        panic!("missing frame")
    };
    thread.join().unwrap();
    assert_eq!(frame.read_guards.len(), 1);
    let mut byte = [0];
    assert!(matches!(
        peer.read(&mut byte).unwrap_err().kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    ));
    drop(frame);
    assert_eq!(peer.read(&mut byte).unwrap(), 0);
}
