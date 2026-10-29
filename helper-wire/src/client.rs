//! Unprivileged client of a running helper.
//!
//! One request is outstanding at a time. Any transport or protocol fault
//! leaves the request stream out of step, so the client then refuses further
//! use; the owner replaces the helper process.
use crate::{
    channel::{ChannelShutdown, SeqpacketChannel},
    protocol::{
        self, Failure, FrameDescriptor, HELPER_ADMIN_PERMITTED, MAX_PLANES, RESPONSE_LEN, Reply,
        Request, RequestRecord,
    },
};
use std::{io, os::fd::OwnedFd, time::Duration};

/// Bound for HELLO and OPEN_CARD, which may include helper startup.
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(1);
/// Bound for FRAME, which never waits inside the driver.
pub const FRAME_TIMEOUT: Duration = Duration::from_millis(100);
/// Added to a WAIT's own timeout to bound the round trip.
pub const WAIT_MARGIN: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub enum ClientError {
    /// The helper answered with a failure record.
    Refused {
        failure: Failure,
        os_error: i32,
    },
    /// The helper did not answer in time.
    TimedOut,
    /// The helper closed the socket (it exited or refused to start).
    Closed,
    /// The reply was malformed or did not match the request.
    Protocol,
    Transport(io::Error),
    /// An earlier fault made this client unusable.
    Unusable,
}

impl ClientError {
    pub fn failure(&self) -> Option<Failure> {
        match self {
            Self::Refused { failure, .. } => Some(*failure),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelperHello {
    pub admin_permitted: bool,
}

/// One exported presentation. The descriptors are owned by the receiver
/// and close when this value drops.
#[derive(Debug)]
pub struct ExportedFrame {
    pub descriptor: FrameDescriptor,
    pub dma_bufs: Vec<OwnedFd>,
    pub fences: [Option<OwnedFd>; MAX_PLANES],
}

#[derive(Debug)]
pub enum FrameExport {
    /// The CRTC has not completed a presentation yet.
    Empty,
    Ready(ExportedFrame),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Presentation {
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub flags: u32,
}

pub struct HelperClient {
    channel: SeqpacketChannel,
    next_request_id: u32,
    usable: bool,
}

impl HelperClient {
    pub fn new(channel: SeqpacketChannel) -> Self {
        Self {
            channel,
            next_request_id: 1,
            usable: true,
        }
    }

    pub fn is_usable(&self) -> bool {
        self.usable
    }

    /// A handle another thread can use to wake a request blocked on this
    /// client, such as a WAIT. The interrupted request fails as `Closed`,
    /// the client is unusable afterward, and the helper sees end of stream
    /// and exits once its current driver call returns.
    pub fn shutdown_handle(&self) -> io::Result<ChannelShutdown> {
        self.channel.shutdown_handle()
    }

    pub fn hello(&mut self) -> Result<HelperHello, ClientError> {
        match self.exchange(Request::Hello, CONTROL_TIMEOUT)? {
            (Reply::Hello { capabilities }, _) => Ok(HelperHello {
                admin_permitted: capabilities & HELPER_ADMIN_PERMITTED != 0,
            }),
            _ => Err(self.out_of_step()),
        }
    }

    pub fn open_card(&mut self, major: u32, minor: u32) -> Result<(), ClientError> {
        match self.exchange(Request::OpenCard { major, minor }, CONTROL_TIMEOUT)? {
            (Reply::CardOpened, _) => Ok(()),
            _ => Err(self.out_of_step()),
        }
    }

    pub fn frame(&mut self, crtc_id: u32) -> Result<FrameExport, ClientError> {
        match self.exchange(Request::Frame { crtc_id }, FRAME_TIMEOUT)? {
            (
                Reply::FrameEmpty {
                    crtc_id: echoed, ..
                },
                _,
            ) if echoed == crtc_id => Ok(FrameExport::Empty),
            (Reply::FrameReady(descriptor), descriptors) if descriptor.crtc_id == crtc_id => Ok(
                FrameExport::Ready(split_descriptors(descriptor, descriptors)),
            ),
            _ => Err(self.out_of_step()),
        }
    }

    pub fn wait(
        &mut self,
        crtc_id: u32,
        last_sequence: u64,
        timeout_ms: u32,
    ) -> Result<Presentation, ClientError> {
        let request = Request::Wait {
            crtc_id,
            last_sequence,
            timeout_ms,
        };
        let bound = Duration::from_millis(timeout_ms.into()) + WAIT_MARGIN;
        match self.exchange(request, bound)? {
            (
                Reply::Presentation {
                    crtc_id: echoed,
                    sequence,
                    timestamp_ns,
                    flags,
                },
                _,
            ) if echoed == crtc_id => Ok(Presentation {
                sequence,
                timestamp_ns,
                flags,
            }),
            _ => Err(self.out_of_step()),
        }
    }

    fn out_of_step(&mut self) -> ClientError {
        self.usable = false;
        ClientError::Protocol
    }

    /// Sends one request and returns its reply with exactly the descriptors
    /// the reply declares. Failure replies are returned as `Refused`.
    fn exchange(
        &mut self,
        request: Request,
        timeout: Duration,
    ) -> Result<(Reply, Vec<OwnedFd>), ClientError> {
        if !self.usable {
            return Err(ClientError::Unusable);
        }
        let result = self.round_trip(request, timeout);
        if !matches!(result, Ok(_) | Err(ClientError::Refused { .. })) {
            self.usable = false;
        }
        result
    }

    fn round_trip(
        &mut self,
        request: Request,
        timeout: Duration,
    ) -> Result<(Reply, Vec<OwnedFd>), ClientError> {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        let bytes = protocol::encode_request(&RequestRecord {
            request_id,
            request,
        });
        self.channel
            .set_receive_timeout(Some(timeout))
            .map_err(ClientError::Transport)?;
        self.channel.send(&bytes, &[]).map_err(classify_transport)?;
        let mut buffer = [0u8; RESPONSE_LEN + 1];
        let received = self
            .channel
            .receive(&mut buffer)
            .map_err(classify_transport)?;
        if received.is_end_of_stream() {
            return Err(ClientError::Closed);
        }
        if received.truncated || received.descriptors_truncated {
            return Err(ClientError::Protocol);
        }
        let record = protocol::decode_response(&buffer[..received.len])
            .map_err(|_| ClientError::Protocol)?;
        if record.request_id != request_id || record.opcode != request.opcode() as u16 {
            return Err(ClientError::Protocol);
        }
        let expected_descriptors = match &record.reply {
            Reply::FrameReady(descriptor) => descriptor.descriptor_count(),
            _ => 0,
        };
        if received.descriptors.len() != expected_descriptors {
            return Err(ClientError::Protocol);
        }
        match record.reply {
            Reply::Failure { failure, os_error } => Err(ClientError::Refused { failure, os_error }),
            reply => Ok((reply, received.descriptors)),
        }
    }
}

fn classify_transport(error: io::Error) -> ClientError {
    match error.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => ClientError::TimedOut,
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset => ClientError::Closed,
        _ => ClientError::Transport(error),
    }
}

/// Splits the reply's descriptors: plane DMA-BUFs first, then the fences of
/// the planes named in `fence_mask`, both in plane order.
fn split_descriptors(descriptor: FrameDescriptor, descriptors: Vec<OwnedFd>) -> ExportedFrame {
    let mut descriptors = descriptors.into_iter();
    let dma_bufs: Vec<OwnedFd> = descriptors
        .by_ref()
        .take(descriptor.plane_count as usize)
        .collect();
    let mut fences: [Option<OwnedFd>; MAX_PLANES] = Default::default();
    for (plane, fence) in fences.iter_mut().enumerate() {
        if descriptor.has_fence(plane) {
            *fence = descriptors.next();
        }
    }
    ExportedFrame {
        descriptor,
        dma_bufs,
        fences,
    }
}

#[cfg(test)]
mod tests;
