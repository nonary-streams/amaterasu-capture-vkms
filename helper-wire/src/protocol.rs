//! Fixed-size records exchanged between the capture module and the helper.
//!
//! Every request and response is one SOCK_SEQPACKET datagram with an exact
//! length. Integers are little-endian. Fields an operation does not use, and
//! every reserved byte, must be zero; anything else is rejected before the
//! helper touches the device. A successful frame response carries descriptors
//! in one SCM_RIGHTS message: the DMA-BUF of each plane in plane order,
//! followed by the sync_file of each plane whose bit is set in `fence_mask`,
//! also in plane order.

pub const PROTOCOL_VERSION: u16 = 1;
pub const REQUEST_MAGIC: u32 = u32::from_le_bytes(*b"AVFQ");
pub const RESPONSE_MAGIC: u32 = u32::from_le_bytes(*b"AVFR");
pub const REQUEST_LEN: usize = 48;
pub const RESPONSE_LEN: usize = 128;
pub const MAX_PLANES: usize = 4;
/// One DMA-BUF and at most one sync_file per plane.
pub const MAX_RESPONSE_DESCRIPTORS: usize = MAX_PLANES * 2;
/// The driver rejects longer waits; the helper rejects them first.
pub const MAX_WAIT_TIMEOUT_MS: u32 = 1000;

/// HELLO capability bit: CAP_SYS_ADMIN is in the helper's permitted set.
pub const HELPER_ADMIN_PERMITTED: u32 = 1 << 0;

/// WAIT result bits, copied from the driver presentation flags.
pub const PRESENT_CHANGED: u32 = 1 << 0;
pub const PRESENT_TIMEOUT: u32 = 1 << 1;
pub const PRESENT_PENDING: u32 = 1 << 2;
const PRESENT_KNOWN_FLAGS: u32 = PRESENT_CHANGED | PRESENT_TIMEOUT | PRESENT_PENDING;

const FRAME_READY: u32 = 1;
const FRAME_EMPTY: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Opcode {
    Hello = 1,
    OpenCard = 2,
    Frame = 3,
    Wait = 4,
}

impl Opcode {
    fn from_wire(value: u16) -> Option<Self> {
        Some(match value {
            1 => Self::Hello,
            2 => Self::OpenCard,
            3 => Self::Frame,
            4 => Self::Wait,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    /// Reports protocol version and whether CAP_SYS_ADMIN is permitted.
    Hello,
    /// Opens `/dev/dri/card<minor>` after proving it is this exact DRM device
    /// and that its driver is `vibeshine_drm`. A helper serves one card.
    OpenCard { major: u32, minor: u32 },
    /// Exports the newest completed primary-plane presentation of a CRTC.
    Frame { crtc_id: u32 },
    /// Waits up to `timeout_ms` for a presentation newer than `last_sequence`.
    Wait {
        crtc_id: u32,
        last_sequence: u64,
        timeout_ms: u32,
    },
}

impl Request {
    pub fn opcode(&self) -> Opcode {
        match self {
            Self::Hello => Opcode::Hello,
            Self::OpenCard { .. } => Opcode::OpenCard,
            Self::Frame { .. } => Opcode::Frame,
            Self::Wait { .. } => Opcode::Wait,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestRecord {
    pub request_id: u32,
    pub request: Request,
}

/// Why a request was refused before any device work happened. The helper
/// echoes the request id and opcode when they could be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestRejection {
    pub request_id: u32,
    pub opcode: u16,
    pub failure: Failure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Failure {
    MalformedRequest = 1,
    UnsupportedVersion = 2,
    /// CAP_SYS_ADMIN is not permitted, could not be raised, or the driver
    /// refused it. The operator must grant the file capability.
    PermissionDenied = 3,
    CardNotOpen = 4,
    CardAlreadyOpen = 5,
    /// The device number does not name a DRM primary node.
    DeviceRejected = 6,
    /// Opening the device node failed; `os_error` carries errno.
    DeviceUnavailable = 7,
    /// The device's DRM driver is not `vibeshine_drm`.
    WrongDriver = 8,
    CrtcNotFound = 9,
    Busy = 10,
    Interrupted = 11,
    /// Another driver errno; `os_error` carries it.
    DriverError = 12,
    /// The driver answered with a malformed record; its descriptors were closed.
    InvalidDriverResponse = 13,
}

impl Failure {
    fn from_wire(value: u32) -> Option<Self> {
        Some(match value {
            1 => Self::MalformedRequest,
            2 => Self::UnsupportedVersion,
            3 => Self::PermissionDenied,
            4 => Self::CardNotOpen,
            5 => Self::CardAlreadyOpen,
            6 => Self::DeviceRejected,
            7 => Self::DeviceUnavailable,
            8 => Self::WrongDriver,
            9 => Self::CrtcNotFound,
            10 => Self::Busy,
            11 => Self::Interrupted,
            12 => Self::DriverError,
            13 => Self::InvalidDriverResponse,
            _ => return None,
        })
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::MalformedRequest => "malformed_request",
            Self::UnsupportedVersion => "unsupported_version",
            Self::PermissionDenied => "permission_denied",
            Self::CardNotOpen => "card_not_open",
            Self::CardAlreadyOpen => "card_already_open",
            Self::DeviceRejected => "device_rejected",
            Self::DeviceUnavailable => "device_unavailable",
            Self::WrongDriver => "wrong_driver",
            Self::CrtcNotFound => "crtc_not_found",
            Self::Busy => "busy",
            Self::Interrupted => "interrupted",
            Self::DriverError => "driver_error",
            Self::InvalidDriverResponse => "invalid_driver_response",
        }
    }
}

/// Layout of one exported presentation. Descriptors travel separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameDescriptor {
    pub crtc_id: u32,
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub plane_count: u32,
    pub pitches: [u32; MAX_PLANES],
    pub offsets: [u32; MAX_PLANES],
    /// Bit `i` set when plane `i` has an acquire sync_file.
    pub fence_mask: u32,
}

impl FrameDescriptor {
    /// Number of SCM_RIGHTS descriptors that accompany this frame.
    pub fn descriptor_count(&self) -> usize {
        self.plane_count as usize + self.fence_mask.count_ones() as usize
    }

    pub fn has_fence(&self, plane: usize) -> bool {
        plane < MAX_PLANES && self.fence_mask & (1 << plane) != 0
    }

    fn validate(&self) -> Result<(), ()> {
        let planes = self.plane_count as usize;
        if !(1..=MAX_PLANES).contains(&planes) {
            return Err(());
        }
        let used_mask = (1u32 << planes) - 1;
        let shape_valid = self.crtc_id != 0
            && self.sequence != 0
            && self.timestamp_ns != 0
            && self.width != 0
            && self.height != 0
            && self.fourcc != 0
            && self.fence_mask & !used_mask == 0;
        if !shape_valid {
            return Err(());
        }
        let (used_pitches, unused_pitches) = self.pitches.split_at(planes);
        let (_, unused_offsets) = self.offsets.split_at(planes);
        if used_pitches.contains(&0)
            || unused_pitches.iter().any(|value| *value != 0)
            || unused_offsets.iter().any(|value| *value != 0)
        {
            return Err(());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    Hello {
        capabilities: u32,
    },
    CardOpened,
    FrameEmpty {
        crtc_id: u32,
        sequence: u64,
        timestamp_ns: u64,
    },
    FrameReady(FrameDescriptor),
    Presentation {
        crtc_id: u32,
        sequence: u64,
        timestamp_ns: u64,
        flags: u32,
    },
    Failure {
        failure: Failure,
        /// errno detail for device and driver failures, otherwise zero.
        os_error: i32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResponseRecord {
    pub request_id: u32,
    /// Echo of the request opcode, or zero when it could not be read.
    pub opcode: u16,
    pub reply: Reply,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MalformedResponse;

/// Little-endian field writer over one fixed record.
struct RecordWriter<const N: usize>([u8; N]);

impl<const N: usize> RecordWriter<N> {
    fn new() -> Self {
        Self([0; N])
    }
    fn u16(&mut self, offset: usize, value: u16) {
        self.0[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }
    fn u32(&mut self, offset: usize, value: u32) {
        self.0[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn u64(&mut self, offset: usize, value: u64) {
        self.0[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
}

/// Little-endian field reader over one record of verified length.
struct RecordReader<'a>(&'a [u8]);

impl RecordReader<'_> {
    fn u16(&self, offset: usize) -> u16 {
        u16::from_le_bytes(self.0[offset..offset + 2].try_into().expect("two bytes"))
    }
    fn u32(&self, offset: usize) -> u32 {
        u32::from_le_bytes(self.0[offset..offset + 4].try_into().expect("four bytes"))
    }
    fn u64(&self, offset: usize) -> u64 {
        u64::from_le_bytes(self.0[offset..offset + 8].try_into().expect("eight bytes"))
    }
    fn zero(&self, range: std::ops::Range<usize>) -> bool {
        self.0[range].iter().all(|byte| *byte == 0)
    }
}

mod request_layout {
    pub const MAGIC: usize = 0;
    pub const VERSION: usize = 4;
    pub const OPCODE: usize = 6;
    pub const REQUEST_ID: usize = 8;
    pub const CRTC_ID: usize = 12;
    pub const DEVICE_MAJOR: usize = 16;
    pub const DEVICE_MINOR: usize = 20;
    pub const LAST_SEQUENCE: usize = 24;
    pub const TIMEOUT_MS: usize = 32;
    pub const RESERVED: std::ops::Range<usize> = 36..super::REQUEST_LEN;
}

mod response_layout {
    pub const MAGIC: usize = 0;
    pub const VERSION: usize = 4;
    pub const OPCODE: usize = 6;
    pub const REQUEST_ID: usize = 8;
    pub const STATUS: usize = 12;
    pub const OS_ERROR: usize = 16;
    pub const RESULT_FLAGS: usize = 20;
    pub const CRTC_ID: usize = 24;
    pub const PLANE_COUNT: usize = 28;
    pub const SEQUENCE: usize = 32;
    pub const TIMESTAMP_NS: usize = 40;
    pub const WIDTH: usize = 48;
    pub const HEIGHT: usize = 52;
    pub const FOURCC: usize = 56;
    pub const FENCE_MASK: usize = 60;
    pub const MODIFIER: usize = 64;
    pub const PITCHES: usize = 72;
    pub const OFFSETS: usize = 88;
    pub const RESERVED: std::ops::Range<usize> = 104..super::RESPONSE_LEN;
    /// Everything after the common header; used to check unused body fields.
    pub const BODY: std::ops::Range<usize> = RESULT_FLAGS..super::RESPONSE_LEN;
}

const STATUS_OK: u32 = 0;

pub fn encode_request(record: &RequestRecord) -> [u8; REQUEST_LEN] {
    use request_layout::*;
    let mut writer = RecordWriter::<REQUEST_LEN>::new();
    writer.u32(MAGIC, REQUEST_MAGIC);
    writer.u16(VERSION, PROTOCOL_VERSION);
    writer.u16(OPCODE, record.request.opcode() as u16);
    writer.u32(REQUEST_ID, record.request_id);
    match record.request {
        Request::Hello => {}
        Request::OpenCard { major, minor } => {
            writer.u32(DEVICE_MAJOR, major);
            writer.u32(DEVICE_MINOR, minor);
        }
        Request::Frame { crtc_id } => writer.u32(CRTC_ID, crtc_id),
        Request::Wait {
            crtc_id,
            last_sequence,
            timeout_ms,
        } => {
            writer.u32(CRTC_ID, crtc_id);
            writer.u64(LAST_SEQUENCE, last_sequence);
            writer.u32(TIMEOUT_MS, timeout_ms);
        }
    }
    writer.0
}

/// Strictly decodes one request datagram. `bytes` is exactly what was
/// received; any other length is malformed.
pub fn decode_request(bytes: &[u8]) -> Result<RequestRecord, RequestRejection> {
    use request_layout::*;
    let mut rejection = RequestRejection {
        request_id: 0,
        opcode: 0,
        failure: Failure::MalformedRequest,
    };
    if bytes.len() != REQUEST_LEN {
        return Err(rejection);
    }
    let reader = RecordReader(bytes);
    if reader.u32(MAGIC) != REQUEST_MAGIC {
        return Err(rejection);
    }
    rejection.request_id = reader.u32(REQUEST_ID);
    if reader.u16(VERSION) != PROTOCOL_VERSION {
        rejection.failure = Failure::UnsupportedVersion;
        return Err(rejection);
    }
    let Some(opcode) = Opcode::from_wire(reader.u16(OPCODE)) else {
        return Err(rejection);
    };
    rejection.opcode = opcode as u16;
    let crtc_id = reader.u32(CRTC_ID);
    let major = reader.u32(DEVICE_MAJOR);
    let minor = reader.u32(DEVICE_MINOR);
    let last_sequence = reader.u64(LAST_SEQUENCE);
    let timeout_ms = reader.u32(TIMEOUT_MS);
    if !reader.zero(RESERVED) {
        return Err(rejection);
    }
    let no_device = major == 0 && minor == 0;
    let no_wait = last_sequence == 0 && timeout_ms == 0;
    let request = match opcode {
        Opcode::Hello if crtc_id == 0 && no_device && no_wait => Request::Hello,
        Opcode::OpenCard if major != 0 && crtc_id == 0 && no_wait => {
            Request::OpenCard { major, minor }
        }
        Opcode::Frame if crtc_id != 0 && no_device && no_wait => Request::Frame { crtc_id },
        Opcode::Wait if crtc_id != 0 && no_device && timeout_ms <= MAX_WAIT_TIMEOUT_MS => {
            Request::Wait {
                crtc_id,
                last_sequence,
                timeout_ms,
            }
        }
        _ => return Err(rejection),
    };
    Ok(RequestRecord {
        request_id: rejection.request_id,
        request,
    })
}

pub fn encode_response(record: &ResponseRecord) -> [u8; RESPONSE_LEN] {
    use response_layout::*;
    let mut writer = RecordWriter::<RESPONSE_LEN>::new();
    writer.u32(MAGIC, RESPONSE_MAGIC);
    writer.u16(VERSION, PROTOCOL_VERSION);
    writer.u16(OPCODE, record.opcode);
    writer.u32(REQUEST_ID, record.request_id);
    match record.reply {
        Reply::Hello { capabilities } => writer.u32(RESULT_FLAGS, capabilities),
        Reply::CardOpened => {}
        Reply::FrameEmpty {
            crtc_id,
            sequence,
            timestamp_ns,
        } => {
            writer.u32(RESULT_FLAGS, FRAME_EMPTY);
            writer.u32(CRTC_ID, crtc_id);
            writer.u64(SEQUENCE, sequence);
            writer.u64(TIMESTAMP_NS, timestamp_ns);
        }
        Reply::FrameReady(frame) => {
            writer.u32(RESULT_FLAGS, FRAME_READY);
            writer.u32(CRTC_ID, frame.crtc_id);
            writer.u32(PLANE_COUNT, frame.plane_count);
            writer.u64(SEQUENCE, frame.sequence);
            writer.u64(TIMESTAMP_NS, frame.timestamp_ns);
            writer.u32(WIDTH, frame.width);
            writer.u32(HEIGHT, frame.height);
            writer.u32(FOURCC, frame.fourcc);
            writer.u32(FENCE_MASK, frame.fence_mask);
            writer.u64(MODIFIER, frame.modifier);
            for plane in 0..MAX_PLANES {
                writer.u32(PITCHES + plane * 4, frame.pitches[plane]);
                writer.u32(OFFSETS + plane * 4, frame.offsets[plane]);
            }
        }
        Reply::Presentation {
            crtc_id,
            sequence,
            timestamp_ns,
            flags,
        } => {
            writer.u32(RESULT_FLAGS, flags);
            writer.u32(CRTC_ID, crtc_id);
            writer.u64(SEQUENCE, sequence);
            writer.u64(TIMESTAMP_NS, timestamp_ns);
        }
        Reply::Failure { failure, os_error } => {
            writer.u32(STATUS, failure as u32);
            writer.u32(OS_ERROR, os_error as u32);
        }
    }
    writer.0
}

/// Strictly decodes one response datagram. The caller still checks that the
/// request id, opcode and descriptor count match what it expects.
pub fn decode_response(bytes: &[u8]) -> Result<ResponseRecord, MalformedResponse> {
    use response_layout::*;
    if bytes.len() != RESPONSE_LEN {
        return Err(MalformedResponse);
    }
    let reader = RecordReader(bytes);
    if reader.u32(MAGIC) != RESPONSE_MAGIC
        || reader.u16(VERSION) != PROTOCOL_VERSION
        || !reader.zero(RESERVED)
    {
        return Err(MalformedResponse);
    }
    let opcode = reader.u16(OPCODE);
    let request_id = reader.u32(REQUEST_ID);
    let status = reader.u32(STATUS);
    let os_error = reader.u32(OS_ERROR) as i32;
    let reply = if status != STATUS_OK {
        let failure = Failure::from_wire(status).ok_or(MalformedResponse)?;
        if !reader.zero(BODY) {
            return Err(MalformedResponse);
        }
        Reply::Failure { failure, os_error }
    } else {
        if os_error != 0 {
            return Err(MalformedResponse);
        }
        let opcode = Opcode::from_wire(opcode).ok_or(MalformedResponse)?;
        decode_success(&reader, opcode)?
    };
    Ok(ResponseRecord {
        request_id,
        opcode,
        reply,
    })
}

fn decode_success(reader: &RecordReader<'_>, opcode: Opcode) -> Result<Reply, MalformedResponse> {
    use response_layout::*;
    let flags = reader.u32(RESULT_FLAGS);
    let crtc_id = reader.u32(CRTC_ID);
    let sequence = reader.u64(SEQUENCE);
    let timestamp_ns = reader.u64(TIMESTAMP_NS);
    let after_timing_zero = reader.zero(WIDTH..RESERVED.start) && reader.u32(PLANE_COUNT) == 0;
    match opcode {
        Opcode::Hello => {
            if flags & !HELPER_ADMIN_PERMITTED != 0 || !reader.zero(CRTC_ID..RESPONSE_LEN) {
                return Err(MalformedResponse);
            }
            Ok(Reply::Hello {
                capabilities: flags,
            })
        }
        Opcode::OpenCard if reader.zero(BODY) => Ok(Reply::CardOpened),
        Opcode::Frame if flags == FRAME_EMPTY && crtc_id != 0 && after_timing_zero => {
            Ok(Reply::FrameEmpty {
                crtc_id,
                sequence,
                timestamp_ns,
            })
        }
        Opcode::Frame if flags == FRAME_READY => {
            let mut frame = FrameDescriptor {
                crtc_id,
                sequence,
                timestamp_ns,
                width: reader.u32(WIDTH),
                height: reader.u32(HEIGHT),
                fourcc: reader.u32(FOURCC),
                modifier: reader.u64(MODIFIER),
                plane_count: reader.u32(PLANE_COUNT),
                pitches: [0; MAX_PLANES],
                offsets: [0; MAX_PLANES],
                fence_mask: reader.u32(FENCE_MASK),
            };
            for plane in 0..MAX_PLANES {
                frame.pitches[plane] = reader.u32(PITCHES + plane * 4);
                frame.offsets[plane] = reader.u32(OFFSETS + plane * 4);
            }
            frame.validate().map_err(|()| MalformedResponse)?;
            Ok(Reply::FrameReady(frame))
        }
        Opcode::Wait
            if crtc_id != 0
                && flags & !PRESENT_KNOWN_FLAGS == 0
                && (flags & PRESENT_CHANGED != 0) != (flags & PRESENT_TIMEOUT != 0)
                && after_timing_zero =>
        {
            Ok(Reply::Presentation {
                crtc_id,
                sequence,
                timestamp_ns,
                flags,
            })
        }
        _ => Err(MalformedResponse),
    }
}

/// Validates a frame layout produced by the helper before it is encoded.
pub fn frame_descriptor_is_valid(frame: &FrameDescriptor) -> bool {
    frame.validate().is_ok()
}

#[cfg(test)]
mod tests;
