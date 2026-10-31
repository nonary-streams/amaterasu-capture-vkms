//! The private vibeshine_drm presentation and frame ioctls on a verified node.
//!
//! The helper never changes display state. Opening goes through
//! `drm_node`, which never keeps DRM master.
use crate::service::{
    CardOpener, DRIVER_ABI_VERSION, DRIVER_FRAME_ABI_VERSION, DriverFrame, DriverPresentation,
    OpenFailure, PresentationCard,
};
use amaterasu_vkms_frame_helper::{
    drm_node::{
        self, Access, DeviceNumber, IOC_READ, IOC_WRITE, NodeError, drm_ioctl, ioctl_record,
    },
    protocol::{Failure, MAX_PLANES},
};
use std::{
    io,
    mem::size_of,
    os::fd::{FromRawFd, OwnedFd},
};

const DRM_COMMAND_BASE: u32 = 0x40;

/// `struct vibeshine_drm_wait_present` from vibeshine_drm_uapi.h.
#[repr(C)]
#[derive(Default)]
pub struct WaitPresentRecord {
    pub abi_version: u32,
    pub crtc_id: u32,
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub timeout_ms: u32,
    pub flags: u32,
    pub reserved: [u64; 2],
}

/// `struct vibeshine_drm_frame` shared by the ordinary and leased export ioctls.
#[repr(C)]
#[derive(Default)]
pub struct FrameRecord {
    pub abi_version: u32,
    pub crtc_id: u32,
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub flags: u32,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub plane_count: u32,
    pub dma_buf_fds: [i32; MAX_PLANES],
    pub pitches: [u32; MAX_PLANES],
    pub offsets: [u32; MAX_PLANES],
    pub sync_file_fds: [i32; MAX_PLANES],
    pub reserved_u32: u32,
    pub reserved: [u64; 4],
}

const _: () = assert!(size_of::<WaitPresentRecord>() == 48);
const _: () = assert!(size_of::<FrameRecord>() == 152);

pub const VIBESHINE_IOCTL_WAIT_PRESENT: u32 = drm_ioctl(
    IOC_READ | IOC_WRITE,
    DRM_COMMAND_BASE,
    size_of::<WaitPresentRecord>(),
);
#[cfg(any(not(feature = "driver-read-leases"), test))]
pub const VIBESHINE_IOCTL_GET_FRAME: u32 = drm_ioctl(
    IOC_READ | IOC_WRITE,
    DRM_COMMAND_BASE + 1,
    size_of::<FrameRecord>(),
);
#[cfg(feature = "driver-read-leases")]
pub const VIBESHINE_IOCTL_GET_FRAME_LEASED: u32 = drm_ioctl(
    IOC_READ | IOC_WRITE,
    DRM_COMMAND_BASE + 4,
    size_of::<FrameRecord>(),
);

/// Opens `/dev/dri/card<minor>` when it is that exact vibeshine_drm node.
pub struct DeviceOpener;

impl CardOpener for DeviceOpener {
    type Card = VibeshineCard;

    fn open(&self, major: u32, minor: u32) -> Result<VibeshineCard, OpenFailure> {
        let number = DeviceNumber { major, minor };
        match drm_node::open_vibeshine_node(number, Access::ReadWrite) {
            Ok(device) => Ok(VibeshineCard { device }),
            Err(error) => Err(open_failure(&error)),
        }
    }
}

fn open_failure(error: &NodeError) -> OpenFailure {
    let (failure, os_error) = match error {
        NodeError::NotThisDevice => (Failure::DeviceRejected, 0),
        NodeError::WrongDriver => (Failure::WrongDriver, 0),
        NodeError::Unavailable(error) => (
            Failure::DeviceUnavailable,
            error.raw_os_error().unwrap_or(0),
        ),
    };
    OpenFailure { failure, os_error }
}

pub struct VibeshineCard {
    device: OwnedFd,
}

impl PresentationCard for VibeshineCard {
    fn export_frame(&self, crtc_id: u32) -> io::Result<DriverFrame> {
        // The driver requires every field except the version and CRTC to be zero.
        let mut record = FrameRecord {
            abi_version: DRIVER_FRAME_ABI_VERSION,
            crtc_id,
            ..FrameRecord::default()
        };
        // SAFETY: GET_FRAME takes one vibeshine_drm_frame that lives through the call.
        #[cfg(feature = "driver-read-leases")]
        let request = VIBESHINE_IOCTL_GET_FRAME_LEASED;
        #[cfg(not(feature = "driver-read-leases"))]
        let request = VIBESHINE_IOCTL_GET_FRAME;
        unsafe { ioctl_record(&self.device, request, &mut record) }?;
        Ok(take_frame(record))
    }

    fn wait_present(
        &self,
        crtc_id: u32,
        last_sequence: u64,
        timeout_ms: u32,
    ) -> io::Result<DriverPresentation> {
        let mut record = WaitPresentRecord {
            abi_version: DRIVER_ABI_VERSION,
            crtc_id,
            sequence: last_sequence,
            timeout_ms,
            ..WaitPresentRecord::default()
        };
        // SAFETY: WAIT_PRESENT takes one vibeshine_drm_wait_present that lives
        // through the call.
        unsafe { ioctl_record(&self.device, VIBESHINE_IOCTL_WAIT_PRESENT, &mut record) }?;
        Ok(DriverPresentation {
            abi_version: record.abi_version,
            crtc_id: record.crtc_id,
            sequence: record.sequence,
            timestamp_ns: record.timestamp_ns,
            timeout_ms: record.timeout_ms,
            flags: record.flags,
            reserved_zero: record.reserved == [0; 2],
        })
    }
}

/// Takes ownership of every descriptor a successful GET_FRAME installed.
/// Only called after success: on failure the driver already closed them.
fn take_frame(record: FrameRecord) -> DriverFrame {
    let own = |raw: i32| {
        // SAFETY: the driver installed each nonnegative descriptor in this
        // process for the caller to own and close.
        (raw >= 0).then(|| unsafe { OwnedFd::from_raw_fd(raw) })
    };
    DriverFrame {
        abi_version: record.abi_version,
        crtc_id: record.crtc_id,
        sequence: record.sequence,
        timestamp_ns: record.timestamp_ns,
        flags: record.flags,
        width: record.width,
        height: record.height,
        fourcc: record.fourcc,
        modifier: record.modifier,
        plane_count: record.plane_count,
        dma_bufs: record.dma_buf_fds.map(own),
        pitches: record.pitches,
        offsets: record.offsets,
        sync_files: record.sync_file_fds.map(own),
        reserved_zero: if cfg!(feature = "driver-read-leases") {
            record.reserved_u32 <= 2 && (record.reserved_u32 == 2 || record.reserved == [0; 4])
        } else {
            record.reserved_u32 == 0 && record.reserved == [0; 4]
        },
        color: if cfg!(feature = "driver-read-leases") {
            record.reserved_u32
        } else {
            1
        },
        hdr_metadata: if cfg!(feature = "driver-read-leases") && record.reserved_u32 == 2 {
            // Linux hdr_output_metadata is a u32 type followed by the HDMI
            // infoframe (EOTF/type bytes, twelve u16 fields and padding).
            let mut bytes = [0u8; 32];
            for (word, value) in record.reserved.into_iter().enumerate() {
                bytes[word * 8..word * 8 + 8].copy_from_slice(&value.to_ne_bytes());
            }
            std::array::from_fn(|index| {
                u16::from_ne_bytes([bytes[6 + index * 2], bytes[7 + index * 2]])
            })
        } else {
            [0; 12]
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn private_ioctl_numbers_match_the_driver_header() {
        assert_eq!(VIBESHINE_IOCTL_WAIT_PRESENT, 0xC030_6440);
        assert_eq!(VIBESHINE_IOCTL_GET_FRAME, 0xC098_6441);
        #[cfg(feature = "driver-read-leases")]
        assert_eq!(VIBESHINE_IOCTL_GET_FRAME_LEASED, 0xC098_6444);
    }

    #[test]
    fn open_errors_map_to_protocol_failures() {
        assert_eq!(
            DeviceOpener.open(8, 0).err().map(|failure| failure.failure),
            Some(Failure::DeviceRejected)
        );
        assert_eq!(
            open_failure(&NodeError::WrongDriver).failure,
            Failure::WrongDriver
        );
        let unavailable = open_failure(&NodeError::Unavailable(io::Error::from_raw_os_error(
            libc::EACCES,
        )));
        assert_eq!(unavailable.failure, Failure::DeviceUnavailable);
        assert_eq!(unavailable.os_error, libc::EACCES);
    }

    #[test]
    fn negative_driver_descriptors_are_not_owned() {
        let record = FrameRecord {
            dma_buf_fds: [-1; MAX_PLANES],
            sync_file_fds: [-1; MAX_PLANES],
            ..FrameRecord::default()
        };
        let frame = take_frame(record);
        assert!(frame.dma_bufs.iter().all(Option::is_none));
        assert!(frame.sync_files.iter().all(Option::is_none));
        assert!(frame.reserved_zero);
    }

    #[test]
    #[ignore = "requires vibeshine_drm and CAP_SYS_ADMIN"]
    fn exports_a_frame_from_real_hardware() {
        let card = DeviceOpener
            .open(drm_node::DRM_MAJOR, 0)
            .expect("vibeshine_drm card0");
        let _ = card.export_frame(1);
    }
}
