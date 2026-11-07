use super::*;
use crate::test_support::{FakeSource, Scripted, ScriptedWait, SourceScript, eventually};
use amaterasu_media::{CAPTURE_FRAME_FRESH, MEDIA_BACKEND_ERROR, MEDIA_PENDING};
use amaterasu_module_sdk::abi::STATUS_OK;
use std::time::Instant;

const EPOCH_NS: u64 = 1_000_000;

#[test]
fn diagnostic_sequences_count_jumps_without_counting_initial_or_retarget_offsets() {
    let mut metrics = DiagnosticWindow::new(Instant::now());
    metrics.observe_sequence(0, 80, 82);
    assert_eq!(metrics.sequence_gaps, 0);
    assert_eq!(metrics.export_ahead_sequences, 2);
    metrics.observe_sequence(82, 83, 86);
    assert_eq!(metrics.sequence_gaps, 3);
    assert_eq!(metrics.export_ahead_sequences, 5);
    // Retargeting resets the source sequence: the first new value must not
    // interpret the new CRTC's absolute counter as missed presentations.
    metrics.observe_sequence(0, 1000, 1000);
    assert_eq!(metrics.sequence_gaps, 3);
    metrics.observe_sequence(1000, 1001, 1001);
    assert_eq!(metrics.sequence_gaps, 3);
}

#[test]
fn presenter_reports_driver_gaps_through_sdk_logger_without_changing_delivery() {
    use amaterasu_module_sdk::{
        RuntimeClient,
        abi::{AbiHeader, BorrowedBytes, HostApi, ReplySink, Status},
    };
    use std::ffi::c_void;

    unsafe extern "C" fn invoke(
        context: *mut c_void,
        input: BorrowedBytes,
        reply: ReplySink,
    ) -> Status {
        // SAFETY: test retains the collector and input for this synchronous call.
        let collector = unsafe { &mut *context.cast::<Vec<serde_json::Value>>() };
        let bytes = unsafe { input.read() }.unwrap();
        collector.push(serde_json::from_slice(bytes).unwrap());
        // SAFETY: runtime owns the sink until the callback returns.
        unsafe { reply.send(br#"{"kind":"ok"}"#) };
        STATUS_OK
    }

    let mut records = Vec::<serde_json::Value>::new();
    let endpoint = HostApi {
        header: AbiHeader::of::<HostApi>(),
        context: (&mut records as *mut Vec<serde_json::Value>).cast(),
        invoke,
    };
    // SAFETY: the test retains the endpoint and collector until all calls end.
    let runtime = unsafe { RuntimeClient::from_endpoint(endpoint) };
    let (logger, mut queue) = runtime.log_emitter();
    let script = Arc::new(SourceScript::default());
    let frames = Arc::new(StreamFrames::new(41, EPOCH_NS));
    let mut presenter = Presenter::new(
        Box::new(FakeSource::new(script.clone())),
        frames.clone(),
        Arc::new(AtomicBool::new(false)),
        None,
    );
    presenter.logger = Some(logger);
    presenter.stream_id = 77;
    presenter.publish(script.export(1, EPOCH_NS, false), 1);
    presenter.publish(script.export(4, EPOCH_NS + 24_000_000, false), 3);
    assert_eq!(frames.latest_presentation().unwrap().sequence, 4);
    let started = presenter.diagnostics.started;
    presenter.log_diagnostics(started + Duration::from_secs(10), false);
    queue.flush(&runtime);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["op"], "log");
    assert_eq!(records[0]["target"], "capture.presentation");
    let message = records[0]["message"].as_str().unwrap();
    assert!(message.contains("stream=77 "));
    assert!(message.contains("publish_attempts=2 "));
    assert!(message.contains("sequence_gaps=2 "));
    assert!(message.contains("export_ahead_sequences=1 "));
    presenter.log_diagnostics(started + Duration::from_secs(11), false);
    queue.flush(&runtime);
    assert_eq!(records.len(), 1);
    presenter
        .diagnostics
        .wait
        .record(Duration::from_micros(123));
    presenter.log_diagnostics(started + Duration::from_secs(11), true);
    queue.flush(&runtime);
    assert_eq!(records.len(), 2);
    let final_message = records[1]["message"].as_str().unwrap();
    assert!(final_message.contains("final_window=true "));
    assert!(final_message.contains("wait_calls=1 wait_total_us=123 "));
}

struct Running {
    frames: Arc<StreamFrames>,
    script: Arc<SourceScript>,
    thread: PresentationThread,
}

fn running() -> Running {
    let script = Arc::new(SourceScript::default());
    let frames = Arc::new(StreamFrames::new(41, EPOCH_NS));
    let source = Box::new(FakeSource::new(script.clone()));
    let thread = PresentationThread::spawn(source, frames.clone()).unwrap();
    Running {
        frames,
        script,
        thread,
    }
}

impl Running {
    fn latest_sequence(&self) -> u64 {
        self.frames
            .latest_presentation()
            .map_or(0, |latest| latest.sequence)
    }

    fn reaches(&self, sequence: u64) -> bool {
        eventually(|| self.latest_sequence() == sequence)
    }

    fn waited_for(&self, crtc_id: u32, last_sequence: u64) -> bool {
        eventually(|| {
            self.script
                .waits
                .lock()
                .unwrap()
                .contains(&(crtc_id, last_sequence))
        })
    }
}

#[test]
fn each_presentation_is_stored_as_it_completes() {
    let run = running();
    assert_eq!(run.frames.lend().err(), Some(MEDIA_PENDING));
    run.script.present(1, EPOCH_NS + 16_000_000);
    assert!(run.reaches(1));
    let lent = run.frames.lend().unwrap();
    assert_eq!(lent.freshness, CAPTURE_FRAME_FRESH);
    assert_eq!(lent.frame().timestamp_us, 16_000);
    run.frames.finish(lent, STATUS_OK);

    run.script.present(2, EPOCH_NS + 23_000_000);
    assert!(run.reaches(2));
    let latest = run.frames.latest_presentation().unwrap();
    assert_eq!(latest.timestamp_us, 23_000);
    // The thread starts from nothing and then waits for a sequence newer
    // than the one it stored.
    assert_eq!(run.script.waits.lock().unwrap()[0], (41, 0));
    assert!(run.waited_for(41, 2));
}

#[test]
fn replaced_frames_are_closed_as_newer_ones_arrive() {
    let run = running();
    for sequence in 1..=5 {
        run.script.present_fenced(sequence);
        assert!(run.reaches(sequence));
    }
    assert_eq!(run.script.writers.lock().unwrap().len(), 10);
    // Only the newest DMA-BUF and its fence remain open.
    assert_eq!(run.script.open_descriptors(), 2);
}

#[test]
fn presentation_sequences_and_timestamps_never_decrease() {
    let run = running();
    let frames = run.frames.clone();
    let observer = std::thread::spawn(move || {
        let mut seen = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let latest = frames.latest_presentation().unwrap();
            seen.push(latest);
            if latest.sequence == 40 {
                break;
            }
        }
        seen
    });
    for sequence in 1..=40 {
        // Every fifth timestamp steps backward; the stream clamps it.
        let offset = if sequence % 5 == 0 { 0 } else { sequence };
        run.script.present(sequence, EPOCH_NS + offset * 8_000_000);
    }
    let seen = observer.join().unwrap();
    assert_eq!(seen.last().unwrap().sequence, 40);
    assert!(seen.windows(2).all(|pair| {
        pair[0].sequence <= pair[1].sequence && pair[0].timestamp_us <= pair[1].timestamp_us
    }));
}

#[test]
fn a_frame_newer_than_the_wait_coalesces_the_gap() {
    let run = running();
    run.script.push(Scripted::Frame {
        sequence: 5,
        timestamp_ns: EPOCH_NS,
    });
    run.script.push_wait(ScriptedWait::Presented(3));
    assert!(run.reaches(5));
    assert!(run.waited_for(41, 5));
}

#[test]
fn an_empty_crtc_waits_for_the_next_presentation_without_spinning() {
    let run = running();
    run.script.push_wait(ScriptedWait::Presented(1));
    run.script.push(Scripted::Empty);
    assert!(run.waited_for(41, 1));
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(run.script.requested_crtcs.lock().unwrap().len(), 1);
    assert_eq!(run.frames.lend().err(), Some(MEDIA_PENDING));
    assert_eq!(run.latest_sequence(), 0);
}

#[test]
fn busy_and_interrupted_drivers_keep_waiting() {
    let run = running();
    run.script.push_wait(ScriptedWait::Refuse(Failure::Busy));
    run.script
        .push_wait(ScriptedWait::Refuse(Failure::Interrupted));
    run.script.present(1, EPOCH_NS);
    assert!(run.reaches(1));
    assert_eq!(run.frames.fault(), None);
}

#[test]
fn a_missing_crtc_requests_re_resolution_and_follows_the_retarget() {
    let run = running();
    run.script.present(7, EPOCH_NS);
    assert!(run.reaches(7));
    run.script
        .push_wait(ScriptedWait::Refuse(Failure::CrtcNotFound));
    assert!(eventually(|| run.frames.take_resolve_request()));
    assert_eq!(run.frames.fault(), None);

    // The controller resolves the output to a new CRTC whose counter
    // restarted; the stream sequence continues past the last stored one.
    run.frames.retarget(60);
    run.thread.wake();
    run.script.present(1, EPOCH_NS);
    assert!(run.reaches(8));
    assert!(run.waited_for(60, 0));
    let last_frame_crtc = *run.script.requested_crtcs.lock().unwrap().last().unwrap();
    assert_eq!(last_frame_crtc, 60);
    assert_eq!(run.frames.lend().unwrap().frame().sequence, 8);
}

#[test]
fn a_refused_export_is_handled_like_a_refused_wait() {
    let run = running();
    run.script.push(Scripted::Refuse(Failure::CrtcNotFound));
    run.script.push_wait(ScriptedWait::Presented(1));
    assert!(eventually(|| run.frames.take_resolve_request()));
    run.thread.wake();
    run.script.present(2, EPOCH_NS);
    assert!(run.reaches(2));
    assert_eq!(run.frames.fault(), None);
}

#[test]
fn a_lost_helper_or_refused_permission_faults_the_stream() {
    for (answer, fault) in [
        (ScriptedWait::HelperGone, StreamFault::HelperFailed),
        (
            ScriptedWait::Refuse(Failure::PermissionDenied),
            StreamFault::PermissionDenied,
        ),
    ] {
        let run = running();
        run.script.present(1, EPOCH_NS);
        assert!(run.reaches(1));
        run.script.push_wait(answer);
        assert!(eventually(|| run.thread.is_finished()));
        assert_eq!(run.frames.fault(), Some(fault));
        assert_eq!(run.frames.lend().err(), Some(MEDIA_BACKEND_ERROR));
        assert_eq!(run.frames.latest_presentation(), Err(MEDIA_BACKEND_ERROR));
    }
}

#[test]
fn a_failed_export_faults_the_stream() {
    let run = running();
    run.script.push(Scripted::HelperGone);
    run.script.push_wait(ScriptedWait::Presented(1));
    assert!(eventually(|| run.thread.is_finished()));
    assert_eq!(run.frames.fault(), Some(StreamFault::HelperFailed));
}

#[test]
fn persistent_driver_errors_fault_the_stream() {
    let run = running();
    for _ in 0..MAX_CONSECUTIVE_DRIVER_ERRORS {
        run.script
            .push_wait(ScriptedWait::Refuse(Failure::DriverError));
    }
    assert!(eventually(|| run.thread.is_finished()));
    assert_eq!(run.frames.fault(), Some(StreamFault::HelperFailed));
}

#[test]
fn an_isolated_driver_error_is_retried() {
    let run = running();
    run.script
        .push_wait(ScriptedWait::Refuse(Failure::DriverError));
    run.script.present(1, EPOCH_NS);
    assert!(run.reaches(1));
    assert_eq!(run.frames.fault(), None);
}

#[test]
fn an_external_fault_ends_the_thread_after_a_wake() {
    let run = running();
    run.frames
        .set_fault(StreamFault::OutputLost("output_disconnected"));
    run.thread.wake();
    assert!(eventually(|| run.thread.is_finished()));
}

#[test]
fn stop_interrupts_a_pending_wait_promptly() {
    let mut run = running();
    assert!(run.waited_for(41, 0));
    let started = Instant::now();
    run.thread.stop();
    let stopped = started.elapsed();
    assert!(run.thread.is_finished());
    let bound = Duration::from_millis(PRESENT_WAIT_TIMEOUT_MS.into()) / 2;
    assert!(stopped < bound, "{stopped:?}");
    // Stopping is not a helper failure, and stopping twice is harmless.
    assert_eq!(run.frames.fault(), None);
    run.thread.stop();
}

#[test]
fn cancellation_after_a_successful_wait_does_not_start_a_frame_export() {
    struct StopAfterWait {
        source: FakeSource,
        stop: Arc<AtomicBool>,
    }
    impl FrameSource for StopAfterWait {
        fn wait(
            &mut self,
            crtc: u32,
            sequence: u64,
            timeout: u32,
        ) -> Result<
            amaterasu_vkms_frame_helper::client::Presentation,
            amaterasu_vkms_frame_helper::client::ClientError,
        > {
            let result = self.source.wait(crtc, sequence, timeout);
            self.stop.store(true, Ordering::SeqCst);
            result
        }
        fn frame(
            &mut self,
            crtc: u32,
        ) -> Result<
            amaterasu_vkms_frame_helper::client::FrameExport,
            amaterasu_vkms_frame_helper::client::ClientError,
        > {
            self.source.frame(crtc)
        }
        fn interrupter(&self) -> Arc<dyn SourceInterrupt> {
            self.source.interrupter()
        }
    }
    let script = Arc::new(SourceScript::default());
    script.present(1, EPOCH_NS);
    let stop = Arc::new(AtomicBool::new(false));
    let frames = Arc::new(StreamFrames::new(41, EPOCH_NS));
    let mut presenter = Presenter::new(
        Box::new(StopAfterWait {
            source: FakeSource::new(script.clone()),
            stop: stop.clone(),
        }),
        frames.clone(),
        stop,
        None,
    );

    assert!(matches!(presenter.step(), Next::Exit));
    assert!(script.requested_crtcs.lock().unwrap().is_empty());
    assert_eq!(frames.latest_presentation().unwrap().sequence, 0);
}

#[test]
fn stop_is_bounded_by_one_wait_when_the_helper_ignores_interrupts() {
    let mut run = running();
    run.script.ignore_interrupts.store(true, Ordering::SeqCst);
    assert!(run.waited_for(41, 0));
    let started = Instant::now();
    run.thread.stop();
    let stopped = started.elapsed();
    let bound = Duration::from_millis(PRESENT_WAIT_TIMEOUT_MS.into()) + Duration::from_millis(100);
    assert!(stopped <= bound, "{stopped:?}");
    assert_eq!(run.frames.fault(), None);
}

#[test]
fn stopping_the_thread_keeps_the_stored_frame_until_the_stream_drops() {
    let run = running();
    run.script.present(1, EPOCH_NS);
    assert!(run.reaches(1));
    let Running {
        frames,
        script,
        thread,
    } = run;
    drop(thread);
    assert_eq!(script.open_descriptors(), 1);
    assert_eq!(frames.lend().unwrap().frame().sequence, 1);
    drop(frames);
    assert!(script.all_lent_descriptors_closed());
}
