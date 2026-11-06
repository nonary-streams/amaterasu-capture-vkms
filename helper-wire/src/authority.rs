//! Client side of the protected capture-authority session handoff.
//!
//! The inherited control socket is supplied by the privileged launcher. No
//! identity or executable path is accepted over this protocol. Each successful
//! handoff returns a separate proxied helper session, whose privileged peer
//! remains responsible for authorization and revocation on every operation.
use crate::{channel::SeqpacketChannel, client::HelperClient};
use std::{io, os::fd::BorrowedFd, sync::Mutex, time::Duration};

pub const AUTHORITY_OPEN_SESSION: &[u8; 8] = b"AMACA001";
pub const AUTHORITY_SESSION_READY: &[u8; 8] = b"AMACAOK1";
pub const AUTHORITY_SESSION_DENIED: &[u8; 8] = b"AMACANO1";
pub const AUTHORITY_PRECHECK: &[u8; 8] = b"AMACAP01";
pub const AUTHORITY_PRECHECK_READY: &[u8; 8] = b"AMACAPOK";

// Every stream duplicates the same inherited socket. Serialize the complete
// request/reply exchange, not just send, so one stream cannot consume another
// stream's session descriptor.
static CONTROL_EXCHANGE: Mutex<()> = Mutex::new(());

/// Recheck the current protected approval and immutable helper without opening
/// a helper session. The reply must contain no transported descriptors. An old
/// launcher without this opcode is explicitly unavailable; no OPEN fallback.
pub fn precheck_authorized(control: BorrowedFd<'_>) -> io::Result<()> {
    let channel = SeqpacketChannel::from_owned(control.try_clone_to_owned()?)?;
    if channel.peer_uid()? != 0 {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    let _exchange = CONTROL_EXCHANGE
        .lock()
        .map_err(|_| io::Error::other("capture authority control lock poisoned"))?;
    let result = precheck(&channel);
    if result
        .as_ref()
        .is_err_and(|error| error.kind() != io::ErrorKind::PermissionDenied)
    {
        // Lost alignment cannot let a late readiness reply satisfy another
        // operation. This ends only the already failed control conversation.
        if let Ok(shutdown) = channel.shutdown_handle() {
            shutdown.shutdown();
        }
    }
    result
}

fn precheck(channel: &SeqpacketChannel) -> io::Result<()> {
    channel.set_receive_timeout(Some(Duration::from_secs(5)))?;
    channel.send(AUTHORITY_PRECHECK, &[])?;
    let mut reply = [0; 9];
    let received = channel.receive(&mut reply)?;
    if received.truncated || received.descriptors_truncated || !received.descriptors.is_empty() {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    match &reply[..received.len] {
        bytes if bytes == AUTHORITY_PRECHECK_READY => Ok(()),
        bytes if bytes == AUTHORITY_SESSION_DENIED => {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        }
        _ => Err(io::Error::from(io::ErrorKind::Unsupported)),
    }
}

/// Opens one independently revocable helper protocol session.
///
/// `control` must refer to the launcher-provided socket for this worker. The
/// borrowed descriptor remains owned by the host and is never closed here.
pub fn connect_authorized(control: BorrowedFd<'_>) -> io::Result<HelperClient> {
    let channel = SeqpacketChannel::from_owned(control.try_clone_to_owned()?)?;
    if channel.peer_uid()? != 0 {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    let _exchange = CONTROL_EXCHANGE
        .lock()
        .map_err(|_| io::Error::other("capture authority control lock poisoned"))?;
    let result = open_session(&channel);
    if result
        .as_ref()
        .is_err_and(|error| error.kind() != io::ErrorKind::PermissionDenied)
    {
        // A timeout or malformed reply loses protocol alignment. End this
        // control conversation so a late response cannot become the next
        // caller's apparently successful handoff.
        if let Ok(shutdown) = channel.shutdown_handle() {
            shutdown.shutdown();
        }
    }
    result
}

fn open_session(channel: &SeqpacketChannel) -> io::Result<HelperClient> {
    channel.set_receive_timeout(Some(Duration::from_secs(5)))?;
    channel.send(AUTHORITY_OPEN_SESSION, &[])?;
    let mut reply = [0; 9];
    let received = channel.receive(&mut reply)?;
    if received.truncated || received.descriptors_truncated {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    let bytes = &reply[..received.len];
    if bytes == AUTHORITY_SESSION_DENIED && received.descriptors.is_empty() {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    if bytes != AUTHORITY_SESSION_READY || received.descriptors.len() != 1 {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    // receive installs every descriptor with MSG_CMSG_CLOEXEC. Validate the
    // transported endpoint itself; a malicious peer cannot smuggle an ordinary
    // file or an unprivileged socket through this boundary.
    let socket = received.descriptors.into_iter().next().unwrap();
    let session = SeqpacketChannel::from_owned(socket)?;
    if session.peer_uid()? != 0 {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    Ok(HelperClient::new(session))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    #[test]
    fn pure_precheck_transcript_never_requests_or_accepts_a_session_descriptor() {
        for (bytes, descriptor, expected) in [
            (AUTHORITY_PRECHECK_READY.as_slice(), false, Ok(())),
            (
                AUTHORITY_SESSION_DENIED.as_slice(),
                false,
                Err(io::ErrorKind::PermissionDenied),
            ),
            (
                AUTHORITY_SESSION_READY.as_slice(),
                false,
                Err(io::ErrorKind::Unsupported),
            ),
            (
                AUTHORITY_PRECHECK_READY.as_slice(),
                true,
                Err(io::ErrorKind::InvalidData),
            ),
        ] {
            let (client, server) = SeqpacketChannel::pair().unwrap();
            let bytes = bytes.to_vec();
            let peer = std::thread::spawn(move || {
                let mut request = [0; 9];
                let packet = server.receive(&mut request).unwrap();
                assert_eq!(&request[..packet.len], AUTHORITY_PRECHECK);
                assert!(packet.descriptors.is_empty());
                let file = std::fs::File::open("/dev/null").unwrap();
                let descriptors = if descriptor {
                    vec![file.as_fd()]
                } else {
                    vec![]
                };
                server.send(&bytes, &descriptors).unwrap();
            });
            assert_eq!(precheck(&client).map_err(|error| error.kind()), expected);
            peer.join().unwrap();
        }
    }

    fn reply_session(bytes: &[u8], descriptor: bool) -> io::Result<HelperClient> {
        let (client, server) = SeqpacketChannel::pair().unwrap();
        let bytes = bytes.to_vec();
        let helper = std::thread::spawn(move || {
            let mut request = [0; 16];
            let received = server.receive(&mut request).unwrap();
            assert_eq!(&request[..received.len], AUTHORITY_OPEN_SESSION);
            assert!(received.descriptors.is_empty());
            let file = std::fs::File::open("/dev/null").unwrap();
            let descriptors = if descriptor {
                vec![file.as_fd()]
            } else {
                vec![]
            };
            server.send(&bytes, &descriptors).unwrap();
        });
        let result = open_session(&client);
        helper.join().unwrap();
        result
    }

    #[test]
    fn denied_sessions_are_permission_errors_and_malformed_handoffs_are_refused() {
        assert_eq!(
            reply_session(AUTHORITY_SESSION_DENIED, false)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        for bytes in [
            b"AMACAXXX".as_slice(),
            AUTHORITY_SESSION_READY.as_slice(),
            b"AMACAOK1extra".as_slice(),
        ] {
            assert!(reply_session(bytes, false).is_err());
        }
        assert!(reply_session(AUTHORITY_SESSION_DENIED, true).is_err());
        assert!(reply_session(AUTHORITY_SESSION_READY, true).is_err());
    }

    #[test]
    fn rejects_ordinary_files_without_closing_the_borrowed_descriptor() {
        let file = std::fs::File::open("/dev/null").unwrap();
        assert!(connect_authorized(file.as_fd()).is_err());
        assert!(file.metadata().is_ok());
    }

    #[test]
    fn rejects_unprivileged_peers_before_sending_a_request() {
        // Default tests are ordinary user tests. Root can test this boundary in
        // the separate privileged service qualification instead.
        // SAFETY: geteuid has no preconditions and modifies no memory.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let (client, server) = SeqpacketChannel::pair().unwrap();
        let error = connect_authorized(client.as_fd()).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        server
            .set_receive_timeout(Some(Duration::from_millis(1)))
            .unwrap();
        let mut bytes = [0; 9];
        assert!(server.receive(&mut bytes).is_err());
    }
}
