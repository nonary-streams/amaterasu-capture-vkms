//! Local ABI bridge for the unpublished SDK v1.7/v1.8 capture tails.
//! Keep in sync with the host SDK and replace when an SDK revision is published.
#![allow(dead_code)]
use amaterasu_media::CaptureApiV1_6;
use amaterasu_module_sdk::abi::{BorrowedBytes, Status};
use std::ffi::c_void;

/// Borrowed for `request_swap` only. Empty provider asks the consumer to choose;
/// empty output leaves source matching to the consumer. Neither grants access.
/// IDs are nonzero and unique within a capture stream. Provider aliases and
/// reason codes contain at most 64 ASCII letters/digits/`_`/`-`/`.`; output IDs
/// are UTF-8, at most 512 bytes, without control characters.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CaptureSwapRequest {
    pub id: u64,
    pub preferred_provider: BorrowedBytes,
    pub output: BorrowedBytes,
    pub reason: BorrowedBytes,
}

/// Consumer-owned callbacks, valid from successful subscribe until unsubscribe
/// returns or control close completes. Callbacks may run on capture threads,
/// concurrently with the consumer. They must be bounded, contain panics, and
/// only record a notification and wake the consumer: never call a provider or
/// encode inline. They must not retain request pointers.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CaptureEventSink {
    pub context: *mut c_void,
    pub frame_ready: unsafe extern "C" fn(*mut c_void),
    /// OK records the request; BUSY records nothing, permitting an identical retry.
    pub request_swap: unsafe extern "C" fn(*mut c_void, *const CaptureSwapRequest) -> Status,
    /// Terminal capture failure, separate from an advisory swap request. Reason
    /// is a borrowed bounded identifier. No new publications follow; frames
    /// already acquired by consumers keep their normal completion lifetime.
    pub stopped: unsafe extern "C" fn(*mut c_void, BorrowedBytes),
}

/// Additive v1.7, preserving the complete v1.6 capture prefix. At most one
/// subscription per stream; ownership follows the authenticated bound caller.
/// Subscribe must announce an already available frame/pending swap, closing the
/// registration race. Notifications may coalesce: consumers acquire the latest
/// frame using the existing CPU/DMA-BUF callbacks, on the module thread.
#[repr(C)]
pub struct CaptureApiV1_7 {
    pub base: CaptureApiV1_6,
    pub subscribe_events: unsafe extern "C" fn(*mut c_void, u64, CaptureEventSink) -> Status,
    /// Lifetime barrier: stop callbacks and wait for admitted notifications to
    /// finish before returning. Idempotent even after control close. Must also
    /// run when releasing a binding. Never call it from an event callback.
    pub unsubscribe_events: unsafe extern "C" fn(*mut c_void, u64),
    /// ACCEPTED means the coordinator accepted the transition, not that another
    /// provider has already opened. Rejection leaves capture running. `reason`
    /// is a borrowed bounded reason code. Only the exact pending ID may settle.
    pub resolve_swap: unsafe extern "C" fn(*mut c_void, u64, u64, u32, BorrowedBytes) -> Status,
}

pub const CAPTURE_RESOURCE_INTERFACE_MINOR: u32 = 8;
pub const FRAME_RESOURCE_CONSUMER_INTERFACE_MAJOR: u32 = 1;
pub const FRAME_RESOURCE_CONSUMER_INTERFACE_MINOR: u32 = 0;
pub const FRAME_RESOURCE_PROCESSOR_INTERFACE_MAJOR: u32 = 1;
pub const FRAME_RESOURCE_PROCESSOR_INTERFACE_MINOR: u32 = 0;
pub const FRAME_RESOURCE_CPU: u32 = 1;
pub const FRAME_RESOURCE_DMABUF: u32 = 2;
/// Resource and synchronization kinds >= this value require an independently
/// agreed vendor schema. Unknown kinds must never be interpreted as known ones.
pub const FRAME_RESOURCE_VENDOR_BASE: u32 = 0x8000_0000;
pub const FRAME_SYNC_NONE: u32 = 0;
/// Linux sync_file: descriptor v1 points to `FrameSyncFile`.
pub const FRAME_SYNC_SYNC_FILE: u32 = 1;
pub const FRAME_SYNC_VENDOR_BASE: u32 = 0x8000_0000;

/// A typed, versioned descriptor. The schema is determined by kind and version;
/// size is in bytes. Pointers and any handles within are borrowed. NONE has
/// version=0, size=0, namespace all zero and data=null. Unknown versions must be rejected.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FrameDescriptor {
    pub kind: u32,
    pub version: u32,
    pub size: u32,
    pub reserved: u32,
    /// All zero for standard kinds; a stable nonzero UUID for vendor kinds.
    pub namespace: [u8; 16],
    pub data: *const c_void,
}

/// Borrowed Linux sync_file fd. A completion fence covers all work performed
/// under the released reference. The provider duplicates it during release.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FrameSyncFile {
    pub fd: i32,
}

/// CPU v1 offer payload. A separate offer advertises each format.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CpuResourceCapabilities {
    pub format: u32,
    pub min_width: u32,
    pub max_width: u32,
    pub min_height: u32,
    pub max_height: u32,
}

/// DMA-BUF v1 offer payload: one format/modifier/color tuple and size range.
/// `device_id` is an agreed 16-byte device UUID; zero means unknown, not a
/// wildcard proving interoperability. Consumer configure must check importability.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DmaBufResourceCapabilities {
    pub fourcc: u32,
    pub color: u32,
    pub modifier: u64,
    pub min_width: u32,
    pub max_width: u32,
    pub min_height: u32,
    pub max_height: u32,
    pub device_id: [u8; 16],
}

pub const FRAME_OWNERSHIP_BORROWED: u32 = 1;
pub const FRAME_OWNERSHIP_RETAINABLE: u32 = 2;
/// Resource schema exposes a direct CPU access mechanism to the consumer.
pub const FRAME_ACCESS_CPU: u32 = 1;
/// Resource schema exposes a direct GPU access/import mechanism to the consumer.
pub const FRAME_ACCESS_GPU: u32 = 2;

/// One provider offer. IDs are nonzero, unique within a generation and stream.
/// Enumeration reports a consistent nonzero generation, including an empty
/// result; select uses the exact generation and ID. `resource` and `acquire_sync`
/// describe the offered schemas. CPU and DMA-BUF resource offer payloads are
/// `CpuResourceCapabilities` and `DmaBufResourceCapabilities`, respectively;
/// these differ from frame payloads. Sync offer payloads are schema-specific.
/// Descriptors and pointed-to data are borrowed only during the sink callback.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CaptureResourceOffer {
    pub id: u64,
    /// BORROWED or RETAINABLE. Borrowed-only retain returns UNSUPPORTED.
    pub ownership: u32,
    /// Bitset of FRAME_ACCESS_*. Zero means unspecified, not inaccessible.
    /// These describe access mechanisms in the resource schema, not physical
    /// residency, zero-copy, or absence of staging. Actual access remains subject
    /// to the schema's device, format, synchronization and permission constraints.
    pub accessibility: u32,
    pub reserved: u32,
    pub resource: FrameDescriptor,
    pub acquire_sync: FrameDescriptor,
    pub completion_sync: FrameDescriptor,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CaptureResourceOfferSink {
    pub context: *mut c_void,
    pub write: unsafe extern "C" fn(*mut c_void, *const CaptureResourceOffer) -> Status,
}

/// Unknown values must remain unknown; consumers must not infer SDR or HDR.
pub const FRAME_COLOR_UNKNOWN: u32 = 0;
pub const FRAME_PRIMARIES_BT709: u32 = 1;
pub const FRAME_PRIMARIES_BT2020: u32 = 2;
pub const FRAME_TRANSFER_SRGB: u32 = 1;
pub const FRAME_TRANSFER_BT709: u32 = 2;
pub const FRAME_TRANSFER_PQ: u32 = 3;
pub const FRAME_TRANSFER_HLG: u32 = 4;
pub const FRAME_MATRIX_RGB: u32 = 1;
pub const FRAME_MATRIX_BT709: u32 = 2;
pub const FRAME_MATRIX_BT2020_NCL: u32 = 3;
pub const FRAME_RANGE_FULL: u32 = 1;
pub const FRAME_RANGE_LIMITED: u32 = 2;
pub const FRAME_CLOCK_UNKNOWN: u32 = 0;
pub const FRAME_CLOCK_STREAM_RELATIVE: u32 = 1;
pub const FRAME_CLOCK_HOST_MONOTONIC: u32 = 2;

/// Optional versioned color information. All zero means absent. Version 1 has
/// size equal to this struct; each zero component is independently unknown.
/// Unknown or unrecognized values must not be interpreted as a known color.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FrameColorMetadata {
    pub version: u32,
    pub size: u32,
    pub primaries: u32,
    pub transfer: u32,
    pub matrix: u32,
    pub range: u32,
}

/// Optional timestamp clock identity. All zero means absent. In v1, timestamps
/// are microseconds in the associated clock. A nonzero
/// epoch_id is an agreed unique identity for its epoch; zero means unknown.
/// Compare timestamps only for understood matching domains and matching nonzero
/// epoch IDs. Unknown versions or domains do not authorize cross-frame comparison.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FrameClockMetadata {
    pub version: u32,
    pub size: u32,
    pub domain: u32,
    pub reserved: u32,
    pub epoch_id: [u8; 16],
}

/// A single immutable presentation. `resource` v1 is `CpuFrame` for CPU and
/// `DmaBufFrame` for DMA-BUF; size must be at least the corresponding struct.
/// CPU uses SYNC_NONE. DMA-BUF v1 uses SYNC_NONE here and its plane acquire
/// fences; consumers must wait for all those fences. Schema-specific resources
/// specify the interpretation of acquire and completion synchronization.
///
/// The frame and descriptors are borrowed for the synchronous sink callback.
/// Borrowed-only frames return UNSUPPORTED from retain. A successful retain adds one reference and keeps storage, immutable contents,
/// descriptor data and handles valid until its matching release. Copy the frame
/// and descriptor values before returning. Failed retain acquires nothing; the
/// provider limits retained references and may return BUSY. The binding and
/// provider remain live until all retained references and their GPU work retire.
/// Selection changes or stream close do not invalidate retained references;
/// shutdown must defer provider destruction until their completion fences retire.
///
/// Each release consumes one retained reference. NONE asserts all consumer work
/// has completed. Otherwise completion describes pending work in the selected
/// synchronization schema. Its descriptor and handles are borrowed only until
/// release returns: the provider must synchronously duplicate or retain anything
/// it needs. On failure it must quarantine storage, never reuse it early. A caller
/// must not release until it has completed all accesses or supplied a valid
/// completion object covering them. No callbacks may unwind across the ABI.
/// Retain and release are invoked on the module thread; no concurrent-use promise.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CaptureResourceFrame {
    pub offer_generation: u64,
    pub offer_id: u64,
    pub timestamp_us: u64,
    pub capture_pacing_timestamp_us: u64,
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    pub freshness: u32,
    pub reserved: u32,
    pub color: FrameColorMetadata,
    /// Clock for timestamp_us.
    pub clock: FrameClockMetadata,
    /// Independent clock for capture_pacing_timestamp_us.
    pub pacing_clock: FrameClockMetadata,
    pub resource: FrameDescriptor,
    pub acquire_sync: FrameDescriptor,
    pub context: *mut c_void,
    pub retain: unsafe extern "C" fn(*mut c_void) -> Status,
    pub release: unsafe extern "C" fn(*mut c_void, *const FrameDescriptor),
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CaptureResourceFrameSink {
    pub context: *mut c_void,
    pub write: unsafe extern "C" fn(*mut c_void, *const CaptureResourceFrame) -> Status,
}

/// Additive v1.8 capture table. Enumeration invokes the sink synchronously and
/// writes the generation even for an empty list. Errors propagate and output is
/// invalid on error. Selecting an unavailable/stale offer returns UNSUPPORTED
/// and leaves the previous selection unchanged. Successful selection replaces
/// it atomically. A source capability change invalidates the selection; read
/// returns UNSUPPORTED until re-enumerated and selected. Reads synchronously
/// deliver at most one frame; PENDING delivers none. A sink error propagates.
/// These operations do not change or remove the legacy frame APIs. Access uses
/// the authenticated stream owner and binding, just like legacy capture. The
/// provider owns the initial frame through the entire sink callback; a sink
/// return never implicitly transfers a reference.
#[repr(C)]
pub struct CaptureApiV1_8 {
    pub base: CaptureApiV1_7,
    pub enumerate_resources:
        unsafe extern "C" fn(*mut c_void, u64, CaptureResourceOfferSink, *mut u64) -> Status,
    pub select_resource: unsafe extern "C" fn(*mut c_void, u64, u64, u64) -> Status,
    pub read_resource_frame:
        unsafe extern "C" fn(*mut c_void, u64, CaptureResourceFrameSink) -> Status,
}

pub const CAPTURE_API_V1_8_SIZE: u32 = std::mem::size_of::<CaptureApiV1_8>() as u32;

#[cfg(all(test, target_pointer_width = "64"))]
mod tests {
    use super::*;
    #[test]
    fn bridge_matches_sdk_c_layout() {
        assert_eq!(std::mem::size_of::<FrameDescriptor>(), 40);
        assert_eq!(std::mem::size_of::<CaptureResourceOffer>(), 144);
        assert_eq!(std::mem::size_of::<CaptureResourceFrame>(), 248);
        assert_eq!(std::mem::size_of::<FrameColorMetadata>(), 24);
        assert_eq!(std::mem::size_of::<FrameClockMetadata>(), 32);
        assert_eq!(std::mem::size_of::<CaptureEventSink>(), 32);
        assert_eq!(std::mem::size_of::<CaptureApiV1_7>(), 112);
        assert_eq!(
            std::mem::offset_of!(CaptureApiV1_8, enumerate_resources),
            112
        );
        assert_eq!(std::mem::size_of::<CaptureApiV1_8>(), 136);
    }

    #[test]
    fn bridge_fields_match_host_sdk_header_when_available() {
        use std::{fmt::Write, path::PathBuf, process::Command};
        let include = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../sdk/include");
        if !include.join("amaterasu_media.h").is_file() {
            // The independently distributed provider does not carry the host SDK.
            eprintln!("host SDK header unavailable; field-by-field comparison skipped");
            return;
        }
        let mut checks: Vec<(&str, &str, usize)> = Vec::new();
        macro_rules! fields {
            ($ty:ident, $c:literal, $($field:ident),+ $(,)?) => {{
                checks.push(($c, "", std::mem::size_of::<$ty>()));
                $(checks.push(($c, stringify!($field), std::mem::offset_of!($ty, $field)));)+
            }};
        }
        fields!(
            CaptureSwapRequest,
            "AmaterasuCaptureSwapRequest",
            id,
            preferred_provider,
            output,
            reason
        );
        fields!(
            CaptureEventSink,
            "AmaterasuCaptureEventSink",
            context,
            frame_ready,
            request_swap,
            stopped
        );
        fields!(
            CaptureApiV1_7,
            "AmaterasuCaptureApiV1_7",
            base,
            subscribe_events,
            unsubscribe_events,
            resolve_swap
        );
        fields!(
            FrameDescriptor,
            "AmaterasuFrameDescriptor",
            kind,
            version,
            size,
            reserved,
            data
        );
        checks.push((
            "AmaterasuFrameDescriptor",
            "namespace_id",
            std::mem::offset_of!(FrameDescriptor, namespace),
        ));
        fields!(FrameSyncFile, "AmaterasuFrameSyncFile", fd);
        fields!(
            CpuResourceCapabilities,
            "AmaterasuCpuResourceCapabilities",
            format,
            min_width,
            max_width,
            min_height,
            max_height
        );
        fields!(
            DmaBufResourceCapabilities,
            "AmaterasuDmaBufResourceCapabilities",
            fourcc,
            color,
            modifier,
            min_width,
            max_width,
            min_height,
            max_height,
            device_id
        );
        fields!(
            CaptureResourceOffer,
            "AmaterasuCaptureResourceOffer",
            id,
            ownership,
            accessibility,
            reserved,
            resource,
            acquire_sync,
            completion_sync
        );
        fields!(
            CaptureResourceOfferSink,
            "AmaterasuCaptureResourceOfferSink",
            context,
            write
        );
        fields!(
            FrameColorMetadata,
            "AmaterasuFrameColorMetadata",
            version,
            size,
            primaries,
            transfer,
            matrix,
            range
        );
        fields!(
            FrameClockMetadata,
            "AmaterasuFrameClockMetadata",
            version,
            size,
            domain,
            reserved,
            epoch_id
        );
        fields!(
            CaptureResourceFrame,
            "AmaterasuCaptureResourceFrame",
            offer_generation,
            offer_id,
            timestamp_us,
            capture_pacing_timestamp_us,
            sequence,
            width,
            height,
            freshness,
            reserved,
            color,
            clock,
            pacing_clock,
            resource,
            acquire_sync,
            context,
            retain,
            release
        );
        fields!(
            CaptureResourceFrameSink,
            "AmaterasuCaptureResourceFrameSink",
            context,
            write
        );
        fields!(
            CaptureApiV1_8,
            "AmaterasuCaptureApiV1_8",
            base,
            enumerate_resources,
            select_resource,
            read_resource_frame
        );

        let mut source = String::from(
            "#include <stddef.h>\n#include <stdio.h>\n#include \"amaterasu_media.h\"\nint main(void) {\n",
        );
        for (ty, field, _) in &checks {
            if field.is_empty() {
                writeln!(source, "printf(\"%zu\\n\", sizeof({ty}));").unwrap();
            } else {
                writeln!(source, "printf(\"%zu\\n\", offsetof({ty}, {field}));").unwrap();
            }
        }
        source.push_str("return 0; }\n");
        let temp =
            std::env::temp_dir().join(format!("amaterasu-vkms-sdk-layout-{}", std::process::id()));
        std::fs::create_dir_all(&temp).unwrap();
        let source_path = temp.join("layout.c");
        let binary = temp.join("layout");
        std::fs::write(&source_path, source).unwrap();
        let compiler = Command::new("cc")
            .arg("-std=c11")
            .arg("-I")
            .arg(include)
            .arg(&source_path)
            .arg("-o")
            .arg(&binary)
            .output()
            .unwrap();
        assert!(
            compiler.status.success(),
            "C layout compilation failed: {}",
            String::from_utf8_lossy(&compiler.stderr)
        );
        let result = Command::new(&binary).output().unwrap();
        assert!(result.status.success());
        let values = String::from_utf8(result.stdout).unwrap();
        let actual: Vec<usize> = values.lines().map(|line| line.parse().unwrap()).collect();
        assert_eq!(actual.len(), checks.len());
        for ((ty, field, rust), c) in checks.iter().zip(actual) {
            assert_eq!(*rust, c, "layout mismatch in {ty}.{field}");
        }
        std::fs::remove_dir_all(temp).unwrap();
    }
}

/// HDR10 static metadata of the current captured presentation. Fields are RGB
/// primaries xy, white xy (1/50000), max nits, min 1/10000 nit, MaxCLL, MaxFALL.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HdrMetadata {
    pub enabled: u32,
    pub fields: [u16; 12],
    pub reserved: u32,
}

#[repr(C)]
pub struct CaptureApiV1_9 {
    pub base: CaptureApiV1_7,
    pub enumerate_resources:
        unsafe extern "C" fn(*mut c_void, u64, CaptureResourceOfferSink, *mut u64) -> Status,
    pub select_resource: unsafe extern "C" fn(*mut c_void, u64, u64, u64) -> Status,
    pub read_resource_frame:
        unsafe extern "C" fn(*mut c_void, u64, CaptureResourceFrameSink) -> Status,
    pub hdr_metadata: unsafe extern "C" fn(*mut c_void, u64, *mut HdrMetadata) -> Status,
}
