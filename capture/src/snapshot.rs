//! Optional immutable GPU snapshots for asynchronous frame leases.
//!
//! The driver exports mutable compositor storage. This adapter copies it into
//! one of a small pool of private render-node allocations per stream. Each pool
//! buffer is allocated, imported and exported once per geometry/format, and
//! the source import is cached by DMA-BUF identity, so the steady state costs
//! one GPU draw and no allocation.
//!
//! A pool buffer is rewritten only after every reference to its previous
//! contents has been released; the lease release path has already honored the
//! consumer's completion fence by then. When every buffer is still retained,
//! the presentation is skipped and retried later: storage a consumer may read
//! is never overwritten and the presenter never waits for a consumer.
//!
//! The copy is submitted without a CPU wait. Its completion is published as
//! the frame's acquire fence when the render node offers native fences;
//! otherwise the copy finishes before the frame is published. RGB8 remains
//! eight-bit RGB and RGB10 remains ten-bit RGB, using an equivalent channel
//! layout supported by the render node. Driver import or render-target refusal
//! is unsupported, never an implicit precision downgrade. This storage contract
//! does not establish HDR colorimetry or encoder support.
// Without the native backend only the stub snapshotter is built; the pool
// types remain for its interface and the platform-independent tests.
#![cfg_attr(not(amaterasu_vkms_gpu_snapshot), allow(dead_code))]
use amaterasu_media::MEDIA_UNSUPPORTED;
use amaterasu_module_sdk::abi::Status;
use amaterasu_vkms_frame_helper::client::ExportedFrame;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

static QUARANTINED_SNAPSHOTS: AtomicUsize = AtomicUsize::new(0);
pub fn retirement_ready() -> Result<bool, &'static str> {
    if QUARANTINED_SNAPSHOTS.load(Ordering::Acquire) != 0 {
        Err("VKMS snapshot retirement is blocked by unproven GPU reads")
    } else {
        Ok(true)
    }
}

/// Private destination buffers per stream; matches `AM_VKMS_SNAPSHOT_SLOTS`.
pub const SNAPSHOT_SLOTS: usize = 4;

/// Tracks which private destination buffers still hold contents that some
/// reader may use. Owned by the presentation thread; claims may be released
/// from any thread.
pub struct SlotPool {
    busy: [Arc<AtomicBool>; SNAPSHOT_SLOTS],
    next: usize,
}

impl Default for SlotPool {
    fn default() -> Self {
        Self {
            busy: std::array::from_fn(|_| Arc::new(AtomicBool::new(false))),
            next: 0,
        }
    }
}

impl SlotPool {
    /// Claims the least recently claimed free buffer, or `None` when every
    /// buffer is still referenced. Never waits.
    pub fn claim(&mut self) -> Option<Arc<SlotClaim>> {
        for offset in 0..SNAPSHOT_SLOTS {
            let index = (self.next + offset) % SNAPSHOT_SLOTS;
            let busy = &self.busy[index];
            if busy
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                self.next = (index + 1) % SNAPSHOT_SLOTS;
                return Some(Arc::new(SlotClaim {
                    index,
                    busy: busy.clone(),
                }));
            }
        }
        None
    }
}

/// Exclusive right to one pool buffer's contents. The buffer may be rewritten
/// only after this claim drops, which happens when the last stored, lent or
/// retained reference to the snapshot is released.
pub struct SlotClaim {
    index: usize,
    busy: Arc<AtomicBool>,
}

impl SlotClaim {
    pub fn index(&self) -> usize {
        self.index
    }
}

impl Drop for SlotClaim {
    fn drop(&mut self) {
        // Release ordering publishes every reader's completed release to the
        // presentation thread's next Acquire claim of this buffer.
        self.busy.store(false, Ordering::Release);
    }
}

/// What one copy needed beyond the GPU draw itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CopyWork {
    /// The source DMA-BUF was imported rather than found in the cache.
    pub source_imported: bool,
    /// The destination buffer was (re)allocated for a new geometry/format.
    pub slot_allocated: bool,
    /// No completion fence was available; the copy finished on the CPU.
    pub synchronous: bool,
}

/// A submitted copy into a claimed pool buffer.
pub struct PooledSnapshot {
    /// The private DMA-BUF and its completion fence, if any.
    pub frame: ExportedFrame,
    pub slot: Arc<SlotClaim>,
    pub work: CopyWork,
}

pub enum SnapshotOutcome {
    Copied(PooledSnapshot),
    /// Every pool buffer is still referenced; nothing was copied.
    PoolExhausted,
}

/// Copies one exported presentation into private storage.
pub trait FrameSnapshotter {
    fn retire_pending(&mut self) -> Result<bool, Status> {
        Ok(true)
    }
    fn snapshot(&mut self, exported: ExportedFrame) -> Result<SnapshotOutcome, Status>;
}

#[cfg(amaterasu_vkms_gpu_snapshot)]
mod platform {
    use super::*;
    use amaterasu_media::{DMABUF_MAX_DIMENSION, MEDIA_BACKEND_ERROR, MEDIA_BUSY};
    use amaterasu_vkms_frame_helper::protocol::{FrameDescriptor, MAX_PLANES};
    use std::{
        ffi::c_void,
        marker::PhantomData,
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
        ptr::NonNull,
        rc::Rc,
    };

    const COPY_OK: i32 = 0;
    const COPY_UNSUPPORTED: i32 = -1;
    const SOURCE_CACHED: u32 = 1;
    const SLOT_ALLOCATED: u32 = 2;

    #[repr(C)]
    struct Input {
        width: u32,
        height: u32,
        fourcc: u32,
        plane_count: u32,
        modifier: u64,
        fds: [i32; MAX_PLANES],
        pitches: [u32; MAX_PLANES],
        offsets: [u32; MAX_PLANES],
        acquire_fence_fds: [i32; MAX_PLANES],
        slot: u32,
        reserved: u32,
    }

    #[repr(C)]
    struct Output {
        fd: i32,
        fence_fd: i32,
        pitch: u32,
        fourcc: u32,
        modifier: u64,
        flags: u32,
        reserved: u32,
    }

    impl Output {
        fn empty() -> Self {
            Self {
                fd: -1,
                fence_fd: -1,
                pitch: 0,
                fourcc: 0,
                modifier: amaterasu_media::DRM_FORMAT_MOD_INVALID,
                flags: 0,
                reserved: 0,
            }
        }
    }

    const _: () = assert!(std::mem::size_of::<Input>() == 96);
    const _: () = assert!(std::mem::size_of::<Output>() == 32);

    unsafe extern "C" {
        fn am_vkms_snapshot_create() -> *mut c_void;
        fn am_vkms_snapshot_copy(
            snapshotter: *mut c_void,
            input: *const Input,
            output: *mut Output,
        ) -> i32;
        fn am_vkms_snapshot_destroy(snapshotter: *mut c_void);
        fn am_vkms_snapshot_poll(snapshotter: *mut c_void, timeout_ns: u64) -> i32;
    }

    /// One native copy and the descriptors it transferred to Rust.
    struct NativeCopy {
        status: i32,
        output: Output,
        image: Option<OwnedFd>,
        fence: Option<OwnedFd>,
    }

    /// A thread-confined EGL context with its private buffer pool and source
    /// import cache. Create, copy, and drop on one thread.
    pub struct Snapshotter {
        context: NonNull<c_void>,
        pool: SlotPool,
        pending: Option<(ExportedFrame, Arc<SlotClaim>)>,
        _thread_confined: PhantomData<Rc<()>>,
    }

    impl Snapshotter {
        pub fn new() -> Result<Self, Status> {
            retirement_ready().map_err(|_| MEDIA_BACKEND_ERROR)?;
            // SAFETY: this call takes no pointers and returns a unique context
            // which is destroyed exactly once by Drop on the same thread.
            let context =
                NonNull::new(unsafe { am_vkms_snapshot_create() }).ok_or(MEDIA_UNSUPPORTED)?;
            Ok(Self {
                context,
                pool: SlotPool::default(),
                pending: None,
                _thread_confined: PhantomData,
            })
        }

        fn copy(&mut self, input: &Input) -> NativeCopy {
            let mut output = Output::empty();
            // SAFETY: both records have the C layout in gpu_snapshot.h. Input
            // descriptors stay owned by the caller's export throughout this
            // synchronous call; the adapter duplicates any it keeps and retains
            // no Rust pointer. The context is only used on its creating thread.
            let status =
                unsafe { am_vkms_snapshot_copy(self.context.as_ptr(), input, &mut output) };
            // Returned descriptors are always consumed, including on failure,
            // so no error path leaks a native allocation or fence.
            let take = |fd: i32| {
                // SAFETY: the adapter returns newly created descriptors and
                // transfers unique ownership to the caller on return.
                (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
            };
            NativeCopy {
                status,
                image: take(output.fd),
                fence: take(output.fence_fd),
                output,
            }
        }
    }

    impl Snapshotter {
        fn retire_with_timeout(&mut self, timeout_ns: u64) -> Result<bool, Status> {
            if self.pending.is_none() {
                return Ok(true);
            }
            // SAFETY: native context and pending GPU imports belong to this thread.
            // A zero timeout only observes completion; teardown uses a bounded wait.
            let status = unsafe { am_vkms_snapshot_poll(self.context.as_ptr(), timeout_ns) };
            retire_source(&mut self.pending, status)
        }
    }

    impl FrameSnapshotter for Snapshotter {
        fn retire_pending(&mut self) -> Result<bool, Status> {
            self.retire_with_timeout(0)
        }
        fn snapshot(&mut self, exported: ExportedFrame) -> Result<SnapshotOutcome, Status> {
            if !self.retire_pending()? {
                return Err(MEDIA_BUSY);
            }
            let Some(slot) = self.pool.claim() else {
                return Ok(SnapshotOutcome::PoolExhausted);
            };
            let input = Input::from_export(&exported, slot.index())?;
            let source = exported.descriptor;
            let copied = self.copy(&input);
            // Failed submissions may already read compositor storage. Keep both
            // source guards and destination claim until native completion is proven.
            self.pending = Some((exported, slot.clone()));
            if copied.status != COPY_OK {
                self.retire_pending()?;
                return Err(if copied.status == COPY_UNSUPPORTED {
                    MEDIA_UNSUPPORTED
                } else {
                    MEDIA_BACKEND_ERROR
                });
            }
            let descriptor = copied_descriptor(source, &copied.output, copied.fence.is_some())?;
            let image = copied.image.ok_or(MEDIA_BACKEND_ERROR)?;
            if copied.fence.is_none() {
                return Err(MEDIA_BACKEND_ERROR);
            }
            self.retire_pending()?;
            Ok(SnapshotOutcome::Copied(PooledSnapshot {
                work: CopyWork {
                    source_imported: copied.output.flags & SOURCE_CACHED == 0,
                    slot_allocated: copied.output.flags & SLOT_ALLOCATED != 0,
                    synchronous: false,
                },
                frame: ExportedFrame {
                    descriptor,
                    dma_bufs: vec![image],
                    read_guards: Vec::new(),
                    fences: [copied.fence, None, None, None],
                },
                slot,
            }))
        }
    }

    fn retire_source<T>(source: &mut Option<T>, native_status: i32) -> Result<bool, Status> {
        match native_status {
            1 => {
                source.take();
                Ok(true)
            }
            0 => Ok(false),
            _ => Err(MEDIA_BACKEND_ERROR),
        }
    }

    impl Drop for Snapshotter {
        fn drop(&mut self) {
            // A failed completion proof quarantines the native context, source
            // guards and pool claim. Ordinary shutdown reports incomplete retirement.
            if self.retire_with_timeout(1_000_000_000) != Ok(true) {
                QUARANTINED_SNAPSHOTS.fetch_add(1, Ordering::AcqRel);
                std::mem::forget(self.pending.take());
                return;
            }
            // SAFETY: this thread uniquely owns the drained native context.
            unsafe { am_vkms_snapshot_destroy(self.context.as_ptr()) };
        }
    }

    impl Input {
        fn from_export(exported: &ExportedFrame, slot: usize) -> Result<Self, Status> {
            let descriptor = exported.descriptor;
            let planes = descriptor.plane_count as usize;
            if !amaterasu_vkms_frame_helper::protocol::frame_descriptor_is_valid(&descriptor)
                || descriptor.width > DMABUF_MAX_DIMENSION
                || descriptor.height > DMABUF_MAX_DIMENSION
                || exported.dma_bufs.len() != planes
                || exported.read_guards.len() != descriptor.guard_count as usize
                || exported
                    .fences
                    .iter()
                    .enumerate()
                    .any(|(plane, fence)| fence.is_some() != descriptor.has_fence(plane))
                || slot >= SNAPSHOT_SLOTS
            {
                return Err(MEDIA_UNSUPPORTED);
            }
            let mut fds = [-1; MAX_PLANES];
            for (plane, fd) in exported.dma_bufs.iter().enumerate() {
                fds[plane] = fd.as_raw_fd();
            }
            let mut acquire_fence_fds = [-1; MAX_PLANES];
            for (plane, fence) in exported.fences.iter().enumerate() {
                acquire_fence_fds[plane] = fence.as_ref().map_or(-1, AsRawFd::as_raw_fd);
            }
            Ok(Self {
                width: descriptor.width,
                height: descriptor.height,
                fourcc: descriptor.fourcc,
                plane_count: descriptor.plane_count,
                modifier: descriptor.modifier,
                fds,
                pitches: descriptor.pitches,
                offsets: descriptor.offsets,
                acquire_fence_fds,
                slot: slot as u32,
                reserved: 0,
            })
        }
    }

    fn copied_descriptor(
        source: FrameDescriptor,
        output: &Output,
        fenced: bool,
    ) -> Result<FrameDescriptor, Status> {
        let source_precision = rgb_precision(source.fourcc).ok_or(MEDIA_UNSUPPORTED)?;
        if output.fd < 0
            || output.reserved != 0
            || rgb_precision(output.fourcc) != Some(source_precision)
            || output.pitch < source.width.checked_mul(4).ok_or(MEDIA_BACKEND_ERROR)?
        {
            return Err(MEDIA_BACKEND_ERROR);
        }
        let descriptor = FrameDescriptor {
            fourcc: output.fourcc,
            modifier: output.modifier,
            plane_count: 1,
            pitches: [output.pitch, 0, 0, 0],
            offsets: [0; MAX_PLANES],
            // The copy's completion fence becomes the frame's acquire fence.
            fence_mask: u32::from(fenced),
            guard_count: 0,
            ..source
        };
        if !amaterasu_vkms_frame_helper::protocol::frame_descriptor_is_valid(&descriptor) {
            return Err(MEDIA_BACKEND_ERROR);
        }
        Ok(descriptor)
    }

    fn rgb_precision(source: u32) -> Option<u32> {
        let rgb8 = [*b"XR24", *b"AR24", *b"XB24", *b"AB24"];
        let rgb10 = [*b"XR30", *b"AR30", *b"XB30", *b"AB30"];
        if rgb8
            .iter()
            .any(|format| u32::from_le_bytes(*format) == source)
        {
            Some(8)
        } else if rgb10
            .iter()
            .any(|format| u32::from_le_bytes(*format) == source)
        {
            Some(10)
        } else {
            None
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn source() -> FrameDescriptor {
            FrameDescriptor {
                crtc_id: 9,
                sequence: 51,
                timestamp_ns: 1_234_567,
                width: 4,
                height: 2,
                fourcc: u32::from_le_bytes(*b"AR24"),
                modifier: 0,
                plane_count: 1,
                pitches: [16, 0, 0, 0],
                offsets: [0; MAX_PLANES],
                fence_mask: 0,
                guard_count: 0,

                color: 1,
                hdr_metadata: [0; 12],
            }
        }

        fn native_output(fd: i32, pitch: u32, fourcc: u32) -> Output {
            Output {
                fd,
                pitch,
                fourcc,
                modifier: 0,
                ..Output::empty()
            }
        }

        fn eventfd(signaled: bool) -> OwnedFd {
            // SAFETY: eventfd returns a new descriptor owned by this test.
            let raw = unsafe { libc::eventfd(u32::from(signaled), libc::EFD_CLOEXEC) };
            assert!(raw >= 0);
            // SAFETY: successful eventfd transfers ownership of this descriptor.
            unsafe { OwnedFd::from_raw_fd(raw) }
        }

        #[test]
        fn incomplete_gpu_reads_keep_source_and_destination_claim() {
            let mut pool = SlotPool::default();
            let slot = pool.claim().unwrap();
            let source = Arc::new(());
            let observer = source.clone();
            let mut pending = Some((source, slot.clone()));
            drop(slot);
            assert_eq!(retire_source(&mut pending, 0), Ok(false));
            assert_eq!(retire_source(&mut pending, -2), Err(MEDIA_BACKEND_ERROR));
            assert_eq!(Arc::strong_count(&observer), 2);
            let held: Vec<_> = (0..SNAPSHOT_SLOTS - 1)
                .map(|_| pool.claim().unwrap())
                .collect();
            assert!(pool.claim().is_none());
            assert_eq!(retire_source(&mut pending, 1), Ok(true));
            assert_eq!(Arc::strong_count(&observer), 1);
            assert!(pool.claim().is_some());
            drop(held);
        }

        #[test]
        fn a_fenced_copy_publishes_its_completion_as_the_acquire_fence() {
            let copied = copied_descriptor(
                source(),
                &native_output(8, 32, u32::from_le_bytes(*b"XR24")),
                true,
            )
            .unwrap();
            assert_eq!(copied.fence_mask, 1);
            assert!(copied.has_fence(0));
        }

        #[test]
        fn native_input_borrows_acquire_fences_and_names_a_pool_slot() {
            let mut descriptor = source();
            descriptor.fence_mask = 1;
            let fence = eventfd(true);
            let raw_fence = fence.as_raw_fd();
            let exported = ExportedFrame {
                descriptor,
                dma_bufs: vec![eventfd(false)],
                read_guards: Vec::new(),
                fences: [Some(fence), None, None, None],
            };
            let input = Input::from_export(&exported, 3).unwrap();
            assert_eq!(input.acquire_fence_fds, [raw_fence, -1, -1, -1]);
            assert_eq!(input.slot, 3);
            assert!(Input::from_export(&exported, SNAPSHOT_SLOTS).is_err());
        }

        #[test]
        fn copied_layout_preserves_presentation_and_clears_source_fences() {
            let source = source();
            let output = copied_descriptor(
                source,
                &native_output(8, 32, u32::from_le_bytes(*b"XR24")),
                false,
            )
            .unwrap();
            assert_eq!(output.crtc_id, source.crtc_id);
            assert_eq!(output.sequence, source.sequence);
            assert_eq!(output.timestamp_ns, source.timestamp_ns);
            assert_eq!(output.fourcc, u32::from_le_bytes(*b"XR24"));
            assert_eq!(output.pitches, [32, 0, 0, 0]);
            assert_eq!(output.fence_mask, 0);
            assert!(
                copied_descriptor(
                    source,
                    &native_output(-1, 16, u32::from_le_bytes(*b"XR24")),
                    false
                )
                .is_err()
            );
            assert!(
                copied_descriptor(
                    source,
                    &native_output(8, 8, u32::from_le_bytes(*b"XR24")),
                    false
                )
                .is_err()
            );
        }

        #[test]
        fn every_supported_rgb_layout_preserves_source_precision() {
            for (sources, destination) in [
                ([*b"XR24", *b"AR24", *b"XB24", *b"AB24"], *b"XR24"),
                ([*b"XR30", *b"AR30", *b"XB30", *b"AB30"], *b"XR30"),
            ] {
                for format in sources {
                    let source = FrameDescriptor {
                        fourcc: u32::from_le_bytes(format),
                        ..source()
                    };
                    let mut output = native_output(8, 32, u32::from_le_bytes(destination));
                    assert_eq!(
                        copied_descriptor(source, &output, false).unwrap().fourcc,
                        output.fourcc
                    );
                    output.fourcc = u32::from_le_bytes(if destination == *b"XR30" {
                        *b"XR24"
                    } else {
                        *b"XR30"
                    });
                    assert!(copied_descriptor(source, &output, false).is_err());
                }
            }
            assert_eq!(rgb_precision(u32::from_le_bytes(*b"NV12")), None);
        }

        #[test]
        fn equivalent_rgb10_layouts_are_accepted_without_precision_loss() {
            let source = FrameDescriptor {
                fourcc: u32::from_le_bytes(*b"XR30"),
                ..source()
            };
            for format in [*b"XR30", *b"AR30", *b"XB30", *b"AB30"] {
                let output = native_output(8, 32, u32::from_le_bytes(format));
                assert_eq!(
                    copied_descriptor(source, &output, false).unwrap().fourcc,
                    output.fourcc
                );
            }
        }

        #[test]
        fn rejects_descriptor_and_owned_fd_count_mismatch() {
            let exported = ExportedFrame {
                descriptor: source(),
                dma_bufs: vec![],
                read_guards: Vec::new(),
                fences: Default::default(),
            };
            assert!(Input::from_export(&exported, 0).is_err());
        }
    }
}

#[cfg(amaterasu_vkms_gpu_snapshot)]
pub use platform::Snapshotter;

#[cfg(not(amaterasu_vkms_gpu_snapshot))]
pub struct Snapshotter;

#[cfg(not(amaterasu_vkms_gpu_snapshot))]
impl Snapshotter {
    pub fn new() -> Result<Self, Status> {
        Err(MEDIA_UNSUPPORTED)
    }
}

#[cfg(not(amaterasu_vkms_gpu_snapshot))]
impl FrameSnapshotter for Snapshotter {
    fn snapshot(&mut self, _: ExportedFrame) -> Result<SnapshotOutcome, Status> {
        Err(MEDIA_UNSUPPORTED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_rotates_through_free_buffers_and_never_hands_out_a_retained_one() {
        let mut pool = SlotPool::default();
        let claims: Vec<_> = (0..SNAPSHOT_SLOTS).map(|_| pool.claim().unwrap()).collect();
        let indices: Vec<_> = claims.iter().map(|slot| slot.index()).collect();
        assert_eq!(indices, (0..SNAPSHOT_SLOTS).collect::<Vec<_>>());
        // Every buffer is still referenced: the caller must skip, not overwrite.
        assert!(pool.claim().is_none());
        let mut claims = claims.into_iter();
        let first = claims.next().unwrap();
        let second = claims.next().unwrap();
        drop(second);
        let reused_second = pool.claim().unwrap();
        assert_eq!(reused_second.index(), 1);
        drop(first);
        let reused_first = pool.claim().unwrap();
        assert_eq!(reused_first.index(), 0);
        assert!(pool.claim().is_none());
        drop(claims);
        drop(reused_second);
        assert_eq!(pool.claim().map(|claim| claim.index()), Some(1));
    }

    #[test]
    fn a_claim_released_on_another_thread_frees_its_buffer() {
        let mut pool = SlotPool::default();
        let claims: Vec<_> = (0..SNAPSHOT_SLOTS).map(|_| pool.claim().unwrap()).collect();
        std::thread::spawn(move || drop(claims)).join().unwrap();
        assert!(pool.claim().is_some());
    }
}
