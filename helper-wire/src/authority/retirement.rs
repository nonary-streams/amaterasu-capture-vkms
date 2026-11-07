//! Version two authority handoff and independent, nonblocking retirement proof.
use super::*;
use crate::channel::ChannelShutdown;
use std::sync::{Arc, TryLockError};

/// The frame conversation and its independent authority-owned retirement receipt.
/// Dropping this client requests closure; it never proves helper retirement.
pub struct RetiringHelperClient {
    client: HelperClient,
    shutdown: ChannelShutdown,
    retirement: Arc<RetirementReceipt>,
}

impl RetiringHelperClient {
    pub fn client(&mut self) -> &mut HelperClient {
        &mut self.client
    }

    pub fn shutdown_handle(&self) -> io::Result<ChannelShutdown> {
        self.client.shutdown_handle()
    }

    /// Keep this receipt in the lifecycle owner before performing fallible setup.
    /// It remains readable when the frame channel has been interrupted or dropped.
    pub fn retirement_receipt(&self) -> Arc<RetirementReceipt> {
        self.retirement.clone()
    }
}

impl Drop for RetiringHelperClient {
    fn drop(&mut self) {
        self.shutdown.shutdown();
    }
}

enum RetirementState {
    Pending(SeqpacketChannel),
    Retired,
    Failed(io::ErrorKind, String),
}

/// One exact session's receipt, created and transported by its privileged peer.
/// EOF, local shutdown and transport failures are never retirement acknowledgments.
pub struct RetirementReceipt {
    state: Mutex<RetirementState>,
}

impl RetirementReceipt {
    fn new(channel: SeqpacketChannel) -> Self {
        Self {
            state: Mutex::new(RetirementState::Pending(channel)),
        }
    }

    /// Polls without waiting for either a lock or a socket record. A pending
    /// receipt retains its descriptor; terminal success and failure are stable.
    pub fn poll_retired(&self) -> io::Result<bool> {
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return Ok(false),
            Err(TryLockError::Poisoned(_)) => {
                return Err(io::Error::other("capture retirement receipt lock poisoned"));
            }
        };
        let RetirementState::Pending(channel) = &*state else {
            return retirement_result(&state);
        };
        let mut reply = [0; 9];
        let result = match channel.receive_nonblocking(&mut reply) {
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                return Ok(false);
            }
            Err(error) => Err(error),
            Ok(received)
                if !received.truncated
                    && !received.descriptors_truncated
                    && received.descriptors.is_empty() =>
            {
                match &reply[..received.len] {
                    bytes if bytes == AUTHORITY_SESSION_RETIRED => Ok(()),
                    bytes if bytes == AUTHORITY_RETIREMENT_FAILED => Err(io::Error::other(
                        "capture authority could not prove helper retirement",
                    )),
                    _ => Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "capture retirement receipt missing or malformed",
                    )),
                }
            }
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "capture retirement receipt carried unexpected data or descriptors",
            )),
        };
        *state = match result {
            Ok(()) => RetirementState::Retired,
            Err(error) => RetirementState::Failed(error.kind(), error.to_string()),
        };
        retirement_result(&state)
    }
}

fn retirement_result(state: &RetirementState) -> io::Result<bool> {
    match state {
        RetirementState::Pending(_) => Ok(false),
        RetirementState::Retired => Ok(true),
        RetirementState::Failed(kind, message) => Err(io::Error::new(*kind, message.clone())),
    }
}

/// Rechecks protected approval and proves version-two retirement support without
/// creating a helper. Old authorities are explicitly unsupported; no v1 fallback.
pub fn precheck_retiring_authorized(control: BorrowedFd<'_>) -> io::Result<()> {
    control_exchange(control, precheck_retirement)
}

/// Proves retirement support before asking the authority to allocate a session.
/// The caller must retain `retirement_receipt()` before any fallible frame setup.
pub fn connect_authorized_retiring(control: BorrowedFd<'_>) -> io::Result<RetiringHelperClient> {
    control_exchange(control, |channel| {
        precheck_retirement(channel)?;
        open_retiring_session(channel)
    })
}

fn control_exchange<T>(
    control: BorrowedFd<'_>,
    operation: impl FnOnce(&SeqpacketChannel) -> io::Result<T>,
) -> io::Result<T> {
    let channel = SeqpacketChannel::from_owned(control.try_clone_to_owned()?)?;
    if channel.peer_uid()? != 0 {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    let _exchange = CONTROL_EXCHANGE
        .lock()
        .map_err(|_| io::Error::other("capture authority control lock poisoned"))?;
    let result = operation(&channel);
    if result
        .as_ref()
        .is_err_and(|error| error.kind() != io::ErrorKind::PermissionDenied)
        && let Ok(shutdown) = channel.shutdown_handle()
    {
        shutdown.shutdown();
    }
    result
}

fn precheck_retirement(channel: &SeqpacketChannel) -> io::Result<()> {
    channel.set_receive_timeout(Some(Duration::from_secs(5)))?;
    channel.send(AUTHORITY_PRECHECK_RETIREMENT, &[])?;
    let mut reply = [0; 9];
    let received = channel.receive(&mut reply)?;
    if received.truncated || received.descriptors_truncated || !received.descriptors.is_empty() {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    match &reply[..received.len] {
        bytes if bytes == AUTHORITY_RETIREMENT_SUPPORTED => Ok(()),
        bytes if bytes == AUTHORITY_SESSION_DENIED => {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        }
        _ => Err(io::Error::from(io::ErrorKind::Unsupported)),
    }
}

fn open_retiring_session(channel: &SeqpacketChannel) -> io::Result<RetiringHelperClient> {
    channel.send(AUTHORITY_OPEN_RETIRING_SESSION, &[])?;
    let mut reply = [0; 9];
    let received = channel.receive(&mut reply)?;
    if received.truncated || received.descriptors_truncated {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    let bytes = &reply[..received.len];
    if bytes == AUTHORITY_SESSION_DENIED && received.descriptors.is_empty() {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    if bytes != AUTHORITY_RETIRING_SESSION_READY || received.descriptors.len() != 2 {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    let mut descriptors = received.descriptors.into_iter();
    let frame = SeqpacketChannel::from_owned(descriptors.next().unwrap())?;
    let receipt = SeqpacketChannel::from_owned(descriptors.next().unwrap())?;
    if frame.peer_uid()? != 0 || receipt.peer_uid()? != 0 {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    let shutdown = frame.shutdown_handle()?;
    Ok(RetiringHelperClient {
        client: HelperClient::new(frame),
        shutdown,
        retirement: Arc::new(RetirementReceipt::new(receipt)),
    })
}

#[cfg(test)]
mod tests;
