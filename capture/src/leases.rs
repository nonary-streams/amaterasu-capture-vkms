//! Retained immutable snapshots. Ordinary scanout exports never enter this path.
use crate::frames::StoredFrame;
use amaterasu_media::{DmaBufFrameLease, MEDIA_BUSY, MEDIA_UNSUPPORTED};
use amaterasu_module_sdk::abi::{STATUS_OK, Status};
use std::{
    ffi::c_void,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

/// Includes completion-fence quarantine across all streams and stream restarts.
const MAX_RETAINED_REFERENCES: usize = 8;
static RETAINED_REFERENCES: AtomicUsize = AtomicUsize::new(0);
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(1);

#[cfg(test)]
pub(crate) static TEST_LEASE_CALLBACKS: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn descriptor(stored: &Arc<StoredFrame>) -> Result<DmaBufFrameLease, Status> {
    if !stored.immutable() {
        return Err(MEDIA_UNSUPPORTED);
    }
    Ok(DmaBufFrameLease {
        frame: *stored.frame(),
        context: Arc::as_ptr(stored).cast_mut().cast(),
        retain: Some(retain),
        release: Some(release),
    })
}

unsafe extern "C" fn retain(context: *mut c_void) -> Status {
    // SAFETY: context comes only from descriptor; the callback's borrowed
    // reference or an already retained reference keeps this allocation alive.
    let stored = unsafe { &*context.cast::<StoredFrame>() };
    if !stored.immutable() {
        return MEDIA_UNSUPPORTED;
    }
    let mut count = RETAINED_REFERENCES.load(Ordering::Acquire);
    loop {
        if count >= MAX_RETAINED_REFERENCES {
            return MEDIA_BUSY;
        }
        match RETAINED_REFERENCES.compare_exchange_weak(
            count,
            count + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(changed) => count = changed,
        }
    }
    // SAFETY: `context` identifies a live Arc allocation and a successful
    // retain transfers exactly one additional strong reference to the caller.
    unsafe { Arc::increment_strong_count(context.cast::<StoredFrame>()) };
    STATUS_OK
}

unsafe extern "C" fn release(context: *mut c_void, completion_fence_fd: i32) {
    // SAFETY: the caller consumes exactly one successful retain. The native
    // binding remains live until all retained references have been released.
    let stored = unsafe { Arc::from_raw(context.cast::<StoredFrame>()) };
    if completion_fence_fd >= 0 {
        // SAFETY: the ABI transfers ownership of this nonnegative descriptor.
        let fence = unsafe { OwnedFd::from_raw_fd(completion_fence_fd) };
        if !wait_completion(&fence, COMPLETION_TIMEOUT) {
            // A failed or stalled consumer cannot make this allocation safe to
            // release. Pin it for the process lifetime and keep its bounded
            // reference slot occupied. No worker thread or callback outlives
            // the binding; this rare failure trades availability for safety.
            std::mem::forget(stored);
            return;
        }
    } else if completion_fence_fd != -1 {
        std::mem::forget(stored);
        return;
    }
    drop(stored);
    RETAINED_REFERENCES.fetch_sub(1, Ordering::AcqRel);
}

#[repr(C)]
#[derive(Default)]
struct SyncFileInfo {
    name: [u8; 32],
    status: i32,
    flags: u32,
    num_fences: u32,
    pad: u32,
    fence_info: u64,
}

fn fence_status(fence: &OwnedFd) -> Option<i32> {
    let mut info = SyncFileInfo::default();
    // Linux _IOWR('>', 4, struct sync_file_info), with a bounded query that
    // asks only for aggregate status and never follows a userspace pointer.
    const SYNC_IOC_FILE_INFO: libc::c_ulong = (3 << 30) | (56 << 16) | (b'>' as u64) << 8 | 4;
    // SAFETY: ioctl writes one ABI-compatible initialized result record; the
    // descriptor remains owned throughout this call and no pointer is retained.
    if unsafe { libc::ioctl(fence.as_raw_fd(), SYNC_IOC_FILE_INFO, &mut info) } == 0 {
        Some(info.status)
    } else {
        None
    }
}

fn wait_completion(fence: &OwnedFd, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match fence_status(fence) {
            Some(1) => return true,
            Some(0) => {}
            _ => return false,
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
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
        // SAFETY: one live pollfd, with its descriptor held by `fence`.
        let status = unsafe { libc::poll(&mut record, 1, millis) };
        if status == 0
            || (status > 0
                && record.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0)
        {
            return false;
        }
        if status < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{frames::StreamFrames, test_support::SourceScript};

    #[test]
    fn mutable_scanout_cannot_be_retained_and_private_snapshot_survives_replacement() {
        let _serial = TEST_LEASE_CALLBACKS.lock().unwrap();
        let frames = StreamFrames::new(41, 0);
        let script = SourceScript::default();
        frames.store(script.export(1, 0, false), 1);
        let lent = frames.lend().unwrap();
        assert_eq!(lent.lease().err(), Some(MEDIA_UNSUPPORTED));
        frames.finish(lent, STATUS_OK);
        frames.store_snapshot(script.export(2, 0, false), 2);
        let lent = frames.lend().unwrap();
        let lease = lent.lease().unwrap();
        // SAFETY: the frame is alive during the callback and this test consumes
        // precisely the reference successfully retained below.
        assert_eq!(unsafe { lease.retain.unwrap()(lease.context) }, STATUS_OK);
        frames.finish(lent, STATUS_OK);
        frames.store_snapshot(script.export(3, 0, false), 3);
        assert_eq!(script.open_descriptors(), 2);
        drop(frames);
        assert_eq!(script.open_descriptors(), 1);
        // SAFETY: same retained lease; this test has no outstanding GPU work.
        unsafe { lease.release.unwrap()(lease.context, -1) };
        assert!(script.all_lent_descriptors_closed());
    }

    #[test]
    fn retained_reference_bound_spans_distinct_frames_and_streams_then_recovers() {
        let _serial = TEST_LEASE_CALLBACKS.lock().unwrap();
        let frames = [StreamFrames::new(41, 0), StreamFrames::new(42, 0)];
        let script = SourceScript::default();
        let mut retained = Vec::new();
        for sequence in 1..=MAX_RETAINED_REFERENCES {
            let stream = &frames[sequence % 2];
            stream.store_snapshot(script.export(sequence as u64, 0, false), sequence as u64);
            let lent = stream.lend().unwrap();
            let lease = lent.lease().unwrap();
            // SAFETY: each borrowed lease remains live and each successful
            // retained reference is released exactly once below.
            assert_eq!(unsafe { lease.retain.unwrap()(lease.context) }, STATUS_OK);
            stream.finish(lent, STATUS_OK);
            retained.push(lease);
        }
        frames[0].store_snapshot(script.export(100, 0, false), 100);
        let lent = frames[0].lend().unwrap();
        let next = lent.lease().unwrap();
        // SAFETY: the borrowed lease is live; BUSY transfers no reference.
        assert_eq!(unsafe { next.retain.unwrap()(next.context) }, MEDIA_BUSY);
        let first = retained.pop().unwrap();
        // SAFETY: consumes this successful retain, with no outstanding GPU work.
        unsafe { first.release.unwrap()(first.context, -1) };
        // SAFETY: releasing one reference restores capacity for another frame.
        assert_eq!(unsafe { next.retain.unwrap()(next.context) }, STATUS_OK);
        retained.push(next);
        frames[0].finish(lent, STATUS_OK);
        drop(frames);
        assert_eq!(script.open_descriptors(), MAX_RETAINED_REFERENCES);
        for lease in retained {
            // SAFETY: each entry owns exactly one retained reference.
            unsafe { lease.release.unwrap()(lease.context, -1) };
        }
        assert!(script.all_lent_descriptors_closed());
    }

    #[test]
    #[ignore = "runs alone to isolate process-lifetime quarantine and forked descriptor ownership"]
    fn invalid_completion_fence_quarantines_storage_and_capacity() {
        const CHILD: &str = "AMATERASU_KMS_QUARANTINE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "leases::tests::invalid_completion_fence_quarantines_storage_and_capacity",
                    "--nocapture",
                    "--ignored",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        // Quarantine intentionally pins the allocation for process lifetime.
        // Exercise that outcome in a child so later tests keep all capacity.
        use std::os::fd::IntoRawFd;
        let frames = StreamFrames::new(41, 0);
        let script = SourceScript::default();
        frames.store_snapshot(script.export(1, 0, false), 1);
        let lent = frames.lend().unwrap();
        let lease = lent.lease().unwrap();
        // SAFETY: the test owns this retained reference until release below.
        assert_eq!(unsafe { lease.retain.unwrap()(lease.context) }, STATUS_OK);
        frames.finish(lent, STATUS_OK);
        drop(frames);
        let invalid = std::fs::File::open("/dev/null").unwrap().into_raw_fd();
        // SAFETY: transfers this retained reference and this owned descriptor.
        unsafe { lease.release.unwrap()(lease.context, invalid) };
        assert_eq!(script.open_descriptors(), 1);
        assert_eq!(RETAINED_REFERENCES.load(Ordering::Acquire), 1);
        // SAFETY: querying a descriptor number has no pointer preconditions.
        assert_eq!(unsafe { libc::fcntl(invalid, libc::F_GETFD) }, -1);
    }

    #[test]
    fn ordinary_readable_files_are_not_completion_fences() {
        let file: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        assert!(!wait_completion(&file, Duration::from_millis(1)));
    }
}
