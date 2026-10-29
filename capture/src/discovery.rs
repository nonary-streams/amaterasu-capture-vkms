//! Finding a `kwin:Virtual-N` output on a vibeshine_drm card without
//! privilege: sysfs names the candidate cards and connectors, and read-only
//! KMS queries report each connector's current encoder, CRTC and mode.
#[cfg(target_os = "linux")]
pub mod kms;
pub mod sysfs;

use crate::geometry::{self, LayoutOutput};
use crate::{
    model::{CaptureError, CaptureResult},
    output_name::OutputName,
};
use amaterasu_vkms_frame_helper::drm_node::DeviceNumber;
use serde_json::{Value, json};

/// A DRM primary node with at least one `Virtual-N` connector in sysfs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SysfsCard {
    /// `cardN`.
    pub name: String,
    pub device: DeviceNumber,
    /// Connector type indexes named `cardN-Virtual-<index>`.
    pub virtual_connectors: Vec<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Connection {
    Connected,
    Disconnected,
    Unknown,
}

impl Connection {
    pub fn name(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Disconnected => "disconnected",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CrtcState {
    pub crtc_id: u32,
    /// `None` when the CRTC has no valid mode (it is off).
    pub mode: Option<DisplayMode>,
}

/// One virtual connector as KMS currently reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectorState {
    pub connector_id: u32,
    pub connector_index: u32,
    pub connection: Connection,
    /// `None` when the connector has no encoder or its encoder has no CRTC.
    pub crtc: Option<CrtcState>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CardQueryError {
    /// The node is not driven by vibeshine_drm; it is skipped.
    WrongDriver,
    Unavailable(String),
}

/// Unprivileged display inspection. The production probe reads sysfs and
/// issues read-only KMS queries; tests substitute a fake.
pub trait DisplayProbe: Send {
    fn sysfs_cards(&self) -> Result<Vec<SysfsCard>, String>;
    fn query_card(&self, card: &SysfsCard) -> Result<Vec<ConnectorState>, CardQueryError>;
}

/// Everything needed to capture one output: its card, connector, the CRTC
/// that currently scans it out, and that CRTC's mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputBinding {
    pub output: OutputName,
    pub card: String,
    pub device: DeviceNumber,
    pub connector_id: u32,
    pub crtc_id: u32,
    pub mode: DisplayMode,
}

/// One virtual output found during enumeration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredOutput {
    pub output: OutputName,
    pub card: String,
    pub connector: ConnectorState,
}

impl DiscoveredOutput {
    fn binding(&self, device: DeviceNumber) -> CaptureResult<OutputBinding> {
        if self.connector.connection != Connection::Connected {
            return Err(CaptureError::new(
                "output_disconnected",
                format!("{} is not connected", self.output),
            ));
        }
        let Some(CrtcState {
            crtc_id,
            mode: Some(mode),
        }) = self.connector.crtc
        else {
            return Err(CaptureError::new(
                "output_inactive",
                format!("{} has no active CRTC", self.output),
            ));
        };
        Ok(OutputBinding {
            output: self.output,
            card: self.card.clone(),
            device,
            connector_id: self.connector.connector_id,
            crtc_id,
            mode,
        })
    }

    /// Enumeration entry. Desktop geometry is included only when the
    /// compositor layout is known and agrees with the scanout mode.
    pub fn metadata(&self, device: DeviceNumber, layout: Option<&[LayoutOutput]>) -> Value {
        let binding = self.binding(device).ok();
        let desktop_geometry = binding.as_ref().and_then(|binding| {
            geometry::output_geometry(layout?, &self.output.connector_name(), binding.mode).ok()
        });
        let mut entry = json!({
            "id": self.output.to_string(),
            "device_name": self.output.connector_name(),
            "attached": binding.is_some(),
            "card": self.card,
            "connector_id": self.connector.connector_id,
            "connection": self.connector.connection.name(),
            "crtc_id": self.connector.crtc.map(|crtc| crtc.crtc_id),
            "format": "dmabuf",
            "geometry_supported": desktop_geometry.is_some(),
        });
        if let Some(binding) = binding {
            entry["width"] = json!(binding.mode.width);
            entry["height"] = json!(binding.mode.height);
            entry["refresh_hz"] = json!(binding.mode.refresh_hz);
        }
        if let Some(geometry) = desktop_geometry {
            entry["desktop_geometry"] = json!(geometry);
        }
        entry
    }
}

/// Virtual outputs on every vibeshine_drm card, with each card's device.
pub struct Discovery {
    pub outputs: Vec<(DiscoveredOutput, DeviceNumber)>,
}

/// Enumerates every virtual output. Fails with `driver_unavailable` when no
/// vibeshine_drm card answers.
pub fn discover(probe: &dyn DisplayProbe) -> CaptureResult<Discovery> {
    discover_matching(probe, |_| true)
}

/// Resolves one output to its current CRTC. Only cards whose sysfs entries
/// name the connector are queried.
pub fn resolve(probe: &dyn DisplayProbe, output: OutputName) -> CaptureResult<OutputBinding> {
    let index = output.connector_index();
    // Query only cards whose sysfs entries name the connector. When none of
    // them is a usable vibeshine_drm card, look at every card so that a
    // present driver without this connector reports `output_not_found`.
    let discovery = match discover_matching(probe, |card| card.virtual_connectors.contains(&index))
    {
        Err(error) if error.code == "driver_unavailable" => discover(probe)?,
        result => result?,
    };
    let mut matches = discovery
        .outputs
        .iter()
        .filter(|(found, _)| found.output == output);
    let Some((found, device)) = matches.next() else {
        return Err(CaptureError::new(
            "output_not_found",
            format!(
                "no vibeshine_drm connector is named {}",
                output.connector_name()
            ),
        ));
    };
    if matches.next().is_some() {
        return Err(CaptureError::new(
            "output_ambiguous",
            format!(
                "more than one vibeshine_drm card has {}",
                output.connector_name()
            ),
        ));
    }
    found.binding(*device)
}

fn discover_matching(
    probe: &dyn DisplayProbe,
    wanted: impl Fn(&SysfsCard) -> bool,
) -> CaptureResult<Discovery> {
    let cards = probe.sysfs_cards().map_err(|error| {
        CaptureError::driver_unavailable(format!("cannot read DRM sysfs: {error}"))
    })?;
    let mut outputs = Vec::new();
    let mut vibeshine_cards = 0;
    let mut last_error = None;
    for card in cards.iter().filter(|card| wanted(card)) {
        match probe.query_card(card) {
            Ok(connectors) => {
                vibeshine_cards += 1;
                outputs.extend(connectors.into_iter().filter_map(|connector| {
                    let output = OutputName::new(connector.connector_index)?;
                    Some((
                        DiscoveredOutput {
                            output,
                            card: card.name.clone(),
                            connector,
                        },
                        card.device,
                    ))
                }));
            }
            Err(CardQueryError::WrongDriver) => {}
            Err(CardQueryError::Unavailable(error)) => {
                last_error = Some(format!("{}: {error}", card.name));
            }
        }
    }
    if vibeshine_cards == 0 {
        let detail = last_error.unwrap_or_else(|| "no vibeshine_drm card was found".into());
        return Err(CaptureError::driver_unavailable(detail));
    }
    outputs.sort_by_key(|(found, _)| found.output);
    Ok(Discovery { outputs })
}

#[cfg(test)]
mod tests;
