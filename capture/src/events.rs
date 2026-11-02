//! One bounded readiness subscription. Holding the slot lock across callbacks
//! gives unsubscribe/close their mandatory callback lifetime barrier. Callbacks
//! only latch notices and wake consumers; they must never re-enter capture.
use crate::native::sdk_resource::CaptureEventSink;
use amaterasu_media::{MEDIA_BUSY, MEDIA_CLOSED};
use amaterasu_module_sdk::abi::{BorrowedBytes, STATUS_OK, Status};
use std::sync::Mutex;

#[derive(Default)]
struct State {
    sink: Option<CaptureEventSink>,
    ready: bool,
    closed: bool,
}
// SAFETY: the capture ABI permits callbacks on producer threads. Successful
// subscription keeps their context alive until unsubscribe or close returns.
unsafe impl Send for State {}

#[derive(Default)]
pub(crate) struct Events(Mutex<State>);

impl Events {
    /// # Safety
    /// The sink must obey CaptureEventSink's lifetime and threading contract.
    pub unsafe fn subscribe(&self, sink: CaptureEventSink) -> Status {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return MEDIA_CLOSED;
        }
        if state.sink.is_some() {
            return MEDIA_BUSY;
        }
        state.sink = Some(sink);
        if state.ready {
            // SAFETY: subscription retains this context until the slot is cleared.
            unsafe { (sink.frame_ready)(sink.context) };
        }
        STATUS_OK
    }

    pub fn ready(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return;
        }
        state.ready = true;
        if let Some(sink) = state.sink {
            // SAFETY: the lock prevents unsubscribe from releasing the context.
            unsafe { (sink.frame_ready)(sink.context) };
        }
    }

    pub fn unsubscribe(&self) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sink = None;
    }

    pub fn close(&self, reason: &str) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return;
        }
        state.closed = true;
        if let Some(sink) = state.sink.take() {
            // SAFETY: context is retained through this callback under the barrier.
            unsafe { (sink.stopped)(sink.context, BorrowedBytes::new(reason.as_bytes())) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::sdk_resource::CaptureSwapRequest;
    use std::{
        ffi::c_void,
        sync::{
            Arc, Condvar,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        time::Duration,
    };

    type CallbackBarrier = (mpsc::Sender<()>, Arc<(Mutex<bool>, Condvar)>);

    #[derive(Default)]
    struct Receiver {
        frames: AtomicUsize,
        stops: AtomicUsize,
        barrier: Option<CallbackBarrier>,
    }
    unsafe extern "C" fn frame(context: *mut c_void) {
        // SAFETY: tests retain the receiver through unsubscribe/close and join.
        let receiver = unsafe { &*context.cast::<Receiver>() };
        receiver.frames.fetch_add(1, Ordering::SeqCst);
        if let Some((entered, gate)) = &receiver.barrier {
            entered.send(()).unwrap();
            let guard = gate.0.lock().unwrap();
            drop(gate.1.wait_while(guard, |blocked| *blocked).unwrap());
        }
    }
    unsafe extern "C" fn swap(_: *mut c_void, _: *const CaptureSwapRequest) -> Status {
        MEDIA_BUSY
    }
    unsafe extern "C" fn stopped(context: *mut c_void, _: BorrowedBytes) {
        // SAFETY: same retained receiver as frame callback.
        unsafe { &*context.cast::<Receiver>() }
            .stops
            .fetch_add(1, Ordering::SeqCst);
    }
    fn sink(receiver: &Receiver) -> CaptureEventSink {
        CaptureEventSink {
            context: (receiver as *const Receiver).cast_mut().cast(),
            frame_ready: frame,
            request_swap: swap,
            stopped,
        }
    }
    #[test]
    fn subscribe_announces_existing_frame_and_close_is_terminal() {
        let events = Events::default();
        let receiver = Receiver::default();
        events.ready();
        // SAFETY: receiver outlives synchronous close and unsubscribe.
        assert_eq!(unsafe { events.subscribe(sink(&receiver)) }, STATUS_OK);
        assert_eq!(receiver.frames.load(Ordering::SeqCst), 1);
        assert_eq!(unsafe { events.subscribe(sink(&receiver)) }, MEDIA_BUSY);
        events.close("stream_closed");
        events.ready();
        events.close("stream_closed");
        assert_eq!(receiver.stops.load(Ordering::SeqCst), 1);
        assert_eq!(receiver.frames.load(Ordering::SeqCst), 1);
        assert_eq!(unsafe { events.subscribe(sink(&receiver)) }, MEDIA_CLOSED);
        events.unsubscribe();
    }
    #[test]
    fn unsubscribe_waits_for_an_admitted_callback_and_prevents_later_calls() {
        let events = Arc::new(Events::default());
        let (entered, notified) = mpsc::channel();
        let gate = Arc::new((Mutex::new(true), Condvar::new()));
        let receiver = Receiver {
            barrier: Some((entered, gate.clone())),
            ..Receiver::default()
        };
        // SAFETY: receiver remains live until both worker threads retire.
        assert_eq!(unsafe { events.subscribe(sink(&receiver)) }, STATUS_OK);
        let producer = {
            let events = events.clone();
            std::thread::spawn(move || events.ready())
        };
        notified.recv_timeout(Duration::from_secs(2)).unwrap();
        let (done, completion) = mpsc::channel();
        let unsubscriber = {
            let events = events.clone();
            std::thread::spawn(move || {
                events.unsubscribe();
                done.send(()).unwrap();
            })
        };
        assert!(completion.recv_timeout(Duration::from_millis(20)).is_err());
        *gate.0.lock().unwrap() = false;
        gate.1.notify_all();
        completion.recv_timeout(Duration::from_secs(2)).unwrap();
        producer.join().unwrap();
        unsubscriber.join().unwrap();
        events.ready();
        assert_eq!(receiver.frames.load(Ordering::SeqCst), 1);
    }
}
