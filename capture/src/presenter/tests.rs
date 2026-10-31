use super::*;
use crate::test_support::{FakeSource, Scripted, ScriptedWait, SourceScript, eventually};
use amaterasu_media::{CAPTURE_FRAME_FRESH, MEDIA_BACKEND_ERROR, MEDIA_PENDING};
use amaterasu_module_sdk::abi::STATUS_OK;
use std::time::Instant;

const EPOCH_NS: u64 = 1_000_000;

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
    run.script.push_wait(ScriptedWait::Presented(1));
    run.script.push(Scripted::Refuse(Failure::CrtcNotFound));
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
