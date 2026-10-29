//! Capability sets of this single-threaded process.
//!
//! At startup every capability except CAP_SYS_ADMIN leaves the permitted set
//! and nothing stays effective or inheritable. CAP_SYS_ADMIN becomes
//! effective only for the duration of one driver ioctl. capset changes the
//! calling thread only, which is the whole process because the helper never
//! starts another thread.
use std::io;

pub const CAP_SYS_ADMIN: u32 = 21;
const ADMIN: u64 = 1 << CAP_SYS_ADMIN;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CapabilitySets {
    pub effective: u64,
    pub permitted: u64,
    pub inheritable: u64,
}

impl CapabilitySets {
    /// Startup target: CAP_SYS_ADMIN stays permitted if it was granted;
    /// nothing is effective or inheritable.
    pub fn reduced(self) -> Self {
        Self {
            effective: 0,
            permitted: self.permitted & ADMIN,
            inheritable: 0,
        }
    }

    pub fn admin_permitted(self) -> bool {
        self.permitted & ADMIN != 0
    }

    fn with_admin_effective(self) -> Self {
        Self {
            effective: ADMIN,
            ..self
        }
    }

    /// Whether these sets are exactly the reduced startup state.
    pub fn is_reduced(self) -> bool {
        self.permitted & !ADMIN == 0 && self.effective == 0 && self.inheritable == 0
    }
}

/// Reads and replaces this thread's capability sets.
pub trait CapabilityControl {
    fn read(&self) -> io::Result<CapabilitySets>;
    fn write(&self, sets: CapabilitySets) -> io::Result<()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapabilityRefusal {
    /// capget/capset failed.
    Unreadable,
    /// The kernel kept capabilities beyond CAP_SYS_ADMIN, or kept one
    /// effective or inheritable, after the helper asked to drop them.
    Unexpected,
}

/// Holds the reduced capability state and raises CAP_SYS_ADMIN on demand.
pub struct AdminPrivilege<C: CapabilityControl> {
    control: C,
    reduced: CapabilitySets,
}

impl<C: CapabilityControl> AdminPrivilege<C> {
    /// Drops everything but a permitted CAP_SYS_ADMIN and verifies the result.
    pub fn reduce(control: C) -> Result<Self, CapabilityRefusal> {
        let entry = control.read().map_err(|_| CapabilityRefusal::Unreadable)?;
        let target = entry.reduced();
        control
            .write(target)
            .map_err(|_| CapabilityRefusal::Unreadable)?;
        let actual = control.read().map_err(|_| CapabilityRefusal::Unreadable)?;
        if actual != target || !actual.is_reduced() {
            return Err(CapabilityRefusal::Unexpected);
        }
        Ok(Self {
            control,
            reduced: actual,
        })
    }

    pub fn admin_permitted(&self) -> bool {
        self.reduced.admin_permitted()
    }

    /// Runs `operation` with CAP_SYS_ADMIN effective and lowers it before
    /// returning. Returns `None` without running `operation` when the
    /// capability is not permitted or cannot be raised.
    pub fn with_admin<T>(&self, operation: impl FnOnce() -> T) -> Option<T> {
        if !self.admin_permitted() {
            return None;
        }
        self.control
            .write(self.reduced.with_admin_effective())
            .ok()?;
        let _lowered_on_exit = EffectiveAdmin { privilege: self };
        Some(operation())
    }

    fn lower(&self) {
        if self.control.write(self.reduced).is_err() {
            // Continuing with an effective CAP_SYS_ADMIN would violate the
            // helper's contract; stop instead.
            std::process::abort();
        }
    }
}

/// Lowers CAP_SYS_ADMIN when dropped, including during unwinding.
struct EffectiveAdmin<'a, C: CapabilityControl> {
    privilege: &'a AdminPrivilege<C>,
}

impl<C: CapabilityControl> Drop for EffectiveAdmin<'_, C> {
    fn drop(&mut self) {
        self.privilege.lower();
    }
}

/// capget/capset on the calling thread through the version 3 ABI.
#[cfg(target_os = "linux")]
pub struct ProcessCapabilities;

#[cfg(target_os = "linux")]
mod process {
    use super::{CapabilitySets, ProcessCapabilities};
    use std::io;

    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

    #[repr(C)]
    struct CapabilityHeader {
        version: u32,
        pid: libc::c_int,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CapabilityData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    fn split(value: u64) -> (u32, u32) {
        (value as u32, (value >> 32) as u32)
    }

    fn join(low: u32, high: u32) -> u64 {
        u64::from(low) | (u64::from(high) << 32)
    }

    impl super::CapabilityControl for ProcessCapabilities {
        fn read(&self) -> io::Result<CapabilitySets> {
            let mut header = CapabilityHeader {
                version: LINUX_CAPABILITY_VERSION_3,
                pid: 0,
            };
            let mut data = [CapabilityData::default(); 2];
            // SAFETY: version 3 capget reads one header and writes exactly two
            // data records, both of which live on this stack frame.
            let result = unsafe {
                libc::syscall(
                    libc::SYS_capget,
                    &mut header as *mut CapabilityHeader,
                    data.as_mut_ptr(),
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(CapabilitySets {
                effective: join(data[0].effective, data[1].effective),
                permitted: join(data[0].permitted, data[1].permitted),
                inheritable: join(data[0].inheritable, data[1].inheritable),
            })
        }

        fn write(&self, sets: CapabilitySets) -> io::Result<()> {
            let mut header = CapabilityHeader {
                version: LINUX_CAPABILITY_VERSION_3,
                pid: 0,
            };
            let (effective_low, effective_high) = split(sets.effective);
            let (permitted_low, permitted_high) = split(sets.permitted);
            let (inheritable_low, inheritable_high) = split(sets.inheritable);
            let data = [
                CapabilityData {
                    effective: effective_low,
                    permitted: permitted_low,
                    inheritable: inheritable_low,
                },
                CapabilityData {
                    effective: effective_high,
                    permitted: permitted_high,
                    inheritable: inheritable_high,
                },
            ];
            // SAFETY: version 3 capset reads one header and two data records
            // from this stack frame and keeps no pointer after returning.
            let result = unsafe {
                libc::syscall(
                    libc::SYS_capset,
                    &mut header as *mut CapabilityHeader,
                    data.as_ptr(),
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records every write and models a kernel that applies it exactly.
    struct FakeControl {
        current: RefCell<CapabilitySets>,
        writes: RefCell<Vec<CapabilitySets>>,
        refuse_writes: bool,
        sticky_permitted: u64,
    }

    impl FakeControl {
        fn new(sets: CapabilitySets) -> Self {
            Self {
                current: RefCell::new(sets),
                writes: RefCell::new(Vec::new()),
                refuse_writes: false,
                sticky_permitted: 0,
            }
        }
    }

    impl CapabilityControl for &FakeControl {
        fn read(&self) -> io::Result<CapabilitySets> {
            Ok(*self.current.borrow())
        }
        fn write(&self, mut sets: CapabilitySets) -> io::Result<()> {
            if self.refuse_writes {
                return Err(io::Error::from_raw_os_error(1));
            }
            self.writes.borrow_mut().push(sets);
            sets.permitted |= self.sticky_permitted;
            *self.current.borrow_mut() = sets;
            Ok(())
        }
    }

    const CAP_NET_ADMIN: u64 = 1 << 12;

    #[test]
    fn startup_drops_every_capability_except_permitted_admin() {
        let control = FakeControl::new(CapabilitySets {
            effective: ADMIN | CAP_NET_ADMIN,
            permitted: ADMIN | CAP_NET_ADMIN | (1 << 40),
            inheritable: CAP_NET_ADMIN,
        });
        let privilege = AdminPrivilege::reduce(&control).unwrap();
        assert!(privilege.admin_permitted());
        assert_eq!(
            *control.current.borrow(),
            CapabilitySets {
                effective: 0,
                permitted: ADMIN,
                inheritable: 0,
            }
        );
    }

    #[test]
    fn startup_without_admin_succeeds_but_reports_it_unavailable() {
        let control = FakeControl::new(CapabilitySets::default());
        let privilege = AdminPrivilege::reduce(&control).unwrap();
        assert!(!privilege.admin_permitted());
        assert_eq!(privilege.with_admin(|| ()), None);
        assert_eq!(control.writes.borrow().len(), 1, "only the startup drop");
    }

    #[test]
    fn startup_refuses_when_capabilities_cannot_be_dropped() {
        let mut sticky = FakeControl::new(CapabilitySets {
            effective: 0,
            permitted: ADMIN | CAP_NET_ADMIN,
            inheritable: 0,
        });
        sticky.sticky_permitted = CAP_NET_ADMIN;
        assert_eq!(
            AdminPrivilege::reduce(&sticky).err(),
            Some(CapabilityRefusal::Unexpected)
        );

        let mut refusing = FakeControl::new(CapabilitySets {
            effective: ADMIN,
            permitted: ADMIN,
            inheritable: 0,
        });
        refusing.refuse_writes = true;
        assert_eq!(
            AdminPrivilege::reduce(&refusing).err(),
            Some(CapabilityRefusal::Unreadable)
        );
    }

    #[test]
    fn admin_is_effective_only_while_the_operation_runs() {
        let control = FakeControl::new(CapabilitySets {
            effective: 0,
            permitted: ADMIN,
            inheritable: 0,
        });
        let privilege = AdminPrivilege::reduce(&control).unwrap();
        let during = privilege
            .with_admin(|| control.current.borrow().effective)
            .unwrap();
        assert_eq!(during, ADMIN);
        assert_eq!(control.current.borrow().effective, 0);
        let writes = control.writes.borrow();
        assert_eq!(writes.len(), 3);
        assert_eq!(writes[1].effective, ADMIN);
        assert_eq!(writes[2].effective, 0);
        assert!(writes.iter().all(|sets| sets.permitted == ADMIN));
    }
}
