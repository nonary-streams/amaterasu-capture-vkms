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
use amaterasu_module_sdk::{LogEmitter, LogLevel};
use amaterasu_vkms_frame_helper::{
    client::{ClientError, ExportedFrame, FrameExport, Presentation},
    protocol::{Failure, PRESENT_CHANGED, PRESENT_TIMEOUT},
};
use std::{
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Longest single WAIT. It bounds how long a thread that cannot be
/// interrupted takes to notice a stop or a fault.
pub const PRESENT_WAIT_TIMEOUT_MS: u32 = 100;
// Poll direct sources promptly after a reader requests a frame and to expire
// unclaimed mailbox leases, without waiting for a blocked presentation.
const DIRECT_WAIT_TIMEOUT_MS: u32 = 5;
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
    /// Independent proof for a privileged helper outside the local worker.
    /// Sources with remote native ownership must supply the exact session receipt.
    fn retirement(&self) -> Option<Arc<dyn crate::retirement::Retirement>> {
        None
    }
    /// Source descriptors prevent producer retirement until their last release.
    fn holds_producer(&self) -> bool {
        false
    }
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
    retirement_failed: bool,
    worker_failed: Arc<AtomicBool>,
    source_retirement: Option<Arc<dyn crate::retirement::Retirement>>,
    source_retirement_error: Option<String>,
}

impl PresentationThread {
    #[cfg(test)]
    pub fn spawn(source: Box<dyn FrameSource>, frames: Arc<StreamFrames>) -> io::Result<Self> {
        Self::spawn_with_snapshot(source, frames, false, 0, None)
    }

    pub fn spawn_with_snapshot(
        source: Box<dyn FrameSource>,
        frames: Arc<StreamFrames>,
        gpu_snapshot: bool,
        stream_id: u64,
        logger: Option<LogEmitter>,
    ) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let interrupt = source.interrupter();
        let source_retirement = source.retirement();
        let child_stop = stop.clone();
        let worker_failed = Arc::new(AtomicBool::new(false));
        let child_failed = worker_failed.clone();
        let (ready, initialized) = std::sync::mpsc::sync_channel(1);
        let handle = thread::Builder::new()
            .name("vkms-presentation".into())
            .spawn(move || {
                crate::threading::configure_capture_thread();
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
                let mut presenter = Presenter::new(source, frames.clone(), child_stop, snapshotter);
                presenter.stream_id = stream_id;
                presenter.logger = logger;
                let _ = ready.send(true);
                if catch_unwind(AssertUnwindSafe(|| presenter.run())).is_err() {
                    child_failed.store(true, Ordering::SeqCst);
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
            retirement_failed: false,
            worker_failed,
            source_retirement,
            source_retirement_error: None,
        })
    }

    /// Ends a pause early, for example after the controller retargeted the
    /// stream or faulted it.
    pub fn wake(&self) {
        if let Some(handle) = &self.handle {
            handle.thread().unpark();
        }
    }

    /// Announce cancellation without waiting for helper, snapshot, or driver
    /// teardown. Only the owning worker destroys its source and native state.
    pub fn begin_stop(&self) {
        if !self.stop.swap(true, Ordering::SeqCst) {
            self.interrupt.interrupt();
        }
        self.wake();
    }

    /// Join only after the worker and all of its destructors have finished.
    pub fn poll_stop(&mut self) -> Result<bool, String> {
        self.begin_stop();
        if self
            .handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
        {
            return Ok(false);
        }
        if let Some(handle) = self.handle.take() {
            self.retirement_failed |= handle.join().is_err();
        }
        self.retirement_failed |= self.worker_failed.load(Ordering::SeqCst);
        let source_retired = match &self.source_retirement {
            Some(receipt) => match receipt.poll_retired() {
                Ok(retired) => retired,
                Err(error) => {
                    self.source_retirement_error.get_or_insert_with(|| {
                        format!("native helper retirement was not verified: {error}")
                    });
                    false
                }
            },
            None => true,
        };
        if self.retirement_failed {
            Err("presentation worker failed during retirement".into())
        } else if let Some(error) = &self.source_retirement_error {
            Err(error.clone())
        } else {
            Ok(source_retired)
        }
    }

    pub fn retirement_failed(&self) -> bool {
        self.retirement_failed
            || self.worker_failed.load(Ordering::SeqCst)
            || self.source_retirement_error.is_some()
    }

    /// Final containment waits for the worker; ordinary Close and quiescence
    /// use begin_stop/poll_stop so a driver or destructor cannot block the host.
    pub fn stop(&mut self) {
        self.begin_stop();
        if let Some(handle) = self.handle.take() {
            self.retirement_failed |= handle.join().is_err();
        }
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

#[derive(Default)]
struct DurationSamples {
    count: u64,
    total_us: u64,
    max_us: u64,
}

impl DurationSamples {
    fn record(&mut self, elapsed: Duration) {
        let micros = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
        self.count = self.count.saturating_add(1);
        self.total_us = self.total_us.saturating_add(micros);
        self.max_us = self.max_us.max(micros);
    }
}

struct DiagnosticWindow {
    started: Instant,
    changed: u64,
    timeouts: u64,
    sequence_gaps: u64,
    export_ahead_sequences: u64,
    empty_exports: u64,
    publish_attempts: u64,
    wait: DurationSamples,
    export: DurationSamples,
    snapshot: DurationSamples,
}

impl DiagnosticWindow {
    fn new(started: Instant) -> Self {
        Self {
            started,
            changed: 0,
            timeouts: 0,
            sequence_gaps: 0,
            export_ahead_sequences: 0,
            empty_exports: 0,
            publish_attempts: 0,
            wait: DurationSamples::default(),
            export: DurationSamples::default(),
            snapshot: DurationSamples::default(),
        }
    }

    fn observe_sequence(&mut self, previous: u64, waited: u64, exported: u64) {
        let latest = waited.max(exported);
        if previous != 0 {
            self.sequence_gaps = self
                .sequence_gaps
                .saturating_add(latest.saturating_sub(previous).saturating_sub(1));
        }
        self.export_ahead_sequences = self
            .export_ahead_sequences
            .saturating_add(exported.saturating_sub(waited));
    }
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
    producer_held: bool,
    stream_id: u64,
    logger: Option<LogEmitter>,
    diagnostics: DiagnosticWindow,
}

impl Presenter {
    fn new(
        source: Box<dyn FrameSource>,
        frames: Arc<StreamFrames>,
        stop: Arc<AtomicBool>,
        snapshotter: Option<Snapshotter>,
    ) -> Self {
        let producer_held = source.holds_producer();
        Self {
            producer_held,
            crtc_id: frames.crtc_id(),
            source,
            frames,
            stop,
            driver_sequence: 0,
            sequence_base: 0,
            consecutive_driver_errors: 0,
            snapshotter,
            stream_id: 0,
            logger: None,
            diagnostics: DiagnosticWindow::new(Instant::now()),
        }
    }

    fn run(mut self) {
        while !self.finished() {
            let next = self.step();
            self.log_diagnostics(Instant::now(), false);
            match next {
                Next::Continue => {}
                Next::Pause(delay) => thread::park_timeout(delay),
                Next::Exit => break,
            }
        }
        self.log_diagnostics(Instant::now(), true);
    }

    fn log_diagnostics(&mut self, now: Instant, final_window: bool) {
        if (!final_window
            && now.saturating_duration_since(self.diagnostics.started) < Duration::from_secs(10))
            || (final_window && self.diagnostics.wait.count == 0)
        {
            return;
        }
        let window = std::mem::replace(&mut self.diagnostics, DiagnosticWindow::new(now));
        if let Some(logger) = &self.logger {
            let message = format!(
                "stream={} window_us={} final_window={} snapshot_enabled={} changed={} timeouts={} publish_attempts={} sequence_gaps={} export_ahead_sequences={} empty_exports={} wait_calls={} wait_total_us={} wait_max_us={} export_calls={} export_total_us={} export_max_us={} snapshot_calls={} snapshot_total_us={} snapshot_max_us={}",
                self.stream_id,
                now.saturating_duration_since(window.started).as_micros(),
                final_window,
                self.snapshotter.is_some(),
                window.changed,
                window.timeouts,
                window.publish_attempts,
                window.sequence_gaps,
                window.export_ahead_sequences,
                window.empty_exports,
                window.wait.count,
                window.wait.total_us,
                window.wait.max_us,
                window.export.count,
                window.export.total_us,
                window.export.max_us,
                window.snapshot.count,
                window.snapshot.total_us,
                window.snapshot.max_us,
            );
            // Emission uses the SDK's bounded nonblocking producer; loss of
            // diagnostics never blocks capture or changes frame ownership.
            let _ = logger.emit(LogLevel::Info, "capture.presentation", &message);
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
        self.frames.expire_direct_frame(std::time::Instant::now());
        let wait_ms = if self.producer_held && self.snapshotter.is_none() {
            DIRECT_WAIT_TIMEOUT_MS
        } else {
            PRESENT_WAIT_TIMEOUT_MS
        };
        let started = Instant::now();
        let result = self
            .source
            .wait(self.crtc_id, self.driver_sequence, wait_ms);
        self.diagnostics.wait.record(started.elapsed());
        let presentation = match result {
            Ok(presentation) => presentation,
            Err(error) => return self.failed(&error),
        };
        if self.finished() {
            return Next::Exit;
        }
        let requested = self.producer_held && self.frames.take_capture_request();
        if presentation.flags & PRESENT_TIMEOUT != 0 {
            self.diagnostics.timeouts = self.diagnostics.timeouts.saturating_add(1);
        }
        if presentation.flags & PRESENT_CHANGED == 0 && !requested {
            // A timeout: nothing new was presented.
            self.consecutive_driver_errors = 0;
            return Next::Continue;
        }
        if presentation.flags & PRESENT_CHANGED != 0 {
            self.diagnostics.changed = self.diagnostics.changed.saturating_add(1);
        }
        // A direct driver lease blocks buffer retirement. Acquire it only when
        // video asks for a frame, never while an encoder is still starting.
        if self.producer_held && self.snapshotter.is_none() && !requested {
            self.diagnostics.observe_sequence(
                self.driver_sequence,
                presentation.sequence,
                presentation.sequence,
            );
            self.driver_sequence = self.driver_sequence.max(presentation.sequence);
            self.frames.observe_presentation(
                self.sequence_base.saturating_add(presentation.sequence),
                presentation.timestamp_ns,
            );
            return Next::Continue;
        }
        let started = Instant::now();
        let result = self.source.frame(self.crtc_id);
        self.diagnostics.export.record(started.elapsed());
        if self.finished() {
            return Next::Exit;
        }
        match result {
            Ok(FrameExport::Ready(exported)) => {
                self.consecutive_driver_errors = 0;
                self.publish(exported, presentation.sequence);
                Next::Continue
            }
            Ok(FrameExport::Empty) => {
                self.diagnostics.empty_exports = self.diagnostics.empty_exports.saturating_add(1);
                self.diagnostics.observe_sequence(
                    self.driver_sequence,
                    presentation.sequence,
                    presentation.sequence,
                );
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
        if self.finished() {
            return;
        }
        let exported_sequence = exported.descriptor.sequence;
        let previous = self.driver_sequence;
        self.diagnostics
            .observe_sequence(previous, waited_sequence, exported_sequence);
        self.driver_sequence = previous.max(exported_sequence).max(waited_sequence);
        if exported_sequence > previous || self.producer_held {
            let stream_sequence = self.sequence_base.saturating_add(exported_sequence);
            if let Some(snapshotter) = &mut self.snapshotter {
                let started = Instant::now();
                let snapshot = snapshotter.snapshot(exported);
                self.diagnostics.snapshot.record(started.elapsed());
                match snapshot {
                    Ok(snapshot) => {
                        self.frames.store_snapshot(snapshot, stream_sequence);
                        self.diagnostics.publish_attempts =
                            self.diagnostics.publish_attempts.saturating_add(1);
                    }
                    Err(_) => self.frames.set_fault(StreamFault::GpuSnapshotFailed),
                }
            } else if self.producer_held {
                self.frames
                    .store_leased(exported, stream_sequence, self.crtc_id);
                self.diagnostics.publish_attempts =
                    self.diagnostics.publish_attempts.saturating_add(1);
            } else {
                self.frames.store(exported, stream_sequence);
                self.diagnostics.publish_attempts =
                    self.diagnostics.publish_attempts.saturating_add(1);
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
