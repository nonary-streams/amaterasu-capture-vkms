//! The per-stream presentation thread.
//!
//! Each open stream has one thread that owns the stream's helper. It blocks
//! in the driver's WAIT_PRESENT for a sequence newer than the last one it
//! saw, exports that presentation with FRAME, and stores it as the stream's
//! newest frame. Native reads and presentation queries never talk to the
//! helper, so frames become available at the compositor's presentation rate,
//! including variable refresh, instead of being sampled on a fixed timer.
use crate::{
    frames::{StreamFault, StreamFrames},
    snapshot::Snapshotter,
};
use amaterasu_vkms_frame_helper::{
    client::{ClientError, ExportedFrame, FrameExport, Presentation},
    protocol::{Failure, PRESENT_CHANGED},
};
use std::{
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

/// Longest single WAIT. It bounds how long a thread that cannot be
/// interrupted takes to notice a stop or a fault.
pub const PRESENT_WAIT_TIMEOUT_MS: u32 = 100;
/// Pause after the driver reported `busy` or `interrupted`.
const RETRY_DELAY: Duration = Duration::from_millis(5);
/// Pause while the CRTC is gone and the controller re-resolves the output.
/// A retarget wakes the thread early.
const RESOLVE_RETRY_DELAY: Duration = Duration::from_millis(PRESENT_WAIT_TIMEOUT_MS as u64);
/// Pause after any other driver refusal.
const DRIVER_ERROR_DELAY: Duration = Duration::from_millis(20);
/// Consecutive driver refusals after which the helper is treated as failed.
const MAX_CONSECUTIVE_DRIVER_ERRORS: u32 = 10;

/// Exports presentations of one CRTC. The production source is a helper
/// process; the thread owning it is the only caller.
pub trait FrameSource: Send {
    /// Blocks until the CRTC completes a presentation newer than
    /// `last_sequence`, or for at most `timeout_ms`.
    fn wait(
        &mut self,
        crtc_id: u32,
        last_sequence: u64,
        timeout_ms: u32,
    ) -> Result<Presentation, ClientError>;
    /// Exports the newest completed presentation without waiting.
    fn frame(&mut self, crtc_id: u32) -> Result<FrameExport, ClientError>;
    /// A handle that makes a blocked `wait` or `frame` return promptly.
    fn interrupter(&self) -> Arc<dyn SourceInterrupt>;
}

/// Wakes a source's blocked call from another thread. After an interrupt
/// the source fails every call.
pub trait SourceInterrupt: Send + Sync {
    fn interrupt(&self);
}

/// Owns one stream's presentation thread. Dropping it stops the thread.
pub struct PresentationThread {
    stop: Arc<AtomicBool>,
    interrupt: Arc<dyn SourceInterrupt>,
    handle: Option<JoinHandle<()>>,
}

impl PresentationThread {
    #[cfg(test)]
    pub fn spawn(source: Box<dyn FrameSource>, frames: Arc<StreamFrames>) -> io::Result<Self> {
        Self::spawn_with_snapshot(source, frames, false)
    }

    pub fn spawn_with_snapshot(
        source: Box<dyn FrameSource>,
        frames: Arc<StreamFrames>,
        gpu_snapshot: bool,
    ) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let interrupt = source.interrupter();
        let child_stop = stop.clone();
        let (ready, initialized) = std::sync::mpsc::sync_channel(1);
        let handle = thread::Builder::new()
            .name("vkms-presentation".into())
            .spawn(move || {
                let snapshotter = if gpu_snapshot {
                    match Snapshotter::new() {
                        Ok(snapshotter) => Some(snapshotter),
                        Err(_) => {
                            let _ = ready.send(false);
                            return;
                        }
                    }
                } else {
                    None
                };
                let presenter = Presenter::new(source, frames.clone(), child_stop, snapshotter);
                let _ = ready.send(true);
                if catch_unwind(AssertUnwindSafe(|| presenter.run())).is_err() {
                    frames.set_fault(StreamFault::HelperFailed);
                }
            })?;
        if initialized.recv() != Ok(true) {
            let _ = handle.join();
            return Err(io::Error::other(
                "immutable GPU snapshot backend unavailable",
            ));
        }
        Ok(Self {
            stop,
            interrupt,
            handle: Some(handle),
        })
    }

    /// Ends a pause early, for example after the controller retargeted the
    /// stream or faulted it.
    pub fn wake(&self) {
        if let Some(handle) = &self.handle {
            handle.thread().unpark();
        }
    }

    /// Stops the thread and waits for it, which also ends the helper.
    /// Interrupting the source makes a pending WAIT or FRAME return at once,
    /// so this takes about as long as the helper takes to exit; a source that
    /// ignores interrupts still returns within one WAIT timeout.
    pub fn stop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        self.stop.store(true, Ordering::SeqCst);
        self.interrupt.interrupt();
        handle.thread().unpark();
        let _ = handle.join();
    }

    #[cfg(test)]
    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for PresentationThread {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What the loop does after one step.
enum Next {
    Continue,
    Pause(Duration),
    Exit,
}

/// The thread's state: the source and the CRTC sequence it has seen.
struct Presenter {
    source: Box<dyn FrameSource>,
    frames: Arc<StreamFrames>,
    stop: Arc<AtomicBool>,
    crtc_id: u32,
    /// Newest driver sequence seen on `crtc_id`; 0 before any.
    driver_sequence: u64,
    /// Added to driver sequences so stream sequences keep increasing when
    /// the output moves to another CRTC with its own counter.
    sequence_base: u64,
    consecutive_driver_errors: u32,
    snapshotter: Option<Snapshotter>,
}

impl Presenter {
    fn new(
        source: Box<dyn FrameSource>,
        frames: Arc<StreamFrames>,
        stop: Arc<AtomicBool>,
        snapshotter: Option<Snapshotter>,
    ) -> Self {
        Self {
            crtc_id: frames.crtc_id(),
            source,
            frames,
            stop,
            driver_sequence: 0,
            sequence_base: 0,
            consecutive_driver_errors: 0,
            snapshotter,
        }
    }

    fn run(mut self) {
        while !self.finished() {
            match self.step() {
                Next::Continue => {}
                Next::Pause(delay) => thread::park_timeout(delay),
                Next::Exit => break,
            }
        }
    }

    /// Stop was requested, or the stream faulted and the helper is no
    /// longer needed.
    fn finished(&self) -> bool {
        self.stop.load(Ordering::SeqCst) || self.frames.fault().is_some()
    }

    /// Waits for one newer presentation and stores it.
    fn step(&mut self) -> Next {
        self.follow_retarget();
        let presentation =
            match self
                .source
                .wait(self.crtc_id, self.driver_sequence, PRESENT_WAIT_TIMEOUT_MS)
            {
                Ok(presentation) => presentation,
                Err(error) => return self.failed(&error),
            };
        if presentation.flags & PRESENT_CHANGED == 0 {
            // A timeout: nothing new was presented.
            self.consecutive_driver_errors = 0;
            return Next::Continue;
        }
        match self.source.frame(self.crtc_id) {
            Ok(FrameExport::Ready(exported)) => {
                self.consecutive_driver_errors = 0;
                self.publish(exported, presentation.sequence);
                Next::Continue
            }
            Ok(FrameExport::Empty) => {
                // Nothing exportable yet; wait for the next presentation
                // rather than asking again for this one.
                self.consecutive_driver_errors = 0;
                self.driver_sequence = self.driver_sequence.max(presentation.sequence);
                Next::Continue
            }
            Err(error) => self.failed(&error),
        }
    }

    /// Starts over on the CRTC the controller resolved, continuing the
    /// stream sequence after the last stored one.
    fn follow_retarget(&mut self) {
        let crtc_id = self.frames.crtc_id();
        if crtc_id == self.crtc_id {
            return;
        }
        self.crtc_id = crtc_id;
        self.driver_sequence = 0;
        self.sequence_base = self
            .frames
            .latest_presentation()
            .map_or(self.sequence_base, |latest| latest.sequence);
    }

    /// Stores a frame newer than any stored before. FRAME returns the newest
    /// completed scanout, which may already be newer than the WAIT result;
    /// gaps are coalesced.
    fn publish(&mut self, exported: ExportedFrame, waited_sequence: u64) {
        let exported_sequence = exported.descriptor.sequence;
        let previous = self.driver_sequence;
        self.driver_sequence = previous.max(exported_sequence).max(waited_sequence);
        if exported_sequence > previous {
            let stream_sequence = self.sequence_base.saturating_add(exported_sequence);
            if let Some(snapshotter) = &mut self.snapshotter {
                match snapshotter.snapshot(exported) {
                    Ok(snapshot) => self.frames.store_snapshot(snapshot, stream_sequence),
                    Err(_) => self.frames.set_fault(StreamFault::GpuSnapshotFailed),
                }
            } else {
                self.frames.store(exported, stream_sequence);
            }
        }
    }

    /// Decides how the loop continues after a helper error.
    fn failed(&mut self, error: &ClientError) -> Next {
        if self.stop.load(Ordering::SeqCst) {
            // An interrupt for shutdown, not a helper failure.
            return Next::Exit;
        }
        let Some(failure) = error.failure() else {
            // Closed, timed out or out of step: the helper is unusable.
            self.frames.set_fault(StreamFault::HelperFailed);
            return Next::Exit;
        };
        match failure {
            Failure::CrtcNotFound => {
                // The output may have moved to another CRTC; the controller
                // re-resolves it on its next refresh and wakes this thread.
                self.frames.request_resolve();
                Next::Pause(RESOLVE_RETRY_DELAY)
            }
            Failure::Busy | Failure::Interrupted => Next::Pause(RETRY_DELAY),
            Failure::PermissionDenied => {
                self.frames.set_fault(StreamFault::PermissionDenied);
                Next::Exit
            }
            _ => {
                self.consecutive_driver_errors += 1;
                if self.consecutive_driver_errors >= MAX_CONSECUTIVE_DRIVER_ERRORS {
                    self.frames.set_fault(StreamFault::HelperFailed);
                    return Next::Exit;
                }
                Next::Pause(DRIVER_ERROR_DELAY)
            }
        }
    }
}

#[cfg(test)]
mod tests;
