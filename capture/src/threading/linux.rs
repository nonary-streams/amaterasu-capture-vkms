//! Linux capability changes are thread-local and scoped to one setpriority call.
//! Only an already permitted CAP_SYS_NICE may be made effective. The exact
//! previous capability sets are restored before returning, including on failure.
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
    fn missing_permitted_capability_does_not_attempt_to_grant_it() {
        let result = with_permitted_nice(
            [Capabilities::default(); 2],
            |_| panic!("no capability write authorized"),
            || panic!("no privileged operation authorized"),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }
}
