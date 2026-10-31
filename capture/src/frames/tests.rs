use super::*;
use crate::test_support::SourceScript;

const EPOCH_NS: u64 = 10_000_000;

fn stream() -> (StreamFrames, Arc<SourceScript>) {
    (
        StreamFrames::new(41, EPOCH_NS),
        Arc::new(SourceScript::default()),
    )
}

fn store(frames: &StreamFrames, script: &SourceScript, sequence: u64, timestamp_ns: u64) {
    frames.store(script.export(sequence, timestamp_ns, false), sequence);
}

fn deliver(frames: &StreamFrames, status: Status) -> Result<(DmaBufFrame, u32), Status> {
    let lent = frames.lend()?;
    let observed = (*lent.frame(), lent.freshness);
    assert_eq!(frames.finish(lent, status), status);
    Ok(observed)
}

#[test]
fn reads_are_pending_until_the_first_frame_is_stored() {
    let (frames, script) = stream();
    assert_eq!(frames.lend().err(), Some(MEDIA_PENDING));
    assert_eq!(
        frames.latest_presentation(),
        Ok(PresentationStatus::default())
    );
    store(&frames, &script, 3, EPOCH_NS + 1_000_000);
    assert_eq!(deliver(&frames, STATUS_OK).unwrap().0.sequence, 3);
}

#[test]
fn a_stored_frame_is_fresh_once_then_cached_reuse_until_a_newer_one() {
    let (frames, script) = stream();
    store(&frames, &script, 5, EPOCH_NS + 2_000_000);
    let (first, freshness) = deliver(&frames, STATUS_OK).unwrap();
    assert_eq!(freshness, CAPTURE_FRAME_FRESH);
    assert_eq!(first.sequence, 5);
    assert_eq!(first.timestamp_us, 2_000);
    let (repeat, freshness) = deliver(&frames, STATUS_OK).unwrap();
    assert_eq!(freshness, CAPTURE_FRAME_CACHED_REUSE);
    // The repeat is the same scanout buffer.
    assert_eq!(repeat.planes[0].fd, first.planes[0].fd);

    store(&frames, &script, 6, EPOCH_NS + 18_000_000);
    let (third, freshness) = deliver(&frames, STATUS_OK).unwrap();
    assert_eq!(freshness, CAPTURE_FRAME_FRESH);
    assert_eq!(third.timestamp_us, 18_000);
    assert_eq!(frames.delivered(), 3);
}

#[test]
fn a_rejected_fresh_frame_stays_fresh() {
    let (frames, script) = stream();
    store(&frames, &script, 9, EPOCH_NS);
    assert_eq!(deliver(&frames, MEDIA_BUSY).unwrap().1, CAPTURE_FRAME_FRESH);
    assert_eq!(deliver(&frames, STATUS_OK).unwrap().1, CAPTURE_FRAME_FRESH);
    assert_eq!(
        deliver(&frames, STATUS_OK).unwrap().1,
        CAPTURE_FRAME_CACHED_REUSE
    );
    assert_eq!(frames.delivered(), 2);
}

#[test]
fn replacing_the_newest_frame_closes_the_previous_descriptors() {
    let (frames, script) = stream();
    frames.store(script.export(1, EPOCH_NS, true), 1);
    assert_eq!(script.open_descriptors(), 2);
    frames.store(script.export(2, EPOCH_NS, true), 2);
    // Only the newest frame's DMA-BUF and fence remain open.
    assert_eq!(script.open_descriptors(), 2);
    assert_eq!(script.writers.lock().unwrap().len(), 4);
    drop(frames);
    assert!(script.all_lent_descriptors_closed());
}

#[test]
fn a_frame_replaced_during_a_read_stays_open_until_the_read_ends() {
    let (frames, script) = stream();
    store(&frames, &script, 1, EPOCH_NS);
    let lent = frames.lend().unwrap();
    store(&frames, &script, 2, EPOCH_NS);
    assert_eq!(script.open_descriptors(), 2);
    assert_eq!(lent.frame().sequence, 1);
    frames.finish(lent, STATUS_OK);
    assert_eq!(script.open_descriptors(), 1);
    assert_eq!(deliver(&frames, STATUS_OK).unwrap().1, CAPTURE_FRAME_FRESH);
}

#[test]
fn a_concurrent_read_of_the_same_stream_is_busy() {
    let (frames, script) = stream();
    store(&frames, &script, 1, EPOCH_NS);
    let lent = frames.lend().unwrap();
    assert_eq!(frames.lend().err(), Some(MEDIA_BUSY));
    frames.finish(lent, STATUS_OK);
    assert!(frames.lend().is_ok());
}

#[test]
fn timestamps_start_at_stream_open_and_never_decrease() {
    let (frames, script) = stream();
    let mut stamps = Vec::new();
    for (sequence, timestamp_ns) in [
        (1, EPOCH_NS - 5_000_000),
        (2, EPOCH_NS + 7_000_000),
        (3, EPOCH_NS + 6_000_000),
    ] {
        store(&frames, &script, sequence, timestamp_ns);
        let latest = frames.latest_presentation().unwrap();
        assert_eq!(latest.sequence, sequence);
        assert_eq!(
            deliver(&frames, STATUS_OK).unwrap().0.timestamp_us,
            latest.timestamp_us
        );
        stamps.push(latest.timestamp_us);
    }
    assert_eq!(stamps, vec![0, 7_000, 7_000]);
}

#[test]
fn the_lent_frame_describes_the_export() {
    let (frames, script) = stream();
    frames.store(script.export(4, 1_000, true), 4);
    let lent = frames.lend().unwrap();
    let frame = *lent.frame();
    assert!(frame.validate().is_ok());
    assert_eq!(frame.width, 1920);
    assert_eq!(frame.fourcc, u32::from_le_bytes(*b"XR24"));
    assert_eq!(frame.modifier, 0x0100_0000_0000_0004);
    assert_eq!(frame.color, DMABUF_COLOR_SRGB);
    assert_eq!(frame.planes[0].pitch, 7680);
    assert!(frame.planes[0].fd >= 0);
    assert!(frame.planes[0].acquire_fence_fd >= 0);
    assert_eq!(frame.planes[1], DmaBufPlane::UNUSED);
    frames.finish(lent, STATUS_OK);
    // The slot keeps ownership after the read.
    assert!(!script.all_lent_descriptors_closed());
}

#[test]
fn an_oversized_presentation_is_unsupported_and_closed() {
    let (frames, script) = stream();
    let mut exported = script.export(2, EPOCH_NS, false);
    exported.descriptor.width = DMABUF_MAX_DIMENSION + 1;
    frames.store(exported, 2);
    assert_eq!(frames.lend().err(), Some(MEDIA_UNSUPPORTED));
    assert_eq!(frames.latest_presentation().unwrap().sequence, 2);
    assert!(script.all_lent_descriptors_closed());
}

#[test]
fn faults_end_delivery_and_keep_the_first_cause() {
    let (frames, script) = stream();
    store(&frames, &script, 1, EPOCH_NS);
    frames.set_fault(StreamFault::HelperFailed);
    assert_eq!(frames.lend().err(), Some(MEDIA_BACKEND_ERROR));
    assert_eq!(frames.latest_presentation(), Err(MEDIA_BACKEND_ERROR));

    let (frames, _) = stream();
    frames.set_fault(StreamFault::OutputLost("output_disconnected"));
    assert_eq!(frames.lend().err(), Some(MEDIA_CLOSED));
    assert_eq!(frames.latest_presentation(), Err(MEDIA_CLOSED));
    frames.set_fault(StreamFault::HelperFailed);
    assert_eq!(
        frames.fault(),
        Some(StreamFault::OutputLost("output_disconnected"))
    );
}

#[test]
fn resolve_requests_are_taken_once() {
    let (frames, _) = stream();
    assert!(!frames.take_resolve_request());
    frames.request_resolve();
    assert!(frames.take_resolve_request());
    assert!(!frames.take_resolve_request());
}

#[test]
fn producer_lease_is_consumed_only_after_acceptance_and_retention_controls_release() {
    let _serial = crate::leases::TEST_LEASE_CALLBACKS.lock().unwrap();
    let frames = StreamFrames::new(41, 0);
    let source = SourceScript::default();
    frames.store_leased(source.export(7, 7000, false), 7, 41);
    let lent = frames.lend().unwrap();
    assert!(lent.lease().is_ok());
    assert_eq!(frames.finish(lent, MEDIA_BUSY), MEDIA_BUSY);
    assert_eq!(source.open_descriptors(), 1);

    let lent = frames.lend().unwrap();
    let lease = lent.lease().unwrap();
    // SAFETY: the lent frame owns the callback context until finish; retain
    // adds the reference representing an asynchronous encoder GPU read.
    assert_eq!(unsafe { lease.retain.unwrap()(lease.context) }, STATUS_OK);
    frames.finish(lent, STATUS_OK);
    assert!(matches!(frames.lend(), Err(MEDIA_PENDING)));
    assert_eq!(source.open_descriptors(), 1);
    // SAFETY: simulates a completed GPU read, consuming exactly one retain.
    unsafe { lease.release.unwrap()(lease.context, -1) };
    assert!(source.all_lent_descriptors_closed());
}
