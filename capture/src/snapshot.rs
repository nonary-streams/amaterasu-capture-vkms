//! Optional immutable GPU snapshots for asynchronous frame leases.
//!
//! The driver exports mutable compositor storage. This adapter copies it into
//! a fresh render-node allocation and publishes its GPU completion fence. The
//! source and its read guards stay pinned until that copy completes. The
//! exported allocation is never reused or written by this provider afterward.
//! RGB8 remains eight-bit RGB and RGB10 remains ten-bit RGB, using an equivalent
//! channel layout supported by the render node. Driver import or
//! render-target refusal is unsupported, never an implicit precision downgrade.
//! This storage contract does not establish HDR colorimetry or encoder support.
use amaterasu_media::MEDIA_UNSUPPORTED;
use amaterasu_module_sdk::abi::Status;
use amaterasu_vkms_frame_helper::client::ExportedFrame;
use std::sync::atomic::{AtomicUsize, Ordering};

static QUARANTINED_SNAPSHOTS: AtomicUsize = AtomicUsize::new(0);

pub fn retirement_ready() -> Result<bool, &'static str> {
    if QUARANTINED_SNAPSHOTS.load(Ordering::Acquire) != 0 {
        Err("VKMS snapshot retirement is blocked by unproven GPU reads")
    } else {
        Ok(true)
    }
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
        acquire_fences: [i32; MAX_PLANES],
    }

    #[repr(C)]
    struct Output {
        fd: i32,
        pitch: u32,
        fourcc: u32,
        completion_fd: i32,
        modifier: u64,
    }

    const _: () = assert!(std::mem::size_of::<Input>() == 88);
    const _: () = assert!(std::mem::size_of::<Output>() == 24);

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

    /// A thread-confined EGL context. Create, copy, and drop on one thread.
    pub struct Snapshotter {
        context: NonNull<c_void>,
        pending_source: Option<ExportedFrame>,
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
                pending_source: None,
                _thread_confined: PhantomData,
            })
        }

        pub fn snapshot(&mut self, exported: ExportedFrame) -> Result<ExportedFrame, Status> {
            if !self.retire_pending()? {
                return Err(MEDIA_BUSY);
            }
            let input = Input::from_export(&exported)?;
            let source_descriptor = exported.descriptor;
            let mut output = Output {
                fd: -1,
                pitch: 0,
                fourcc: 0,
                completion_fd: -1,
                modifier: amaterasu_media::DRM_FORMAT_MOD_INVALID,
            };
            // SAFETY: both records have the C layout in gpu_snapshot.h. Input
            // descriptors remain owned by pending_source until native polling
            // proves the read complete and destroys the temporary imports.
            // The C adapter retains no Rust pointers and owns its context.
            let status =
                unsafe { am_vkms_snapshot_copy(self.context.as_ptr(), &input, &mut output) };
            // Even a failed enqueue can have outstanding GPU reads. Register
            // ownership before inspecting its result or any returned descriptor.
            self.pending_source = Some(exported);
            // A returned descriptor is always consumed, including malformed or
            // failed outcomes, so no error path leaks a native allocation.
            let owned = if output.fd >= 0 {
                // SAFETY: this adapter returns a newly exported descriptor and
                // transfers unique ownership to the caller on this return.
                Some(unsafe { OwnedFd::from_raw_fd(output.fd) })
            } else {
                None
            };
            let completion = if output.completion_fd >= 0 {
                // SAFETY: native fence export transfers a distinct owned fd.
                Some(unsafe { OwnedFd::from_raw_fd(output.completion_fd) })
            } else {
                None
            };
            if status != 0 {
                self.retire_pending()?;
                return Err(if status == -1 {
                    MEDIA_UNSUPPORTED
                } else {
                    MEDIA_BACKEND_ERROR
                });
            }
            let descriptor = copied_descriptor(source_descriptor, &output)?;
            // Queued copies retain the source independently of the output lease.
            // Polling only reclaims imports that have already completed.
            self.retire_pending()?;
            Ok(ExportedFrame {
                descriptor,
                dma_bufs: vec![owned.ok_or(MEDIA_BACKEND_ERROR)?],
                read_guards: Vec::new(),
                fences: [completion, None, None, None],
            })
        }

        /// Poll before entering WAIT_PRESENT: a retained source guard may be
        /// exactly what prevents the compositor from completing that next flip.
        pub fn retire_pending(&mut self) -> Result<bool, Status> {
            self.retire_with_timeout(0)
        }

        fn retire_with_timeout(&mut self, timeout_ns: u64) -> Result<bool, Status> {
            if self.pending_source.is_none() {
                return Ok(true);
            }
            // SAFETY: this context and its pending imports belong to this
            // thread; zero timeout never waits for GPU execution.
            let status = unsafe { am_vkms_snapshot_poll(self.context.as_ptr(), timeout_ns) };
            retire_source(&mut self.pending_source, status)
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
            // Only teardown can wait, and the native fence wait is bounded.
            // A failed proof pins the context, imports and original driver
            // guards for the process lifetime and blocks provider retirement.
            if self.retire_with_timeout(1_000_000_000) != Ok(true) {
                QUARANTINED_SNAPSHOTS.fetch_add(1, Ordering::AcqRel);
                std::mem::forget(self.pending_source.take());
                return;
            }
            // SAFETY: this is the sole owner of the native context and the
            // !Send marker keeps destruction on its creating thread.
            unsafe { am_vkms_snapshot_destroy(self.context.as_ptr()) };
        }
    }

    impl Input {
        fn from_export(exported: &ExportedFrame) -> Result<Self, Status> {
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
            {
                return Err(MEDIA_UNSUPPORTED);
            }
            let mut fds = [-1; MAX_PLANES];
            for (plane, fd) in exported.dma_bufs.iter().enumerate() {
                fds[plane] = fd.as_raw_fd();
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
                acquire_fences: std::array::from_fn(|plane| {
                    exported.fences[plane]
                        .as_ref()
                        .map_or(-1, AsRawFd::as_raw_fd)
                }),
            })
        }
    }

    fn copied_descriptor(
        source: FrameDescriptor,
        output: &Output,
    ) -> Result<FrameDescriptor, Status> {
        let source_precision = rgb_precision(source.fourcc).ok_or(MEDIA_UNSUPPORTED)?;
        if output.fd < 0
            || output.completion_fd < -1
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
            fence_mask: u32::from(output.completion_fd >= 0),
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

        #[test]
        fn copied_layout_preserves_presentation_and_clears_source_fences() {
            let source = source();
            let output = copied_descriptor(
                source,
                &Output {
                    fd: 8,
                    pitch: 32,
                    fourcc: u32::from_le_bytes(*b"XR24"),
                    completion_fd: -1,
                    modifier: 0,
                },
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
                    &Output {
                        fd: -1,
                        pitch: 16,
                        fourcc: u32::from_le_bytes(*b"XR24"),
                        completion_fd: -1,
                        modifier: 0
                    }
                )
                .is_err()
            );
            assert!(
                copied_descriptor(
                    source,
                    &Output {
                        fd: 8,
                        pitch: 8,
                        fourcc: u32::from_le_bytes(*b"XR24"),
                        completion_fd: -1,
                        modifier: 0
                    }
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
                    let mut output = Output {
                        fd: 8,
                        pitch: 32,
                        fourcc: u32::from_le_bytes(destination),
                        completion_fd: -1,
                        modifier: 0,
                    };
                    assert_eq!(
                        copied_descriptor(source, &output).unwrap().fourcc,
                        output.fourcc
                    );
                    output.fourcc = u32::from_le_bytes(if destination == *b"XR30" {
                        *b"XR24"
                    } else {
                        *b"XR30"
                    });
                    assert!(copied_descriptor(source, &output).is_err());
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
                let output = Output {
                    fd: 8,
                    pitch: 32,
                    fourcc: u32::from_le_bytes(format),
                    completion_fd: -1,
                    modifier: 0,
                };
                assert_eq!(
                    copied_descriptor(source, &output).unwrap().fourcc,
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
            assert!(Input::from_export(&exported).is_err());
        }

        #[test]
        fn pending_source_is_released_only_after_proven_completion() {
            let mut source = Some(std::sync::Arc::new(()));
            let observer = source.as_ref().unwrap().clone();
            assert_eq!(retire_source(&mut source, 0), Ok(false));
            assert_eq!(retire_source(&mut source, -2), Err(MEDIA_BACKEND_ERROR));
            assert_eq!(std::sync::Arc::strong_count(&observer), 2);
            assert_eq!(retire_source(&mut source, 1), Ok(true));
            assert_eq!(std::sync::Arc::strong_count(&observer), 1);
        }
        #[test]
        fn acquire_dependency_is_forwarded_without_waiting() {
            let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
            assert!(raw >= 0);
            let fence = unsafe { OwnedFd::from_raw_fd(raw) };
            let fd = fence.try_clone().unwrap();
            let mut descriptor = source();
            descriptor.fence_mask = 1;
            let exported = ExportedFrame {
                descriptor,
                dma_bufs: vec![fd],
                read_guards: vec![],
                fences: [Some(fence), None, None, None],
            };
            let input = Input::from_export(&exported).unwrap();
            assert_eq!(
                input.acquire_fences[0],
                exported.fences[0].as_ref().unwrap().as_raw_fd()
            );
            assert_eq!(input.acquire_fences[1..], [-1; 3]);
        }
    }
}
#[cfg(not(amaterasu_vkms_gpu_snapshot))]
pub struct Snapshotter;

#[cfg(not(amaterasu_vkms_gpu_snapshot))]
impl Snapshotter {
    pub fn retire_pending(&mut self) -> Result<bool, Status> {
        Ok(true)
    }

    pub fn new() -> Result<Self, Status> {
        Err(MEDIA_UNSUPPORTED)
    }

    pub fn snapshot(&mut self, _: ExportedFrame) -> Result<ExportedFrame, Status> {
        Err(MEDIA_UNSUPPORTED)
    }
}

#[cfg(amaterasu_vkms_gpu_snapshot)]
pub use platform::Snapshotter;
