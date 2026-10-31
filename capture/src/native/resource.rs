//! Negotiated DMA-BUF delivery backed by the existing immutable frame leases.
use super::sdk_resource::{
    CaptureResourceFrame, CaptureResourceFrameSink, CaptureResourceOffer, CaptureResourceOfferSink,
    DmaBufResourceCapabilities, FRAME_ACCESS_GPU, FRAME_CLOCK_STREAM_RELATIVE, FRAME_MATRIX_RGB,
    FRAME_OWNERSHIP_RETAINABLE, FRAME_PRIMARIES_BT709, FRAME_PRIMARIES_BT2020, FRAME_RANGE_FULL,
    FRAME_RESOURCE_DMABUF, FRAME_SYNC_NONE, FRAME_SYNC_SYNC_FILE, FRAME_TRANSFER_PQ,
    FRAME_TRANSFER_SRGB, FrameClockMetadata, FrameColorMetadata, FrameDescriptor, FrameSyncFile,
};
use super::*;
use amaterasu_media::{DmaBufFrame, MEDIA_PENDING};

#[derive(Clone, Copy, PartialEq, Eq)]
struct Format {
    fourcc: u32,
    modifier: u64,
    color: u32,
    width: u32,
    height: u32,
}
impl From<&DmaBufFrame> for Format {
    fn from(frame: &DmaBufFrame) -> Self {
        Self {
            fourcc: frame.fourcc,
            modifier: frame.modifier,
            color: frame.color,
            width: frame.width,
            height: frame.height,
        }
    }
}
pub(super) struct OfferState {
    format: Format,
    generation: u64,
    selected: bool,
}
impl OfferState {
    fn update(&mut self, format: Format) {
        if self.format != format {
            self.format = format;
            self.generation = self.generation.checked_add(1).unwrap_or(1);
            self.selected = false;
        }
    }
}
fn descriptor(kind: u32, version: u32, size: u32, data: *const c_void) -> FrameDescriptor {
    FrameDescriptor {
        kind,
        version,
        size,
        reserved: 0,
        namespace: [0; 16],
        data,
    }
}
fn none() -> FrameDescriptor {
    descriptor(FRAME_SYNC_NONE, 0, 0, std::ptr::null())
}
fn sync_file() -> FrameDescriptor {
    descriptor(FRAME_SYNC_SYNC_FILE, 1, 0, std::ptr::null())
}

pub(super) unsafe extern "C" fn enumerate<B: Backend>(
    context: *mut c_void,
    stream: u64,
    sink: CaptureResourceOfferSink,
    generation: *mut u64,
) -> Status {
    guarded(|| {
        if context.is_null()
            || generation.is_null()
            || !(generation as usize).is_multiple_of(align_of::<u64>())
        {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: binding and output live throughout the call.
        let binding = unsafe { &*context.cast::<Binding<B>>() };
        let frames = match binding.reader(stream) {
            Ok(reader) => reader.frames,
            Err(status) => return status,
        };
        let Some(_admission) = frames.admission.enter() else {
            return MEDIA_CLOSED;
        };
        let lent = match frames.lend() {
            Ok(value) => value,
            Err(status) => return status,
        };
        if let Err(status) = lent.lease() {
            return frames.finish(lent, status);
        }
        let format = Format::from(lent.frame());
        let current = {
            let mut states = match binding.resources.try_lock() {
                Ok(value) => value,
                Err(_) => return frames.finish(lent, MEDIA_BUSY),
            };
            let state = states.entry(stream).or_insert(OfferState {
                format,
                generation: 1,
                selected: false,
            });
            state.update(format);
            state.generation
        };
        let capabilities = DmaBufResourceCapabilities {
            fourcc: format.fourcc,
            color: format.color,
            modifier: format.modifier,
            min_width: format.width,
            max_width: format.width,
            min_height: format.height,
            max_height: format.height,
            device_id: [0; 16],
        };
        let offer = CaptureResourceOffer {
            id: 1,
            ownership: FRAME_OWNERSHIP_RETAINABLE,
            accessibility: FRAME_ACCESS_GPU,
            reserved: 0,
            resource: descriptor(
                FRAME_RESOURCE_DMABUF,
                1,
                size_of::<DmaBufResourceCapabilities>() as u32,
                (&capabilities as *const DmaBufResourceCapabilities).cast(),
            ),
            acquire_sync: none(),
            completion_sync: sync_file(),
        };
        // SAFETY: offer and capability payload live through the synchronous callback.
        let status = unsafe { (sink.write)(sink.context, &offer) };
        if status == STATUS_OK {
            unsafe { generation.write(current) };
        }
        // Enumeration does not count as a delivered frame.
        frames.finish(lent, MEDIA_PENDING);
        status
    })
}

pub(super) unsafe extern "C" fn select<B: Backend>(
    context: *mut c_void,
    stream: u64,
    generation: u64,
    offer: u64,
) -> Status {
    guarded(|| {
        if context.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: broker retains binding throughout call.
        let binding = unsafe { &*context.cast::<Binding<B>>() };
        let frames = match binding.reader(stream) {
            Ok(reader) => reader.frames,
            Err(status) => return status,
        };
        let Some(_admission) = frames.admission.enter() else {
            return MEDIA_CLOSED;
        };
        frames.with_current_immutable(|frame| {
            let mut states = match binding.resources.try_lock() {
                Ok(value) => value,
                Err(_) => return MEDIA_BUSY,
            };
            match states.get_mut(&stream) {
                Some(state) => {
                    state.update(Format::from(frame));
                    if state.generation == generation && offer == 1 {
                        state.selected = true;
                        STATUS_OK
                    } else {
                        MEDIA_UNSUPPORTED
                    }
                }
                None => MEDIA_UNSUPPORTED,
            }
        })
    })
}

unsafe extern "C" fn retain(context: *mut c_void) -> Status {
    // SAFETY: context is a live StoredFrame through callback or retained reference.
    unsafe { crate::leases::retain_resource(context) }
}
unsafe extern "C" fn release(context: *mut c_void, completion: *const FrameDescriptor) {
    // Invalid descriptors or failed duplication quarantine the retained storage.
    let fd = if completion.is_null()
        || !(completion as usize).is_multiple_of(align_of::<FrameDescriptor>())
    {
        -2
    } else {
        // SAFETY: consumer supplies a borrowed descriptor valid for this call.
        let descriptor = unsafe { &*completion };
        if descriptor.reserved != 0 || descriptor.namespace != [0; 16] {
            -2
        } else if descriptor.kind == FRAME_SYNC_NONE
            && descriptor.version == 0
            && descriptor.size == 0
            && descriptor.data.is_null()
        {
            -1
        } else if descriptor.kind == FRAME_SYNC_SYNC_FILE
            && descriptor.version == 1
            && descriptor.size as usize == size_of::<FrameSyncFile>()
            && !descriptor.data.is_null()
            && (descriptor.data as usize).is_multiple_of(align_of::<FrameSyncFile>())
        {
            // SAFETY: schema and alignment were checked; descriptor data is borrowed by contract.
            let borrowed = unsafe { (*descriptor.data.cast::<FrameSyncFile>()).fd };
            if borrowed < 0 {
                -2
            } else {
                // SAFETY: fcntl duplicates the borrowed descriptor without consuming it.
                let dup = unsafe { libc::fcntl(borrowed, libc::F_DUPFD_CLOEXEC, 0) };
                if dup < 0 { -2 } else { dup }
            }
        } else {
            -2
        }
    };
    // SAFETY: consumes exactly one retained reference and transfers duplicated fd.
    unsafe { crate::leases::release_resource(context, fd) };
}

pub(super) unsafe extern "C" fn read<B: Backend>(
    context: *mut c_void,
    stream: u64,
    sink: CaptureResourceFrameSink,
) -> Status {
    guarded(|| {
        if context.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: broker retains binding through synchronous call.
        let binding = unsafe { &*context.cast::<Binding<B>>() };
        let frames = match binding.reader(stream) {
            Ok(reader) => reader.frames,
            Err(status) => return status,
        };
        let Some(_admission) = frames.admission.enter() else {
            return MEDIA_CLOSED;
        };
        let lent = match frames.lend() {
            Ok(value) => value,
            Err(status) => return status,
        };
        let lease = match lent.lease() {
            Ok(value) => value,
            Err(status) => return frames.finish(lent, status),
        };
        let format = Format::from(lent.frame());
        let generation = {
            let mut states = match binding.resources.try_lock() {
                Ok(value) => value,
                Err(_) => return frames.finish(lent, MEDIA_BUSY),
            };
            let Some(state) = states.get_mut(&stream) else {
                return frames.finish(lent, MEDIA_UNSUPPORTED);
            };
            state.update(format);
            if !state.selected {
                return frames.finish(lent, MEDIA_UNSUPPORTED);
            }
            state.generation
        };
        let frame = lent.frame();
        let clock = FrameClockMetadata {
            version: 1,
            size: size_of::<FrameClockMetadata>() as u32,
            domain: FRAME_CLOCK_STREAM_RELATIVE,
            ..Default::default()
        };
        let offered = CaptureResourceFrame {
            offer_generation: generation,
            offer_id: 1,
            timestamp_us: frame.timestamp_us,
            capture_pacing_timestamp_us: frame.timestamp_us,
            sequence: frame.sequence,
            width: frame.width,
            height: frame.height,
            freshness: lent.freshness,
            reserved: 0,
            color: FrameColorMetadata {
                version: 1,
                size: size_of::<FrameColorMetadata>() as u32,
                primaries: if frame.color == 2 {
                    FRAME_PRIMARIES_BT2020
                } else {
                    FRAME_PRIMARIES_BT709
                },
                transfer: if frame.color == 2 {
                    FRAME_TRANSFER_PQ
                } else {
                    FRAME_TRANSFER_SRGB
                },
                matrix: FRAME_MATRIX_RGB,
                range: FRAME_RANGE_FULL,
            },
            clock,
            pacing_clock: clock,
            resource: descriptor(
                FRAME_RESOURCE_DMABUF,
                1,
                size_of::<DmaBufFrame>() as u32,
                (frame as *const DmaBufFrame).cast(),
            ),
            acquire_sync: none(),
            context: lease.context,
            retain,
            release,
        };
        // SAFETY: the lent frame and descriptors remain live through the callback.
        let status = unsafe { (sink.write)(sink.context, &offered) };
        frames.finish(lent, status)
    })
}
