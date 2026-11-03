//! Public capture ABI supplied by the selected SDK.
#![allow(unused_imports)]
pub use amaterasu_media::*;

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
        let include = std::env::var_os("AMATERASU_SDK_INCLUDE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../sdk/include")
            });
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

        fields!(
            CaptureApiV1_10,
            "AmaterasuCaptureApiV1_10",
            base,
            color_provenance
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
