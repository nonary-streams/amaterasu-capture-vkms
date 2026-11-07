//! Exact helper retirement receipts, including failed or abandoned source opens.
use crate::model::{CaptureError, CaptureResult, MAX_STREAMS};
use std::{
    io,
    sync::{Arc, Mutex, PoisonError, TryLockError},
};

/// A receipt is independent of the frame socket used to interrupt native I/O.
/// Successful polling proves the exact authority-owned helper has been reaped.
pub trait Retirement: Send + Sync {
    fn poll_retired(&self) -> io::Result<bool>;
}

// Existing streams plus one probe or source-open attempt. A stalled authority
// cannot accumulate unbounded helper jobs by repeatedly failing initialization.
const MAX_HELPER_RETIREMENTS: usize = MAX_STREAMS + 1;

enum Slot {
    Vacant,
    Reserved,
    Awaiting(Arc<dyn Retirement>),
}

pub struct Registry {
    slots: Mutex<Vec<Slot>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            slots: Mutex::new((0..MAX_HELPER_RETIREMENTS).map(|_| Slot::Vacant).collect()),
        }
    }
}

impl Registry {
    /// Reserve before requesting an authority session. V2 cannot start a helper
    /// until HELLO, which must follow attachment of its receipt to this slot.
    pub fn reserve(self: &Arc<Self>) -> CaptureResult<Reservation> {
        let mut slots = self
            .slots
            .try_lock()
            .map_err(|_| CaptureError::code("helper_retirement_busy"))?;
        let index = slots
            .iter()
            .position(|slot| matches!(slot, Slot::Vacant))
            .ok_or_else(|| CaptureError::code("helper_retirement_capacity"))?;
        slots[index] = Slot::Reserved;
        Ok(Reservation {
            registry: self.clone(),
            index,
            attached: false,
        })
    }

    pub fn poll_retired(&self) -> CaptureResult<bool> {
        let mut slots = match self.slots.try_lock() {
            Ok(slots) => slots,
            Err(TryLockError::WouldBlock) => return Ok(false),
            Err(TryLockError::Poisoned(_)) => {
                return Err(CaptureError::code("helper_retirement_state_poisoned"));
            }
        };
        let mut retired = true;
        let mut failure = None;
        for slot in slots.iter_mut() {
            match slot {
                Slot::Vacant => {}
                Slot::Reserved => retired = false,
                Slot::Awaiting(receipt) => {
                    match receipt.poll_retired() {
                        Ok(true) => *slot = Slot::Vacant,
                        Ok(false) => retired = false,
                        Err(error) => {
                            retired = false;
                            failure.get_or_insert_with(|| CaptureError::new("helper_retirement_failed", format!("capture authority helper retirement was not verified: {error}")));
                        }
                    }
                }
            }
        }
        failure.map_or(Ok(retired), Err)
    }
}

pub struct Reservation {
    registry: Arc<Registry>,
    index: usize,
    attached: bool,
}

impl Reservation {
    pub fn attach(mut self, receipt: Arc<dyn Retirement>) {
        // Preserve receipt ownership even if an earlier internal panic poisoned
        // the map; subsequent polls still refuse to claim successful retirement.
        self.registry
            .slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)[self.index] = Slot::Awaiting(receipt);
        self.attached = true;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.attached {
            self.registry
                .slots
                .lock()
                .unwrap_or_else(PoisonError::into_inner)[self.index] = Slot::Vacant;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU8, Ordering};

    #[derive(Default)]
    struct Receipt(AtomicU8);
    impl Retirement for Receipt {
        fn poll_retired(&self) -> io::Result<bool> {
            match self.0.load(Ordering::SeqCst) {
                0 => Ok(false),
                1 => Ok(true),
                _ => Err(io::Error::other("synthetic helper reap failure")),
            }
        }
    }

    #[test]
    fn pending_or_failed_receipts_keep_their_bounded_slots_until_proven_retired() {
        let registry = Arc::new(Registry::default());
        let receipts: Vec<_> = (0..MAX_HELPER_RETIREMENTS)
            .map(|_| Arc::new(Receipt::default()))
            .collect();
        for receipt in &receipts {
            registry.reserve().unwrap().attach(receipt.clone());
        }
        assert!(registry.reserve().is_err());
        assert_eq!(registry.poll_retired(), Ok(false));
        receipts[0].0.store(2, Ordering::SeqCst);
        assert_eq!(
            registry.poll_retired().unwrap_err().code,
            "helper_retirement_failed"
        );
        assert!(registry.reserve().is_err());
        receipts[1].0.store(1, Ordering::SeqCst);
        assert_eq!(
            registry.poll_retired().unwrap_err().code,
            "helper_retirement_failed"
        );
        let reservation = registry.reserve().unwrap();
        assert!(registry.reserve().is_err());
        drop(reservation);
        assert!(registry.reserve().is_ok());
    }
}
