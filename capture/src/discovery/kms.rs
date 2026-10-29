//! Read-only KMS mode-object queries on a vibeshine_drm primary node.
//!
//! The node is opened read-only, never kept as DRM master, and closed after
//! each query. GETCONNECTOR is issued with no mode buffer; because the
//! descriptor is not master, the kernel answers from its cached state
//! instead of forcing a connector probe.
use super::{
    CardQueryError, Connection, ConnectorState, CrtcState, DisplayMode, DisplayProbe, SysfsCard,
    sysfs,
};
use amaterasu_vkms_frame_helper::drm_node::{
    self, Access, IOC_READ, IOC_WRITE, NodeError, drm_ioctl, ioctl_record,
};
use std::{mem::size_of, os::fd::OwnedFd, path::PathBuf};

const DRM_MODE_CONNECTOR_VIRTUAL: u32 = 15;
const DRM_MODE_CONNECTED: u32 = 1;
const DRM_MODE_DISCONNECTED: u32 = 2;
/// Resource lists can change between the counting and filling calls.
const RESOURCE_ATTEMPTS: usize = 3;
/// More connectors than this on one card is treated as a malformed answer.
const MAX_CONNECTORS: u32 = 256;

/// `struct drm_mode_card_res`.
#[repr(C)]
#[derive(Default)]
struct CardResources {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

/// `struct drm_mode_get_connector`.
#[repr(C)]
#[derive(Default)]
struct GetConnector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    encoder_id: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    pad: u32,
}

/// `struct drm_mode_get_encoder`.
#[repr(C)]
#[derive(Default)]
struct GetEncoder {
    encoder_id: u32,
    encoder_type: u32,
    crtc_id: u32,
    possible_crtcs: u32,
    possible_clones: u32,
}

/// `struct drm_mode_modeinfo`.
#[repr(C)]
#[derive(Default)]
struct ModeInfo {
    clock: u32,
    hdisplay: u16,
    hsync_start: u16,
    hsync_end: u16,
    htotal: u16,
    hskew: u16,
    vdisplay: u16,
    vsync_start: u16,
    vsync_end: u16,
    vtotal: u16,
    vscan: u16,
    vrefresh: u32,
    flags: u32,
    mode_type: u32,
    name: [u8; 32],
}

/// `struct drm_mode_crtc`.
#[repr(C)]
#[derive(Default)]
struct GetCrtc {
    set_connectors_ptr: u64,
    count_connectors: u32,
    crtc_id: u32,
    fb_id: u32,
    x: u32,
    y: u32,
    gamma_size: u32,
    mode_valid: u32,
    mode: ModeInfo,
}

const _: () = assert!(size_of::<CardResources>() == 64);
const _: () = assert!(size_of::<GetConnector>() == 80);
const _: () = assert!(size_of::<GetEncoder>() == 20);
const _: () = assert!(size_of::<ModeInfo>() == 68);
const _: () = assert!(size_of::<GetCrtc>() == 104);

const READ_WRITE: u32 = IOC_READ | IOC_WRITE;
const DRM_IOCTL_MODE_GETRESOURCES: u32 = drm_ioctl(READ_WRITE, 0xA0, size_of::<CardResources>());
const DRM_IOCTL_MODE_GETCRTC: u32 = drm_ioctl(READ_WRITE, 0xA1, size_of::<GetCrtc>());
const DRM_IOCTL_MODE_GETENCODER: u32 = drm_ioctl(READ_WRITE, 0xA6, size_of::<GetEncoder>());
const DRM_IOCTL_MODE_GETCONNECTOR: u32 = drm_ioctl(READ_WRITE, 0xA7, size_of::<GetConnector>());

/// The production probe: `/sys/class/drm` plus read-only KMS queries.
pub struct KmsProbe {
    sysfs_root: PathBuf,
}

impl KmsProbe {
    pub fn new() -> Self {
        Self {
            sysfs_root: PathBuf::from(sysfs::DRM_SYSFS_ROOT),
        }
    }
}

impl DisplayProbe for KmsProbe {
    fn sysfs_cards(&self) -> Result<Vec<SysfsCard>, String> {
        sysfs::scan(&self.sysfs_root)
    }

    fn query_card(&self, card: &SysfsCard) -> Result<Vec<ConnectorState>, CardQueryError> {
        let device =
            drm_node::open_vibeshine_node(card.device, Access::ReadOnly).map_err(|error| {
                match error {
                    NodeError::WrongDriver => CardQueryError::WrongDriver,
                    NodeError::NotThisDevice => {
                        CardQueryError::Unavailable("device number does not match its node".into())
                    }
                    NodeError::Unavailable(error) => CardQueryError::Unavailable(error.to_string()),
                }
            })?;
        virtual_connectors(&device).map_err(|error| CardQueryError::Unavailable(error.to_string()))
    }
}

fn virtual_connectors(device: &OwnedFd) -> std::io::Result<Vec<ConnectorState>> {
    let mut states = Vec::new();
    for connector_id in connector_ids(device)? {
        let connector = get_connector(device, connector_id)?;
        if connector.connector_type != DRM_MODE_CONNECTOR_VIRTUAL {
            continue;
        }
        let crtc = match connector.encoder_id {
            0 => None,
            encoder_id => current_crtc(device, encoder_id)?,
        };
        states.push(ConnectorState {
            connector_id,
            connector_index: connector.connector_type_id,
            connection: match connector.connection {
                DRM_MODE_CONNECTED => Connection::Connected,
                DRM_MODE_DISCONNECTED => Connection::Disconnected,
                _ => Connection::Unknown,
            },
            crtc,
        });
    }
    Ok(states)
}

fn connector_ids(device: &OwnedFd) -> std::io::Result<Vec<u32>> {
    for _ in 0..RESOURCE_ATTEMPTS {
        let mut counts = CardResources::default();
        // SAFETY: GETRESOURCES takes one drm_mode_card_res; with all list
        // pointers null and counts zero the kernel only writes the counts.
        unsafe { ioctl_record(device, DRM_IOCTL_MODE_GETRESOURCES, &mut counts) }?;
        if counts.count_connectors > MAX_CONNECTORS {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
        }
        let mut ids = vec![0u32; counts.count_connectors as usize];
        let mut fill = CardResources {
            connector_id_ptr: ids.as_mut_ptr() as u64,
            count_connectors: counts.count_connectors,
            ..CardResources::default()
        };
        // SAFETY: the connector list points at `ids`, which holds exactly
        // `count_connectors` u32 slots and outlives the call; the other lists
        // have zero counts and are not written.
        unsafe { ioctl_record(device, DRM_IOCTL_MODE_GETRESOURCES, &mut fill) }?;
        if fill.count_connectors == counts.count_connectors {
            return Ok(ids);
        }
    }
    Err(std::io::Error::from(std::io::ErrorKind::Interrupted))
}

fn get_connector(device: &OwnedFd, connector_id: u32) -> std::io::Result<GetConnector> {
    let mut connector = GetConnector {
        connector_id,
        ..GetConnector::default()
    };
    // SAFETY: GETCONNECTOR takes one drm_mode_get_connector; every list
    // count is zero, so the kernel writes only this record.
    unsafe { ioctl_record(device, DRM_IOCTL_MODE_GETCONNECTOR, &mut connector) }?;
    Ok(connector)
}

fn current_crtc(device: &OwnedFd, encoder_id: u32) -> std::io::Result<Option<CrtcState>> {
    let mut encoder = GetEncoder {
        encoder_id,
        ..GetEncoder::default()
    };
    // SAFETY: GETENCODER takes one drm_mode_get_encoder that lives through the call.
    unsafe { ioctl_record(device, DRM_IOCTL_MODE_GETENCODER, &mut encoder) }?;
    if encoder.crtc_id == 0 {
        return Ok(None);
    }
    let mut crtc = GetCrtc {
        crtc_id: encoder.crtc_id,
        ..GetCrtc::default()
    };
    // SAFETY: GETCRTC takes one drm_mode_crtc; with a zero connector count
    // the kernel only writes this record.
    unsafe { ioctl_record(device, DRM_IOCTL_MODE_GETCRTC, &mut crtc) }?;
    Ok(Some(crtc_state(&crtc)))
}

fn crtc_state(crtc: &GetCrtc) -> CrtcState {
    let mode = (crtc.mode_valid != 0 && crtc.mode.hdisplay != 0 && crtc.mode.vdisplay != 0)
        .then_some(DisplayMode {
            width: crtc.mode.hdisplay.into(),
            height: crtc.mode.vdisplay.into(),
            refresh_hz: crtc.mode.vrefresh,
        });
    CrtcState {
        crtc_id: crtc.crtc_id,
        mode,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn mode_ioctl_numbers_match_the_kernel_headers() {
        assert_eq!(DRM_IOCTL_MODE_GETRESOURCES, 0xC040_64A0);
        assert_eq!(DRM_IOCTL_MODE_GETCRTC, 0xC068_64A1);
        assert_eq!(DRM_IOCTL_MODE_GETENCODER, 0xC014_64A6);
        assert_eq!(DRM_IOCTL_MODE_GETCONNECTOR, 0xC050_64A7);
    }

    #[test]
    fn a_crtc_without_a_valid_mode_is_inactive() {
        let mut crtc = GetCrtc {
            crtc_id: 41,
            mode_valid: 1,
            ..GetCrtc::default()
        };
        crtc.mode.hdisplay = 2560;
        crtc.mode.vdisplay = 1440;
        crtc.mode.vrefresh = 120;
        assert_eq!(
            crtc_state(&crtc).mode,
            Some(DisplayMode {
                width: 2560,
                height: 1440,
                refresh_hz: 120
            })
        );
        crtc.mode_valid = 0;
        assert_eq!(crtc_state(&crtc).mode, None);
    }
}
