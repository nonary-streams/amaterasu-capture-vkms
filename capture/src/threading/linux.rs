//! Linux capability changes are thread-local and scoped to one setpriority call.
//! Only an already permitted CAP_SYS_NICE may be made effective. The exact
//! previous capability sets are restored before returning, including on failure.
//! Without that capability, a raise falls back to the strongest level the
//! thread's RLIMIT_NICE already permits; no limit or capability is granted.
use std::io;

#[repr(C)]
struct Header {
    version: u32,
    pid: i32,
}
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, Eq, PartialEq)]
struct Capabilities {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}
const CAP_SYS_NICE_MASK: u32 = 1 << 23;

pub(super) fn current() -> io::Result<i32> {
    // SAFETY: errno belongs to the calling Linux thread; getpriority has no
    // pointer arguments. -1 is valid, so inspect errno separately.
    unsafe {
        *libc::__errno_location() = 0;
    }
    let priority = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
    if priority == -1 && io::Error::last_os_error().raw_os_error() != Some(0) {
        return Err(io::Error::last_os_error());
    }
    Ok(priority)
}
fn set_nice(priority: i32) -> io::Result<()> {
    // SAFETY: pid 0 selects only the calling thread. No scheduling class changes.
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, priority) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
pub(super) fn set(priority: i32) -> io::Result<()> {
    match set_nice(priority) {
        Ok(()) => return Ok(()),
        Err(error) if matches!(error.raw_os_error(), Some(libc::EACCES | libc::EPERM)) => {}
        Err(error) => return Err(error),
    }
    let header = Header {
        version: 0x2008_0522,
        pid: 0,
    };
    let mut previous = [Capabilities::default(); 2];
    // SAFETY: Linux capability v3 reads exactly two initialized data elements;
    // pid 0 always refers to this thread. The header lives through the syscall.
    if unsafe { libc::syscall(libc::SYS_capget, &header, previous.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    with_permitted_nice(
        previous,
        |capabilities| {
            // SAFETY: v3 borrows two complete data elements for this synchronous call.
            if unsafe { libc::syscall(libc::SYS_capset, &header, capabilities.as_ptr()) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        },
        || set_nice(priority),
    )
}

/// Raises the calling thread toward `requested`, returning the nice level
/// actually applied. When neither `requested` nor CAP_SYS_NICE is permitted,
/// the strongest level allowed by RLIMIT_NICE is used instead. A thread that
/// cannot be raised at all reports the original permission error.
pub(super) fn raise_toward(requested: i32) -> io::Result<i32> {
    let denied = match set(requested) {
        Ok(()) => return Ok(requested),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => error,
        Err(error) => return Err(error),
    };
    let current = current()?;
    let Some(permitted) = permitted_nice(requested, rlimit_nice_floor()?, current) else {
        return Err(denied);
    };
    if permitted < current {
        set_nice(permitted)?;
    }
    Ok(permitted)
}

/// The lowest nice value RLIMIT_NICE lets an unprivileged thread select:
/// `20 - rlim_cur`, as defined by setrlimit(2).
fn rlimit_nice_floor() -> io::Result<i32> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes one rlimit through this valid, exclusive pointer.
    if unsafe { libc::getrlimit(libc::RLIMIT_NICE, &mut limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(nice_floor(limit.rlim_cur))
}

fn nice_floor(soft_limit: libc::rlim_t) -> i32 {
    // Values above 40 (including RLIM_INFINITY) permit the strongest level.
    20 - soft_limit.min(40) as i32
}

/// The strongest permitted level, unless the limit cannot even keep the
/// current level (an inherited level stronger than the limit is not raised).
fn permitted_nice(requested: i32, floor: i32, current: i32) -> Option<i32> {
    let permitted = requested.max(floor);
    (permitted <= current).then_some(permitted)
}

fn with_permitted_nice(
    previous: [Capabilities; 2],
    mut set_capabilities: impl FnMut(&[Capabilities; 2]) -> io::Result<()>,
    operation: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    if previous[0].permitted & CAP_SYS_NICE_MASK == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "raising streaming priority requires RLIMIT_NICE or permitted CAP_SYS_NICE",
        ));
    }
    if previous[0].effective & CAP_SYS_NICE_MASK != 0 {
        return operation();
    }
    let mut active = previous;
    active[0].effective |= CAP_SYS_NICE_MASK;
    set_capabilities(&active)?;
    struct Restore<'a, F: FnMut(&[Capabilities; 2]) -> io::Result<()>> {
        previous: [Capabilities; 2],
        setter: &'a mut F,
    }
    impl<F: FnMut(&[Capabilities; 2]) -> io::Result<()>> Drop for Restore<'_, F> {
        fn drop(&mut self) {
            if let Err(error) = (self.setter)(&self.previous) {
                eprintln!("cannot restore thread capability sets after scheduling: {error}");
                std::process::abort();
            }
        }
    }
    let restore = Restore {
        previous,
        setter: &mut set_capabilities,
    };
    let result = operation();
    drop(restore);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capability_scope_restores_exact_sets_after_success_and_os_failure() {
        for fail in [false, true] {
            let previous = [
                Capabilities {
                    effective: 4,
                    permitted: CAP_SYS_NICE_MASK | 4,
                    inheritable: 16,
                },
                Capabilities {
                    effective: 8,
                    permitted: 8,
                    inheritable: 8,
                },
            ];
            let mut transitions = Vec::new();
            let result = with_permitted_nice(
                previous,
                |caps| {
                    transitions.push(*caps);
                    Ok(())
                },
                || {
                    if fail {
                        Err(io::ErrorKind::PermissionDenied.into())
                    } else {
                        Ok(())
                    }
                },
            );
            assert_eq!(result.is_err(), fail);
            assert_eq!(transitions.len(), 2);
            assert_eq!(
                transitions[0][0].effective,
                previous[0].effective | CAP_SYS_NICE_MASK
            );
            assert_eq!(transitions[0][0].permitted, previous[0].permitted);
            assert_eq!(transitions[1], previous);
        }
    }
    #[test]
    fn capability_scope_also_restores_on_unwind() {
        use std::panic::{AssertUnwindSafe, catch_unwind};
        let previous = [
            Capabilities {
                permitted: CAP_SYS_NICE_MASK,
                ..Capabilities::default()
            },
            Capabilities::default(),
        ];
        let mut writes = Vec::new();
        let failed = catch_unwind(AssertUnwindSafe(|| {
            let _ = with_permitted_nice(
                previous,
                |caps| {
                    writes.push(*caps);
                    Ok(())
                },
                || panic!("operation failed"),
            );
        }));
        assert!(failed.is_err());
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[1], previous);
    }
    #[test]
    fn rlimit_nice_maps_to_the_documented_floor() {
        assert_eq!(nice_floor(0), 20);
        assert_eq!(nice_floor(31), -11);
        assert_eq!(nice_floor(40), -20);
        assert_eq!(nice_floor(libc::RLIM_INFINITY), -20);
    }
    #[test]
    fn denied_raise_uses_the_strongest_level_rlimit_permits() {
        assert_eq!(permitted_nice(-15, -11, 0), Some(-11));
        assert_eq!(permitted_nice(-10, -11, 0), Some(-10));
        assert_eq!(permitted_nice(-15, -11, -11), Some(-11));
        assert_eq!(permitted_nice(-15, 20, 0), None);
        assert_eq!(permitted_nice(-15, -11, -12), None);
    }
    #[test]
    fn missing_permitted_capability_does_not_attempt_to_grant_it() {
        let result = with_permitted_nice(
            [Capabilities::default(); 2],
            |_| panic!("no capability write authorized"),
            || panic!("no privileged operation authorized"),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }
}
