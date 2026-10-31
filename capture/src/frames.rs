//! Per-stream frame state shared by the presentation thread, the controller
//! and native reads.
//!
//! The presentation thread stores each completed presentation as the
//! stream's newest frame. Reads lend that stored frame to one sink callback
//! without asking the helper. Ordinary exports retain the scanout allocation;
//! driver leases hold producer storage directly; optional GPU snapshots decouple
//! capture from producer pacing. A consumed driver lease is never cached.
use crate::admission::Admission;
use amaterasu_media::{
    CAPTURE_FRAME_CACHED_REUSE, CAPTURE_FRAME_FRESH, DMABUF_COLOR_SRGB, DMABUF_MAX_DIMENSION,
    DmaBufFrame, DmaBufPlane, MEDIA_BACKEND_ERROR, MEDIA_BUSY, MEDIA_CLOSED, MEDIA_PENDING,
    MEDIA_UNSUPPORTED, PresentationStatus,
};
use amaterasu_module_sdk::abi::{STATUS_OK, Status};
use amaterasu_vkms_frame_helper::client::ExportedFrame;
use std::{
    os::fd::AsRawFd,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError, TryLockError,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// Maximum time an unclaimed direct frame may hold up producer retirement.
const DIRECT_MAILBOX_LIFETIME: Duration = Duration::from_millis(25);

/// Why a stream stopped delivering; reported in status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamFault {
    /// The helper exited, timed out, broke protocol or kept failing.
    HelperFailed,
    /// The driver refused CAP_SYS_ADMIN.
    PermissionDenied,
    /// Import, synchronization, allocation or private GPU copy failed.
    GpuSnapshotFailed,
    /// The output disappeared or lost its CRTC.
    OutputLost(&'static str),
}

impl StreamFault {
    pub fn code(self) -> &'static str {
        match self {
            Self::HelperFailed => "frame_helper_failed",
            Self::PermissionDenied => "permission_denied",
            Self::GpuSnapshotFailed => "gpu_snapshot_failed",
            Self::OutputLost(code) => code,
        }
    }

    /// What native calls on a faulted stream return.
    pub fn status(self) -> Status {
        match self {
            Self::OutputLost(_) => MEDIA_CLOSED,
            Self::HelperFailed | Self::PermissionDenied | Self::GpuSnapshotFailed => {
                MEDIA_BACKEND_ERROR
            }
        }
    }
}

/// One exported presentation and the frame description that borrows its
/// descriptors. Dropping it closes every descriptor.
pub struct StoredFrame {
    frame: DmaBufFrame,
    _descriptors: ExportedFrame,
    immutable: bool,
    producer_held: bool,
    hdr_metadata: [u16; 12],
}

impl StoredFrame {
    pub fn frame(&self) -> &DmaBufFrame {
        &self.frame
    }
    pub fn hdr_metadata(&self) -> [u16; 12] {
        self.hdr_metadata
    }

    pub fn immutable(&self) -> bool {
        self.immutable
    }
}

/// The newest presentation the presentation thread stored.
struct Newest {
    presentation: PresentationStatus,
    /// `MEDIA_PENDING` before the first frame, `MEDIA_UNSUPPORTED` when the
    /// newest presentation cannot be described.
    frame: Result<Arc<StoredFrame>, Status>,
    direct_deadline: Option<Instant>,
}

pub struct StreamFrames {
    pub admission: Arc<Admission>,
    newest: Mutex<Newest>,
    /// Serializes reads so freshness accounting sees one sink at a time.
    reading: Mutex<()>,
    crtc_id: AtomicU32,
    /// CLOCK_MONOTONIC nanoseconds at stream open: the stream epoch.
    epoch_ns: u64,
    last_timestamp_us: AtomicU64,
    /// Sequence of the last presentation a sink accepted; 0 before any.
    last_delivered_sequence: AtomicU64,
    delivered: AtomicU64,
    resolve_requested: AtomicBool,
    capture_requested: AtomicBool,
    fault: Mutex<Option<StreamFault>>,
}

/// One stored frame lent to a sink. The read gate stays held and the
/// descriptors stay open until it drops, even if a newer frame replaces it.
pub struct Lent<'a> {
    stored: Arc<StoredFrame>,
    pub freshness: u32,
    _reading: MutexGuard<'a, ()>,
}

impl Lent<'_> {
    pub fn frame(&self) -> &DmaBufFrame {
        &self.stored.frame
    }

    pub fn lease(&self) -> Result<amaterasu_media::DmaBufFrameLease, Status> {
        crate::leases::descriptor(&self.stored)
    }
}

impl StreamFrames {
    pub fn new(crtc_id: u32, epoch_ns: u64) -> Self {
        Self {
            admission: Arc::new(Admission::default()),
            newest: Mutex::new(Newest {
                presentation: PresentationStatus::default(),
                frame: Err(MEDIA_PENDING),
                direct_deadline: None,
            }),
            reading: Mutex::new(()),
            crtc_id: AtomicU32::new(crtc_id),
            epoch_ns,
            last_timestamp_us: AtomicU64::new(0),
            last_delivered_sequence: AtomicU64::new(0),
            delivered: AtomicU64::new(0),
            resolve_requested: AtomicBool::new(false),
            capture_requested: AtomicBool::new(false),
            fault: Mutex::new(None),
        }
    }

    pub fn crtc_id(&self) -> u32 {
        self.crtc_id.load(Ordering::SeqCst)
    }

    pub fn retarget(&self, crtc_id: u32) {
        let mut newest = self.lock_newest();
        self.crtc_id.store(crtc_id, Ordering::SeqCst);
        let replaced = Self::clear_direct_frame(&mut newest);
        drop(newest);
        drop(replaced);
    }

    pub fn delivered(&self) -> u64 {
        self.delivered.load(Ordering::Relaxed)
    }

    /// Asks the controller to re-resolve the output's CRTC.
    pub fn request_resolve(&self) {
        self.resolve_requested.store(true, Ordering::SeqCst);
    }

    /// Takes a pending request to re-resolve the output's CRTC.
    pub fn take_resolve_request(&self) -> bool {
        self.resolve_requested.swap(false, Ordering::SeqCst)
    }

    pub fn observe_presentation(&self, sequence: u64, timestamp_ns: u64) {
        let timestamp_us = self.stream_timestamp_us(timestamp_ns);
        self.lock_newest().presentation = PresentationStatus {
            sequence,
            timestamp_us,
        };
    }

    pub fn take_capture_request(&self) -> bool {
        self.capture_requested.swap(false, Ordering::SeqCst)
    }

    pub fn fault(&self) -> Option<StreamFault> {
        *self.fault.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records why delivery stopped. The first fault is kept.
    pub fn set_fault(&self, fault: StreamFault) {
        let mut current = self.fault.lock().unwrap_or_else(PoisonError::into_inner);
        current.get_or_insert(fault);
    }

    /// The newest stored presentation; sequence zero before the first.
    pub fn latest_presentation(&self) -> Result<PresentationStatus, Status> {
        if let Some(fault) = self.fault() {
            return Err(fault.status());
        }
        Ok(self.lock_newest().presentation)
    }

    /// Stores `exported` as the newest frame under the stream `sequence`.
    /// The previous frame's descriptors close at once unless a read still
    /// borrows them, in which case they close when that read ends.
    pub fn store(&self, exported: ExportedFrame, sequence: u64) {
        self.store_frame(exported, sequence, false, false, None);
    }

    /// A completed private copy may remain cached independently of the producer.
    pub fn store_snapshot(&self, exported: ExportedFrame, sequence: u64) {
        self.store_frame(exported, sequence, true, false, None);
    }

    /// The source remains immutable through its driver lease. Do not cache its
    /// initial reference after successful delivery, or producer progress stalls.
    pub fn store_leased(&self, exported: ExportedFrame, sequence: u64, crtc_id: u32) {
        self.store_frame(exported, sequence, true, true, Some(crtc_id));
    }

    fn store_frame(
        &self,
        exported: ExportedFrame,
        sequence: u64,
        immutable: bool,
        producer_held: bool,
        source_crtc: Option<u32>,
    ) {
        let timestamp_us = self.stream_timestamp_us(exported.descriptor.timestamp_ns);
        let frame = describe(exported, sequence, timestamp_us, immutable).map(|mut stored| {
            stored.producer_held = producer_held;
            Arc::new(stored)
        });
        let replaced = {
            let mut newest = self.lock_newest();
            if source_crtc.is_some_and(|id| self.crtc_id() != id) {
                return;
            }
            newest.direct_deadline =
                producer_held.then(|| Instant::now() + DIRECT_MAILBOX_LIFETIME);
            newest.presentation = PresentationStatus {
                sequence,
                timestamp_us,
            };
            std::mem::replace(&mut newest.frame, frame)
        };
        // The old descriptors close outside the lock.
        drop(replaced);
    }

    /// Drop only the mailbox reference; any active or retained read keeps its lease.
    fn clear_direct_frame(newest: &mut Newest) -> Option<Result<Arc<StoredFrame>, Status>> {
        if newest.frame.as_ref().is_ok_and(|frame| frame.producer_held) {
            newest.direct_deadline = None;
            Some(std::mem::replace(&mut newest.frame, Err(MEDIA_PENDING)))
        } else {
            None
        }
    }

    pub fn expire_direct_frame(&self, now: Instant) {
        let mut newest = self.lock_newest();
        let replaced = if newest
            .direct_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            Self::clear_direct_frame(&mut newest)
        } else {
            None
        };
        drop(newest);
        drop(replaced);
    }

    /// Inspect the current immutable capability while preventing replacement.
    /// The callback must not invoke foreign code or re-enter this stream.
    pub fn color_status(&self) -> Option<(u32, [u16; 12])> {
        let newest = self.lock_newest();
        newest
            .frame
            .as_ref()
            .ok()
            .map(|frame| (frame.frame.color, frame.hdr_metadata()))
    }

    pub fn with_current_immutable(&self, inspect: impl FnOnce(&DmaBufFrame) -> Status) -> Status {
        if let Some(fault) = self.fault() {
            return fault.status();
        }
        let newest = self.lock_newest();
        match &newest.frame {
            Ok(frame) if frame.immutable => inspect(&frame.frame),
            Ok(_) => MEDIA_UNSUPPORTED,
            Err(status) => {
                if *status == MEDIA_PENDING {
                    self.capture_requested.store(true, Ordering::SeqCst);
                }
                *status
            }
        }
    }

    /// Lends the stored newest frame for one sink callback.
    pub fn lend(&self) -> Result<Lent<'_>, Status> {
        if let Some(fault) = self.fault() {
            return Err(fault.status());
        }
        let reading = match self.reading.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::WouldBlock) => return Err(MEDIA_BUSY),
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        };
        let stored = match self.lock_newest().frame.clone() {
            Ok(stored) => stored,
            Err(status) => {
                if status == MEDIA_PENDING {
                    self.capture_requested.store(true, Ordering::SeqCst);
                }
                return Err(status);
            }
        };
        let last_delivered = self.last_delivered_sequence.load(Ordering::SeqCst);
        let freshness = if stored.frame.sequence > last_delivered {
            CAPTURE_FRAME_FRESH
        } else {
            CAPTURE_FRAME_CACHED_REUSE
        };
        Ok(Lent {
            stored,
            freshness,
            _reading: reading,
        })
    }

    /// Records the sink's verdict. Only an accepted presentation counts as
    /// delivered, so a rejected fresh frame stays fresh on the next read.
    pub fn finish(&self, lent: Lent<'_>, status: Status) -> Status {
        if status == STATUS_OK {
            self.last_delivered_sequence
                .fetch_max(lent.frame().sequence, Ordering::SeqCst);
            self.delivered.fetch_add(1, Ordering::Relaxed);
        }
        if lent.stored.producer_held && status == STATUS_OK {
            let mut newest = self.lock_newest();
            let replaced = if newest
                .frame
                .as_ref()
                .is_ok_and(|frame| Arc::ptr_eq(frame, &lent.stored))
            {
                Self::clear_direct_frame(&mut newest)
            } else {
                None
            };
            drop(newest);
            drop(replaced);
        }
        drop(lent);
        status
    }

    fn lock_newest(&self) -> MutexGuard<'_, Newest> {
        self.newest.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Converts a driver CLOCK_MONOTONIC presentation time to microseconds
    /// since stream open. Presentations completed before the stream opened
    /// map to zero, and the result never decreases.
    fn stream_timestamp_us(&self, presentation_ns: u64) -> u64 {
        let elapsed_us = presentation_ns.saturating_sub(self.epoch_ns) / 1_000;
        let previous = self
            .last_timestamp_us
            .fetch_max(elapsed_us, Ordering::SeqCst);
        previous.max(elapsed_us)
    }
}

/// Describes an exported presentation as a borrowed DMA-BUF frame whose
/// descriptors the returned value owns.
fn describe(
    exported: ExportedFrame,
    sequence: u64,
    timestamp_us: u64,
    immutable: bool,
) -> Result<StoredFrame, Status> {
    let descriptor = exported.descriptor;
    if !matches!(
        descriptor.color,
        DMABUF_COLOR_SRGB | amaterasu_media::DMABUF_COLOR_BT2020_PQ
    ) || descriptor.width > DMABUF_MAX_DIMENSION
        || descriptor.height > DMABUF_MAX_DIMENSION
    {
        return Err(MEDIA_UNSUPPORTED);
    }
    if descriptor.color == amaterasu_media::DMABUF_COLOR_BT2020_PQ
        && ![*b"XR30", *b"AR30", *b"XB30", *b"AB30"]
            .iter()
            .any(|code| descriptor.fourcc == u32::from_le_bytes(*code))
    {
        return Err(MEDIA_UNSUPPORTED);
    }
    let mut planes = [DmaBufPlane::UNUSED; amaterasu_media::DMABUF_MAX_PLANES];
    for (plane, buffer) in exported.dma_bufs.iter().enumerate() {
        planes[plane] = DmaBufPlane {
            fd: buffer.as_raw_fd(),
            acquire_fence_fd: exported.fences[plane]
                .as_ref()
                .map_or(-1, AsRawFd::as_raw_fd),
            offset: descriptor.offsets[plane],
            pitch: descriptor.pitches[plane],
        };
    }
    let frame = DmaBufFrame {
        width: descriptor.width,
        height: descriptor.height,
        fourcc: descriptor.fourcc,
        plane_count: descriptor.plane_count,
        modifier: descriptor.modifier,
        planes,
        color: descriptor.color,
        reserved: 0,
        timestamp_us,
        sequence,
    };
    Ok(StoredFrame {
        frame,
        _descriptors: exported,
        immutable,
        producer_held: false,
        hdr_metadata: descriptor.hdr_metadata,
    })
}

#[cfg(test)]
mod tests;
