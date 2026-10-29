//! Desktop geometry of an owned output from the compositor's Wayland layout.
//!
//! The semantics match capture-pipewire's `kwin:Virtual-N` geometry exactly:
//! rectangles use compositor logical coordinates from xdg-output (position
//! and size after fractional scaling), and the virtual desktop is the
//! bounding box of every live output. The DRM mode is a cross-check only:
//! the compositor's current pixel mode for the output must equal the CRTC
//! mode the frames come from, or the geometry is reported as not ready.
#[cfg(amaterasu_kwin_geometry)]
mod wayland;

use crate::discovery::DisplayMode;
use amaterasu_media::{CaptureGeometry, DesktopRect};

/// Largest accepted logical output extent, as in capture-pipewire.
const MAX_OUTPUT_EXTENT: i32 = 32_768;
/// Largest accepted logical desktop extent, as in capture-pipewire.
const MAX_DESKTOP_EXTENT: i64 = 65_536;

/// One `wl_output` with its xdg-output logical rectangle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutOutput {
    /// `wl_output.name`, for example `Virtual-9`.
    pub name: String,
    pub logical_left: i32,
    pub logical_top: i32,
    pub logical_width: i32,
    pub logical_height: i32,
    /// Current `wl_output.mode` in pixels, if announced.
    pub pixel_mode: Option<(u32, u32)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeometryProblem {
    /// The compositor does not list an output with this name.
    OutputMissing,
    /// Some output has no usable logical rectangle, or the desktop is too large.
    InvalidLayout,
    /// The compositor's pixel mode differs from the CRTC mode, usually
    /// while a mode change is in flight.
    ModeMismatch,
}

impl GeometryProblem {
    pub fn describe(self) -> &'static str {
        match self {
            Self::OutputMissing => "the compositor does not list this output",
            Self::InvalidLayout => "the compositor's output layout is incomplete",
            Self::ModeMismatch => "the compositor's output mode differs from the scanout mode",
        }
    }
}

/// Geometry of `connector_name` within `outputs`, cross-checked against
/// the CRTC mode that produces its frames.
pub fn output_geometry(
    outputs: &[LayoutOutput],
    connector_name: &str,
    scanout: DisplayMode,
) -> Result<CaptureGeometry, GeometryProblem> {
    let selected = outputs
        .iter()
        .find(|output| output.name == connector_name)
        .ok_or(GeometryProblem::OutputMissing)?;
    if selected.pixel_mode != Some((scanout.width, scanout.height)) {
        return Err(GeometryProblem::ModeMismatch);
    }
    let desktop = bounding_box(outputs)?;
    let output = logical_rect(selected)?;
    CaptureGeometry::new(output, desktop).map_err(|_| GeometryProblem::InvalidLayout)
}

fn valid_extent(extent: i32) -> bool {
    extent > 0 && extent <= MAX_OUTPUT_EXTENT
}

fn logical_rect(output: &LayoutOutput) -> Result<DesktopRect, GeometryProblem> {
    if !valid_extent(output.logical_width) || !valid_extent(output.logical_height) {
        return Err(GeometryProblem::InvalidLayout);
    }
    DesktopRect::new(
        output.logical_left,
        output.logical_top,
        output.logical_width as u32,
        output.logical_height as u32,
    )
    .map_err(|_| GeometryProblem::InvalidLayout)
}

/// The smallest rectangle containing every output. Any output without a
/// valid logical size invalidates the layout, as in capture-pipewire.
fn bounding_box(outputs: &[LayoutOutput]) -> Result<DesktopRect, GeometryProblem> {
    let (mut left, mut top) = (i64::from(i32::MAX), i64::from(i32::MAX));
    let (mut right, mut bottom) = (i64::from(i32::MIN), i64::from(i32::MIN));
    for output in outputs {
        if !valid_extent(output.logical_width) || !valid_extent(output.logical_height) {
            return Err(GeometryProblem::InvalidLayout);
        }
        let (x, y) = (
            i64::from(output.logical_left),
            i64::from(output.logical_top),
        );
        left = left.min(x);
        top = top.min(y);
        right = right.max(x + i64::from(output.logical_width));
        bottom = bottom.max(y + i64::from(output.logical_height));
    }
    let (width, height) = (right - left, bottom - top);
    if width <= 0 || height <= 0 || width > MAX_DESKTOP_EXTENT || height > MAX_DESKTOP_EXTENT {
        return Err(GeometryProblem::InvalidLayout);
    }
    DesktopRect::new(left as i32, top as i32, width as u32, height as u32)
        .map_err(|_| GeometryProblem::InvalidLayout)
}

/// The compositor's current output layout. The production source is an
/// unprivileged Wayland client; tests substitute a fake.
pub trait DesktopLayout: Send {
    /// Dispatches pending compositor events without blocking. Returns true
    /// when the layout changed since the previous call.
    fn poll(&mut self) -> bool;
    /// Every live output, or `None` when no Wayland display is reachable.
    fn outputs(&mut self) -> Option<Vec<LayoutOutput>>;
}

/// The build has no Wayland client; geometry is unsupported.
#[cfg(not(amaterasu_kwin_geometry))]
pub struct WaylandLayout;

#[cfg(not(amaterasu_kwin_geometry))]
impl WaylandLayout {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(not(amaterasu_kwin_geometry))]
impl DesktopLayout for WaylandLayout {
    fn poll(&mut self) -> bool {
        false
    }
    fn outputs(&mut self) -> Option<Vec<LayoutOutput>> {
        None
    }
}

#[cfg(amaterasu_kwin_geometry)]
pub use wayland::WaylandLayout;

#[cfg(test)]
mod tests;
