//! Local Linux scheduling bridge until the separately pinned SDK publishes its
//! threading helper. Keep the capability boundary aligned with the host SDK;
//! this provider remains independently buildable against its existing SDK pin.
mod linux;

/// Vibeshine's critical streaming level. Installations without CAP_SYS_NICE
/// receive the strongest level their RLIMIT_NICE permits.
pub(crate) const CRITICAL_NICE: i32 = -15;

/// The priority a capture thread obtained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapturePriority {
    Critical,
    /// RLIMIT_NICE allowed only this weaker level.
    Limited {
        nice: i32,
    },
    /// The thread kept the priority it inherited.
    Unchanged,
}

pub fn configure_capture_thread() -> CapturePriority {
    match linux::raise_toward(CRITICAL_NICE) {
        Ok(applied) if applied != CRITICAL_NICE => {
            eprintln!("VKMS capture thread priority limited to nice {applied} by RLIMIT_NICE");
            CapturePriority::Limited { nice: applied }
        }
        Ok(_) => CapturePriority::Critical,
        Err(error) => {
            eprintln!("VKMS capture thread critical priority unavailable: {error}");
            CapturePriority::Unchanged
        }
    }
}

/// Only the legacy per-stream helper inherits capture priority. Restore the
/// module caller before other providers or application launchers can run.
pub struct ScopedCapturePriority {
    previous: Option<i32>,
    _same_thread: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl ScopedCapturePriority {
    pub fn enter() -> Self {
        let previous = match linux::current()
            .and_then(|previous| linux::raise_toward(CRITICAL_NICE).map(|_| previous))
        {
            Ok(previous) => Some(previous),
            Err(error) => {
                eprintln!("capture helper priority unavailable: {error}");
                None
            }
        };
        Self {
            previous,
            _same_thread: std::marker::PhantomData,
        }
    }
}
impl Drop for ScopedCapturePriority {
    fn drop(&mut self) {
        if let Some(previous) = self.previous
            && let Err(error) = linux::set(previous)
        {
            eprintln!("capture helper caller priority restoration failed: {error}");
            std::process::abort();
        }
    }
}
