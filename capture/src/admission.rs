use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

const CLOSED: u64 = 1 << 63;
const READERS: u64 = !CLOSED;

/// Lock-free admission gate for synchronous native frame callbacks: close
/// refuses new callbacks while admitted ones finish.
#[derive(Default)]
pub struct Admission(AtomicU64);

impl Admission {
    pub fn enter(self: &Arc<Self>) -> Option<Permit> {
        let mut state = self.0.load(Ordering::SeqCst);
        loop {
            if state & CLOSED != 0 || state & READERS == READERS {
                return None;
            }
            match self
                .0
                .compare_exchange_weak(state, state + 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return Some(Permit(self.clone())),
                Err(actual) => state = actual,
            }
        }
    }

    pub fn close(&self) {
        self.0.fetch_or(CLOSED, Ordering::SeqCst);
    }
}

pub struct Permit(Arc<Admission>);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_rejects_new_callbacks_and_preserves_an_already_admitted_callback() {
        let admission = Arc::new(Admission::default());
        let admitted = admission.enter().unwrap();

        admission.close();

        assert!(admission.enter().is_none());
        drop(admitted);
        assert!(admission.enter().is_none());
    }
}
