//! Request handling: validates each record, applies the helper's privilege
//! rules and converts driver answers into protocol replies. Device access is
//! behind traits so the rules are testable without a DRM device.
use crate::capabilities::{AdminPrivilege, CapabilityControl};
use amaterasu_vkms_frame_helper::protocol::{
    self, Failure, FrameDescriptor, HELPER_ADMIN_PERMITTED, MAX_PLANES, PRESENT_CHANGED,
    PRESENT_PENDING, PRESENT_TIMEOUT, Reply, Request, RequestRecord, ResponseRecord,
};
use std::{io, os::fd::OwnedFd};

/// Driver ABI version of both private ioctls.
pub const DRIVER_ABI_VERSION: u32 = 1;
const DRIVER_FRAME_READY: u32 = 1 << 0;
const DRIVER_FRAME_EMPTY: u32 = 1 << 1;

/// Answer of the driver's frame export ioctl after it succeeded. Every
/// descriptor the driver returned is owned here, so dropping a rejected
/// export closes it.
#[derive(Debug, Default)]
pub struct DriverFrame {
    pub abi_version: u32,
    pub crtc_id: u32,
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub flags: u32,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub plane_count: u32,
    pub dma_bufs: [Option<OwnedFd>; MAX_PLANES],
    pub pitches: [u32; MAX_PLANES],
    pub offsets: [u32; MAX_PLANES],
    pub sync_files: [Option<OwnedFd>; MAX_PLANES],
    pub reserved_zero: bool,
}

/// Answer of the driver's presentation wait ioctl after it succeeded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DriverPresentation {
    pub abi_version: u32,
    pub crtc_id: u32,
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub timeout_ms: u32,
    pub flags: u32,
    pub reserved_zero: bool,
}

/// An opened vibeshine_drm primary node.
pub trait PresentationCard {
    fn export_frame(&self, crtc_id: u32) -> io::Result<DriverFrame>;
    fn wait_present(
        &self,
        crtc_id: u32,
        last_sequence: u64,
        timeout_ms: u32,
    ) -> io::Result<DriverPresentation>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenFailure {
    pub failure: Failure,
    pub os_error: i32,
}

pub trait CardOpener {
    type Card: PresentationCard;
    /// Opens and verifies the DRM primary node with this device number.
    fn open(&self, major: u32, minor: u32) -> Result<Self::Card, OpenFailure>;
}

/// Raises CAP_SYS_ADMIN around one operation.
pub trait Privilege {
    fn admin_permitted(&self) -> bool;
    fn with_admin<T>(&self, operation: impl FnOnce() -> T) -> Option<T>;
}

impl<C: CapabilityControl> Privilege for AdminPrivilege<C> {
    fn admin_permitted(&self) -> bool {
        AdminPrivilege::admin_permitted(self)
    }
    fn with_admin<T>(&self, operation: impl FnOnce() -> T) -> Option<T> {
        AdminPrivilege::with_admin(self, operation)
    }
}

/// One reply and the descriptors that travel with it. The helper closes its
/// copies by dropping this after the reply is sent.
#[derive(Debug)]
pub struct Outcome {
    pub reply: Reply,
    pub descriptors: Vec<OwnedFd>,
}

impl Outcome {
    fn reply(reply: Reply) -> Self {
        Self {
            reply,
            descriptors: Vec::new(),
        }
    }

    fn failure(failure: Failure, os_error: i32) -> Self {
        Self::reply(Reply::Failure { failure, os_error })
    }
}

struct OpenedCard<C> {
    major: u32,
    minor: u32,
    card: C,
}

pub struct Service<O: CardOpener, P: Privilege> {
    opener: O,
    privilege: P,
    card: Option<OpenedCard<O::Card>>,
}

impl<O: CardOpener, P: Privilege> Service<O, P> {
    pub fn new(opener: O, privilege: P) -> Self {
        Self {
            opener,
            privilege,
            card: None,
        }
    }

    /// Answers one received datagram. `transport_malformed` reports attached
    /// descriptors or truncation, which make any request malformed: requests
    /// never carry descriptors.
    pub fn respond(
        &mut self,
        datagram: &[u8],
        transport_malformed: bool,
    ) -> (ResponseRecord, Vec<OwnedFd>) {
        let record = match protocol::decode_request(datagram) {
            Ok(record) if !transport_malformed => record,
            Ok(record) => {
                return rejected(
                    record.request_id,
                    record.request.opcode() as u16,
                    Failure::MalformedRequest,
                );
            }
            Err(rejection) => {
                return rejected(rejection.request_id, rejection.opcode, rejection.failure);
            }
        };
        let RequestRecord {
            request_id,
            request,
        } = record;
        let outcome = self.handle(request);
        (
            ResponseRecord {
                request_id,
                opcode: request.opcode() as u16,
                reply: outcome.reply,
            },
            outcome.descriptors,
        )
    }

    fn handle(&mut self, request: Request) -> Outcome {
        match request {
            Request::Hello => Outcome::reply(Reply::Hello {
                capabilities: if self.privilege.admin_permitted() {
                    HELPER_ADMIN_PERMITTED
                } else {
                    0
                },
            }),
            Request::OpenCard { major, minor } => self.open_card(major, minor),
            Request::Frame { crtc_id } => self.export_frame(crtc_id),
            Request::Wait {
                crtc_id,
                last_sequence,
                timeout_ms,
            } => self.wait_present(crtc_id, last_sequence, timeout_ms),
        }
    }

    fn open_card(&mut self, major: u32, minor: u32) -> Outcome {
        if let Some(opened) = &self.card {
            return if opened.major == major && opened.minor == minor {
                Outcome::reply(Reply::CardOpened)
            } else {
                Outcome::failure(Failure::CardAlreadyOpen, 0)
            };
        }
        match self.opener.open(major, minor) {
            Ok(card) => {
                self.card = Some(OpenedCard { major, minor, card });
                Outcome::reply(Reply::CardOpened)
            }
            Err(failure) => Outcome::failure(failure.failure, failure.os_error),
        }
    }

    fn export_frame(&self, crtc_id: u32) -> Outcome {
        let card = match self.privileged_card() {
            Ok(card) => card,
            Err(outcome) => return outcome,
        };
        match self.privilege.with_admin(|| card.export_frame(crtc_id)) {
            None => Outcome::failure(Failure::PermissionDenied, 0),
            Some(Err(error)) => driver_failure(&error),
            Some(Ok(frame)) => frame_outcome(crtc_id, frame),
        }
    }

    fn wait_present(&self, crtc_id: u32, last_sequence: u64, timeout_ms: u32) -> Outcome {
        let card = match self.privileged_card() {
            Ok(card) => card,
            Err(outcome) => return outcome,
        };
        let waited = self
            .privilege
            .with_admin(|| card.wait_present(crtc_id, last_sequence, timeout_ms));
        match waited {
            None => Outcome::failure(Failure::PermissionDenied, 0),
            Some(Err(error)) => driver_failure(&error),
            Some(Ok(presentation)) => presentation_outcome(crtc_id, timeout_ms, presentation),
        }
    }

    /// The open card, provided CAP_SYS_ADMIN is permitted at all. Checking
    /// permission first gives the module a stable "run setcap" answer.
    fn privileged_card(&self) -> Result<&O::Card, Outcome> {
        let card = self
            .card
            .as_ref()
            .ok_or_else(|| Outcome::failure(Failure::CardNotOpen, 0))?;
        if !self.privilege.admin_permitted() {
            return Err(Outcome::failure(Failure::PermissionDenied, 0));
        }
        Ok(&card.card)
    }
}

fn rejected(request_id: u32, opcode: u16, failure: Failure) -> (ResponseRecord, Vec<OwnedFd>) {
    (
        ResponseRecord {
            request_id,
            opcode,
            reply: Reply::Failure {
                failure,
                os_error: 0,
            },
        },
        Vec::new(),
    )
}

fn driver_failure(error: &io::Error) -> Outcome {
    let errno = error.raw_os_error().unwrap_or(0);
    let failure = match errno {
        libc::EACCES | libc::EPERM => Failure::PermissionDenied,
        libc::ENOENT => Failure::CrtcNotFound,
        libc::EBUSY => Failure::Busy,
        libc::EINTR => Failure::Interrupted,
        _ => Failure::DriverError,
    };
    Outcome::failure(failure, errno)
}

fn frame_outcome(crtc_id: u32, frame: DriverFrame) -> Outcome {
    let common_valid =
        frame.abi_version == DRIVER_ABI_VERSION && frame.crtc_id == crtc_id && frame.reserved_zero;
    if !common_valid {
        return Outcome::failure(Failure::InvalidDriverResponse, 0);
    }
    match frame.flags {
        DRIVER_FRAME_EMPTY => empty_frame_outcome(frame),
        DRIVER_FRAME_READY => ready_frame_outcome(frame),
        _ => Outcome::failure(Failure::InvalidDriverResponse, 0),
    }
}

fn empty_frame_outcome(frame: DriverFrame) -> Outcome {
    let layout_empty = frame.width == 0
        && frame.height == 0
        && frame.fourcc == 0
        && frame.modifier == 0
        && frame.plane_count == 0
        && frame.dma_bufs.iter().all(Option::is_none)
        && frame.sync_files.iter().all(Option::is_none)
        && frame.pitches == [0; MAX_PLANES]
        && frame.offsets == [0; MAX_PLANES];
    if !layout_empty {
        return Outcome::failure(Failure::InvalidDriverResponse, 0);
    }
    Outcome::reply(Reply::FrameEmpty {
        crtc_id: frame.crtc_id,
        sequence: frame.sequence,
        timestamp_ns: frame.timestamp_ns,
    })
}

fn ready_frame_outcome(frame: DriverFrame) -> Outcome {
    let planes = (frame.plane_count as usize).min(MAX_PLANES + 1);
    let inactive_planes_clear = (planes..MAX_PLANES)
        .all(|plane| frame.dma_bufs[plane].is_none() && frame.sync_files[plane].is_none());
    let active_buffers_present = frame.dma_bufs.iter().take(planes).all(Option::is_some);
    let fence_mask = frame
        .sync_files
        .iter()
        .enumerate()
        .filter(|(_, fence)| fence.is_some())
        .fold(0u32, |mask, (plane, _)| mask | (1 << plane));
    let descriptor = FrameDescriptor {
        crtc_id: frame.crtc_id,
        sequence: frame.sequence,
        timestamp_ns: frame.timestamp_ns,
        width: frame.width,
        height: frame.height,
        fourcc: frame.fourcc,
        modifier: frame.modifier,
        plane_count: frame.plane_count,
        pitches: frame.pitches,
        offsets: frame.offsets,
        fence_mask,
    };
    if !inactive_planes_clear
        || !active_buffers_present
        || !protocol::frame_descriptor_is_valid(&descriptor)
    {
        return Outcome::failure(Failure::InvalidDriverResponse, 0);
    }
    let DriverFrame {
        dma_bufs,
        sync_files,
        ..
    } = frame;
    let mut descriptors: Vec<OwnedFd> = dma_bufs.into_iter().flatten().collect();
    descriptors.extend(sync_files.into_iter().flatten());
    Outcome {
        reply: Reply::FrameReady(descriptor),
        descriptors,
    }
}

fn presentation_outcome(
    crtc_id: u32,
    timeout_ms: u32,
    presentation: DriverPresentation,
) -> Outcome {
    let flags = presentation.flags;
    let outcome_bits = flags & (PRESENT_CHANGED | PRESENT_TIMEOUT);
    let valid = presentation.abi_version == DRIVER_ABI_VERSION
        && presentation.crtc_id == crtc_id
        && presentation.timeout_ms == timeout_ms
        && presentation.reserved_zero
        && flags & !(PRESENT_CHANGED | PRESENT_TIMEOUT | PRESENT_PENDING) == 0
        && (outcome_bits == PRESENT_CHANGED || outcome_bits == PRESENT_TIMEOUT);
    if !valid {
        return Outcome::failure(Failure::InvalidDriverResponse, 0);
    }
    Outcome::reply(Reply::Presentation {
        crtc_id,
        sequence: presentation.sequence,
        timestamp_ns: presentation.timestamp_ns,
        flags,
    })
}

#[cfg(test)]
mod tests;
