//! Optional immutable GPU snapshots for asynchronous frame leases.
//!
//! The driver exports mutable compositor storage. This adapter copies it into
//! a fresh render-node allocation and waits for the GPU copy to complete. The
//! exported allocation is never reused or written by this provider afterward.
//! RGB8 remains eight-bit RGB and RGB10 remains ten-bit RGB, using an equivalent
//! channel layout supported by the render node. Driver import or
//! render-target refusal is unsupported, never an implicit precision downgrade.
//! This storage contract does not establish HDR colorimetry or encoder support.
use amaterasu_media::MEDIA_UNSUPPORTED;
use amaterasu_module_sdk::abi::Status;
use amaterasu_vkms_frame_helper::client::ExportedFrame;

#[cfg(amaterasu_vkms_gpu_snapshot)]
mod platform {
    use super::*;
    use amaterasu_media::{DMABUF_MAX_DIMENSION, MEDIA_BACKEND_ERROR};
    use amaterasu_vkms_frame_helper::protocol::{FrameDescriptor, MAX_PLANES};
    use std::{
        ffi::c_void,
        marker::PhantomData,
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
        ptr::NonNull,
        rc::Rc,
        time::{Duration, Instant},
    };

    const ACQUIRE_TIMEOUT: Duration = Duration::from_millis(1_000);

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
    }

    #[repr(C)]
    struct Output {
        fd: i32,
        pitch: u32,
        fourcc: u32,
        reserved: u32,
        modifier: u64,
    }

    const _: () = assert!(std::mem::size_of::<Input>() == 72);
    const _: () = assert!(std::mem::size_of::<Output>() == 24);

    unsafe extern "C" {
        fn am_vkms_snapshot_create() -> *mut c_void;
        fn am_vkms_snapshot_copy(
            snapshotter: *mut c_void,
            input: *const Input,
            output: *mut Output,
        ) -> i32;
        fn am_vkms_snapshot_destroy(snapshotter: *mut c_void);
    }

    /// A thread-confined EGL context. Create, copy, and drop on one thread.
    pub struct Snapshotter {
        context: NonNull<c_void>,
        _thread_confined: PhantomData<Rc<()>>,
    }

    impl Snapshotter {
        pub fn new() -> Result<Self, Status> {
            // SAFETY: this call takes no pointers and returns a unique context
            // which is destroyed exactly once by Drop on the same thread.
            let context =
                NonNull::new(unsafe { am_vkms_snapshot_create() }).ok_or(MEDIA_UNSUPPORTED)?;
            Ok(Self {
                context,
                _thread_confined: PhantomData,
            })
        }

        pub fn snapshot(&mut self, exported: ExportedFrame) -> Result<ExportedFrame, Status> {
            let input = Input::from_export(&exported)?;
            wait_for_acquire(&exported, ACQUIRE_TIMEOUT)?;
            let mut output = Output {
                fd: -1,
                pitch: 0,
                fourcc: 0,
                reserved: 0,
                modifier: amaterasu_media::DRM_FORMAT_MOD_INVALID,
            };
            // SAFETY: both records have the C layout in gpu_snapshot.h. Input
            // descriptors remain owned by `exported` throughout this synchronous
            // copy. The C adapter retains no Rust pointers and owns its context.
            let status =
                unsafe { am_vkms_snapshot_copy(self.context.as_ptr(), &input, &mut output) };
            // A returned descriptor is always consumed, including malformed or
            // failed outcomes, so no error path leaks a native allocation.
            let owned = if output.fd >= 0 {
                // SAFETY: this adapter returns a newly exported descriptor and
                // transfers unique ownership to the caller on this return.
                Some(unsafe { OwnedFd::from_raw_fd(output.fd) })
            } else {
                None
            };
            if status != 0 {
                return Err(if status == -1 {
                    MEDIA_UNSUPPORTED
                } else {
                    MEDIA_BACKEND_ERROR
                });
            }
            let descriptor = copied_descriptor(exported.descriptor, &output)?;
            Ok(ExportedFrame {
                descriptor,
                dma_bufs: vec![owned.ok_or(MEDIA_BACKEND_ERROR)?],
                read_guards: Vec::new(),
                fences: Default::default(),
            })
        }
    }

    impl Drop for Snapshotter {
        fn drop(&mut self) {
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
            })
        }
    }

    fn copied_descriptor(
        source: FrameDescriptor,
        output: &Output,
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
            fence_mask: 0,
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

    fn wait_for_acquire(exported: &ExportedFrame, timeout: Duration) -> Result<(), Status> {
        let deadline = Instant::now() + timeout;
        for fence in exported.fences.iter().flatten() {
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(MEDIA_BACKEND_ERROR);
                }
                let mut record = libc::pollfd {
                    fd: fence.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let millis = remaining
                    .as_millis()
                    .saturating_add(1)
                    .min(i32::MAX as u128) as i32;
                // SAFETY: poll reads and writes exactly one live pollfd. The
                // borrowed fence remains owned by `exported` for this wait.
                let result = unsafe { libc::poll(&mut record, 1, millis) };
                if result > 0 {
                    if record.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
                        || record.revents & libc::POLLIN == 0
                    {
                        return Err(MEDIA_BACKEND_ERROR);
                    }
                    break;
                }
                if result == 0 {
                    return Err(MEDIA_BACKEND_ERROR);
                }
                if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    return Err(MEDIA_BACKEND_ERROR);
                }
            }
        }
        Ok(())
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
                    reserved: 0,
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
                        reserved: 0,
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
                        reserved: 0,
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
                        reserved: 0,
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
                    reserved: 0,
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
        fn acquire_wait_accepts_multiple_completed_planes() {
            // SAFETY: eventfd returns a new descriptor owned by this test.
            let raw = unsafe { libc::eventfd(1, libc::EFD_CLOEXEC) };
            assert!(raw >= 0);
            // SAFETY: successful eventfd transfers ownership of this descriptor.
            let ready = unsafe { OwnedFd::from_raw_fd(raw) };
            let second = ready.try_clone().unwrap();
            let exported = ExportedFrame {
                descriptor: source(),
                dma_bufs: vec![],
                read_guards: Vec::new(),
                fences: [Some(ready), Some(second), None, None],
            };
            assert!(wait_for_acquire(&exported, Duration::from_millis(10)).is_ok());
        }

        #[test]
        fn acquire_wait_checks_every_plane_and_times_out_unsignaled_fence() {
            // SAFETY: eventfd returns new descriptors owned by this test.
            let ready = unsafe { libc::eventfd(1, libc::EFD_CLOEXEC) };
            let blocked = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
            assert!(ready >= 0 && blocked >= 0);
            // SAFETY: the two successful eventfd calls transferred ownership.
            let fences = unsafe {
                [
                    Some(OwnedFd::from_raw_fd(ready)),
                    Some(OwnedFd::from_raw_fd(blocked)),
                    None,
                    None,
                ]
            };
            let exported = ExportedFrame {
                descriptor: source(),
                dma_bufs: vec![],
                read_guards: Vec::new(),
                fences,
            };
            assert!(wait_for_acquire(&exported, Duration::from_millis(1)).is_err());
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

    pub fn snapshot(&mut self, _: ExportedFrame) -> Result<ExportedFrame, Status> {
        Err(MEDIA_UNSUPPORTED)
    }
}
