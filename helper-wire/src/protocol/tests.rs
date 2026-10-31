use super::*;

fn frame() -> FrameDescriptor {
    FrameDescriptor {
        crtc_id: 41,
        sequence: 7,
        timestamp_ns: 1_000_000,
        width: 1920,
        height: 1080,
        fourcc: u32::from_le_bytes(*b"XR24"),
        modifier: 0x0200_0000_0000_0001,
        plane_count: 2,
        pitches: [7680, 3840, 0, 0],
        offsets: [0, 4096, 0, 0],
        fence_mask: 0b01,
        guard_count: 0,

        color: 1,
        hdr_metadata: [0; 12],
    }
}

fn request_bytes(request: Request) -> [u8; REQUEST_LEN] {
    encode_request(&RequestRecord {
        request_id: 9,
        request,
    })
}

#[test]
fn every_request_round_trips() {
    for request in [
        Request::Hello,
        Request::OpenCard {
            major: 226,
            minor: 3,
        },
        Request::Frame { crtc_id: 41 },
        Request::Wait {
            crtc_id: 41,
            last_sequence: u64::MAX,
            timeout_ms: MAX_WAIT_TIMEOUT_MS,
        },
    ] {
        let decoded = decode_request(&request_bytes(request)).unwrap();
        assert_eq!(decoded.request, request);
        assert_eq!(decoded.request_id, 9);
    }
}

#[test]
fn requests_with_wrong_length_magic_or_version_are_rejected() {
    let bytes = request_bytes(Request::Hello);
    assert_eq!(
        decode_request(&bytes[..REQUEST_LEN - 1])
            .unwrap_err()
            .failure,
        Failure::MalformedRequest
    );
    let mut longer = bytes.to_vec();
    longer.push(0);
    assert!(decode_request(&longer).is_err());

    let mut magic = bytes;
    magic[0] ^= 1;
    let rejection = decode_request(&magic).unwrap_err();
    assert_eq!(rejection.failure, Failure::MalformedRequest);
    assert_eq!(rejection.request_id, 0, "untrusted ids are not echoed");

    let mut version = bytes;
    version[4] = (PROTOCOL_VERSION + 1) as u8;
    let rejection = decode_request(&version).unwrap_err();
    assert_eq!(rejection.failure, Failure::UnsupportedVersion);
    assert_eq!(rejection.request_id, 9);
}

#[test]
fn unused_fields_and_reserved_bytes_must_be_zero() {
    let mut reserved = request_bytes(Request::Frame { crtc_id: 41 });
    reserved[REQUEST_LEN - 1] = 1;
    assert_eq!(
        decode_request(&reserved).unwrap_err().opcode,
        Opcode::Frame as u16
    );

    let mut hello_with_crtc = request_bytes(Request::Hello);
    hello_with_crtc[12] = 1;
    assert!(decode_request(&hello_with_crtc).is_err());

    let mut frame_with_device = request_bytes(Request::Frame { crtc_id: 41 });
    frame_with_device[16] = 226;
    assert!(decode_request(&frame_with_device).is_err());

    let mut frame_with_timeout = request_bytes(Request::Frame { crtc_id: 41 });
    frame_with_timeout[32] = 1;
    assert!(decode_request(&frame_with_timeout).is_err());

    let mut open_with_crtc = request_bytes(Request::OpenCard {
        major: 226,
        minor: 0,
    });
    open_with_crtc[12] = 1;
    assert!(decode_request(&open_with_crtc).is_err());
}

#[test]
fn request_values_are_range_checked() {
    assert!(decode_request(&request_bytes(Request::Frame { crtc_id: 0 })).is_err());
    assert!(decode_request(&request_bytes(Request::OpenCard { major: 0, minor: 3 })).is_err());
    assert!(
        decode_request(&request_bytes(Request::Wait {
            crtc_id: 41,
            last_sequence: 0,
            timeout_ms: MAX_WAIT_TIMEOUT_MS + 1,
        }))
        .is_err()
    );
    let mut unknown = request_bytes(Request::Hello);
    unknown[6] = 99;
    let rejection = decode_request(&unknown).unwrap_err();
    assert_eq!(rejection.opcode, 0);
    assert_eq!(rejection.failure, Failure::MalformedRequest);
}

fn response(opcode: Opcode, reply: Reply) -> ResponseRecord {
    ResponseRecord {
        request_id: 5,
        opcode: opcode as u16,
        reply,
    }
}

#[test]
fn every_response_round_trips() {
    for record in [
        response(
            Opcode::Hello,
            Reply::Hello {
                capabilities: HELPER_ADMIN_PERMITTED,
            },
        ),
        response(Opcode::Hello, Reply::Hello { capabilities: 0 }),
        response(Opcode::OpenCard, Reply::CardOpened),
        response(
            Opcode::Frame,
            Reply::FrameEmpty {
                crtc_id: 41,
                sequence: 0,
                timestamp_ns: 0,
            },
        ),
        response(Opcode::Frame, Reply::FrameReady(frame())),
        response(
            Opcode::Wait,
            Reply::Presentation {
                crtc_id: 41,
                sequence: 8,
                timestamp_ns: 2,
                flags: PRESENT_CHANGED | PRESENT_PENDING,
            },
        ),
        response(
            Opcode::Frame,
            Reply::Failure {
                failure: Failure::PermissionDenied,
                os_error: 13,
            },
        ),
        ResponseRecord {
            request_id: 0,
            opcode: 0,
            reply: Reply::Failure {
                failure: Failure::MalformedRequest,
                os_error: 0,
            },
        },
    ] {
        assert_eq!(decode_response(&encode_response(&record)), Ok(record));
    }
}

#[test]
fn frame_descriptor_counts_planes_and_fences() {
    let frame = frame();
    assert_eq!(frame.descriptor_count(), 3);
    assert!(frame.has_fence(0));
    assert!(!frame.has_fence(1));
    assert!(!frame.has_fence(9));
}

#[test]
fn malformed_frame_layouts_are_rejected() {
    let cases: [fn(&mut FrameDescriptor); 8] = [
        |frame| frame.plane_count = 0,
        |frame| frame.plane_count = 5,
        |frame| frame.fence_mask = 0b100,
        |frame| frame.pitches[1] = 0,
        |frame| frame.pitches[2] = 1,
        |frame| frame.offsets[3] = 1,
        |frame| frame.sequence = 0,
        |frame| frame.width = 0,
    ];
    for mutate in cases {
        let mut candidate = frame();
        mutate(&mut candidate);
        assert!(!frame_descriptor_is_valid(&candidate));
        let bytes = encode_response(&response(Opcode::Frame, Reply::FrameReady(candidate)));
        assert_eq!(decode_response(&bytes), Err(MalformedResponse));
    }
}

#[test]
fn responses_with_stray_fields_are_rejected() {
    let mut reserved = encode_response(&response(Opcode::OpenCard, Reply::CardOpened));
    reserved[RESPONSE_LEN - 1] = 1;
    assert_eq!(decode_response(&reserved), Err(MalformedResponse));

    let mut opened_with_body = encode_response(&response(Opcode::OpenCard, Reply::CardOpened));
    opened_with_body[24] = 1;
    assert_eq!(decode_response(&opened_with_body), Err(MalformedResponse));

    let mut empty_with_width = encode_response(&response(
        Opcode::Frame,
        Reply::FrameEmpty {
            crtc_id: 41,
            sequence: 1,
            timestamp_ns: 1,
        },
    ));
    empty_with_width[48] = 1;
    assert_eq!(decode_response(&empty_with_width), Err(MalformedResponse));

    let mut failure_with_body = encode_response(&response(
        Opcode::Frame,
        Reply::Failure {
            failure: Failure::Busy,
            os_error: 16,
        },
    ));
    failure_with_body[24] = 1;
    assert_eq!(decode_response(&failure_with_body), Err(MalformedResponse));

    let mut unknown_status = encode_response(&response(Opcode::OpenCard, Reply::CardOpened));
    unknown_status[12] = 200;
    assert_eq!(decode_response(&unknown_status), Err(MalformedResponse));

    let mut unknown_hello_bit =
        encode_response(&response(Opcode::Hello, Reply::Hello { capabilities: 0 }));
    unknown_hello_bit[20] = 2;
    assert_eq!(decode_response(&unknown_hello_bit), Err(MalformedResponse));
}

#[test]
fn wait_replies_need_exactly_one_outcome() {
    for flags in [0, PRESENT_CHANGED | PRESENT_TIMEOUT, 1 << 7] {
        let bytes = encode_response(&response(
            Opcode::Wait,
            Reply::Presentation {
                crtc_id: 41,
                sequence: 1,
                timestamp_ns: 1,
                flags,
            },
        ));
        assert_eq!(decode_response(&bytes), Err(MalformedResponse));
    }
}

#[test]
fn every_failure_has_a_distinct_code() {
    let codes: std::collections::BTreeSet<_> = (1..=13)
        .map(|value| Failure::from_wire(value).unwrap().code())
        .collect();
    assert_eq!(codes.len(), 13);
    assert_eq!(Failure::from_wire(14), None);
}

#[test]
#[cfg(feature = "driver-read-leases")]
fn read_guard_count_round_trips_and_cannot_exceed_source_planes() {
    let mut layout = frame();
    layout.guard_count = layout.plane_count;
    let record = ResponseRecord {
        request_id: 1,
        opcode: Opcode::Frame as u16,
        reply: Reply::FrameReady(layout),
    };
    assert_eq!(decode_response(&encode_response(&record)).unwrap(), record);
    layout.guard_count += 1;
    let invalid = ResponseRecord {
        reply: Reply::FrameReady(layout),
        ..record
    };
    assert!(decode_response(&encode_response(&invalid)).is_err());
}
