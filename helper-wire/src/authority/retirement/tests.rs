use super::*;
use std::{os::fd::AsFd, sync::mpsc, time::Instant};

fn retiring_client() -> (RetiringHelperClient, SeqpacketChannel, SeqpacketChannel) {
    let (frame, authority_frame) = SeqpacketChannel::pair().unwrap();
    let (receipt, authority_receipt) = SeqpacketChannel::pair().unwrap();
    let shutdown = frame.shutdown_handle().unwrap();
    (
        RetiringHelperClient {
            client: HelperClient::new(frame),
            shutdown,
            retirement: Arc::new(RetirementReceipt::new(receipt)),
        },
        authority_frame,
        authority_receipt,
    )
}

#[test]
fn interrupting_frame_io_never_proves_remote_retirement() {
    let (client, frame, receipt) = retiring_client();
    let retained = client.retirement_receipt();
    let started = Instant::now();
    for _ in 0..100 {
        assert!(!retained.poll_retired().unwrap());
    }
    assert!(started.elapsed() < Duration::from_millis(100));
    client.shutdown_handle().unwrap().shutdown();
    assert!(frame.receive(&mut [0; 9]).unwrap().is_end_of_stream());
    assert!(
        !retained.poll_retired().unwrap(),
        "frame shutdown is not remote reap proof"
    );
    drop(client);
    assert!(
        !retained.poll_retired().unwrap(),
        "dropping the client does not forge a receipt"
    );
    receipt.send(AUTHORITY_SESSION_RETIRED, &[]).unwrap();
    assert!(retained.poll_retired().unwrap());
    drop(receipt);
    assert!(
        retained.poll_retired().unwrap(),
        "positive receipt remains stable"
    );
}

#[test]
fn missing_malformed_failed_and_descriptor_receipts_never_become_success() {
    for (bytes, descriptors) in [
        (None, false),
        (Some(b"AMACAOK1".as_slice()), false),
        (Some(AUTHORITY_RETIREMENT_FAILED.as_slice()), false),
        (Some(AUTHORITY_SESSION_RETIRED.as_slice()), true),
    ] {
        let (client, _frame, receipt) = retiring_client();
        let retained = client.retirement_receipt();
        let file = std::fs::File::open("/dev/null").unwrap();
        if let Some(bytes) = bytes {
            receipt
                .send(
                    bytes,
                    &if descriptors {
                        vec![file.as_fd()]
                    } else {
                        vec![]
                    },
                )
                .unwrap();
        }
        drop(receipt);
        let first = retained.poll_retired().unwrap_err().to_string();
        assert_eq!(retained.poll_retired().unwrap_err().to_string(), first);
    }
}

#[test]
fn a_busy_receipt_lock_keeps_poll_nonblocking() {
    let (client, _, _) = retiring_client();
    let receipt = client.retirement_receipt();
    let _held = receipt.state.lock().unwrap();
    assert!(!receipt.poll_retired().unwrap());
}

#[test]
fn old_authority_capability_is_refused_before_requesting_a_session() {
    let (client, authority) = SeqpacketChannel::pair().unwrap();
    let peer = std::thread::spawn(move || {
        let mut bytes = [0; 9];
        let request = authority.receive(&mut bytes).unwrap();
        assert_eq!(&bytes[..request.len], AUTHORITY_PRECHECK_RETIREMENT);
        authority.send(AUTHORITY_PRECHECK_READY, &[]).unwrap();
        authority
            .set_receive_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        assert!(
            authority.receive(&mut bytes).is_err(),
            "unsupported authority must not receive OPEN"
        );
    });
    let result = precheck_retirement(&client).and_then(|()| open_retiring_session(&client));
    assert_eq!(result.err().unwrap().kind(), io::ErrorKind::Unsupported);
    peer.join().unwrap();
}

#[test]
fn retirement_receipt_can_outlive_failed_setup_on_another_thread() {
    let (client, _frame, receipt) = retiring_client();
    let retained = client.retirement_receipt();
    let (registered, registration) = mpsc::channel();
    let failed_setup = std::thread::spawn(move || {
        registered.send(retained).unwrap();
        drop(client);
    });
    let retained = registration.recv().unwrap();
    failed_setup.join().unwrap();
    assert!(!retained.poll_retired().unwrap());
    receipt.send(AUTHORITY_SESSION_RETIRED, &[]).unwrap();
    assert!(retained.poll_retired().unwrap());
}
