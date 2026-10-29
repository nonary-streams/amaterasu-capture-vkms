use super::*;

fn output(name: &str, rect: (i32, i32, i32, i32), pixels: (u32, u32)) -> LayoutOutput {
    LayoutOutput {
        name: name.into(),
        logical_left: rect.0,
        logical_top: rect.1,
        logical_width: rect.2,
        logical_height: rect.3,
        pixel_mode: Some(pixels),
    }
}

fn mode(width: u32, height: u32) -> DisplayMode {
    DisplayMode {
        width,
        height,
        refresh_hz: 60,
    }
}

fn rect(left: i32, top: i32, width: u32, height: u32) -> DesktopRect {
    DesktopRect::new(left, top, width, height).unwrap()
}

#[test]
fn an_owned_output_appended_right_of_a_scaled_primary() {
    // A 3024x1890 primary at scale 2.5 is 1210x756 logical; the owned
    // 1920x1080 output at scale 1 follows it on the right.
    let outputs = [
        output("eDP-1", (0, 0, 1210, 756), (3024, 1890)),
        output("Virtual-9", (1210, 0, 1920, 1080), (1920, 1080)),
    ];
    let geometry = output_geometry(&outputs, "Virtual-9", mode(1920, 1080)).unwrap();
    assert_eq!(geometry.output, rect(1210, 0, 1920, 1080));
    assert_eq!(geometry.virtual_desktop, rect(0, 0, 3130, 1080));
}

#[test]
fn a_scaled_owned_output_reports_logical_size() {
    let outputs = [
        output("DP-1", (0, 0, 2560, 1440), (2560, 1440)),
        output("Virtual-10", (2560, 0, 1536, 864), (3840, 2160)),
    ];
    let geometry = output_geometry(&outputs, "Virtual-10", mode(3840, 2160)).unwrap();
    assert_eq!(geometry.output, rect(2560, 0, 1536, 864));
    assert_eq!(geometry.virtual_desktop, rect(0, 0, 4096, 1440));
}

#[test]
fn negative_origins_and_vertical_stacks_are_bounded() {
    let outputs = [
        output("DP-1", (0, 0, 1920, 1080), (1920, 1080)),
        output("HDMI-A-1", (-1280, 200, 1280, 1024), (1280, 1024)),
        output("Virtual-1", (0, 1080, 1280, 720), (1280, 720)),
    ];
    let geometry = output_geometry(&outputs, "Virtual-1", mode(1280, 720)).unwrap();
    assert_eq!(geometry.output, rect(0, 1080, 1280, 720));
    assert_eq!(geometry.virtual_desktop, rect(-1280, 0, 3200, 1800));
}

#[test]
fn a_single_owned_output_is_the_whole_desktop() {
    let outputs = [output("Virtual-1", (0, 0, 1920, 1080), (1920, 1080))];
    let geometry = output_geometry(&outputs, "Virtual-1", mode(1920, 1080)).unwrap();
    assert_eq!(geometry.output, geometry.virtual_desktop);
}

#[test]
fn a_mode_that_differs_from_the_scanout_is_not_ready() {
    let mut outputs = vec![output("Virtual-9", (0, 0, 1920, 1080), (1920, 1080))];
    assert_eq!(
        output_geometry(&outputs, "Virtual-9", mode(2560, 1440)),
        Err(GeometryProblem::ModeMismatch)
    );
    outputs[0].pixel_mode = None;
    assert_eq!(
        output_geometry(&outputs, "Virtual-9", mode(1920, 1080)),
        Err(GeometryProblem::ModeMismatch)
    );
}

#[test]
fn missing_outputs_and_invalid_layouts_are_refused() {
    let outputs = [output("Virtual-9", (0, 0, 1920, 1080), (1920, 1080))];
    assert_eq!(
        output_geometry(&outputs, "Virtual-1", mode(1920, 1080)),
        Err(GeometryProblem::OutputMissing)
    );
    let incomplete = [
        output("Virtual-9", (0, 0, 1920, 1080), (1920, 1080)),
        output("DP-1", (1920, 0, 0, 0), (2560, 1440)),
    ];
    assert_eq!(
        output_geometry(&incomplete, "Virtual-9", mode(1920, 1080)),
        Err(GeometryProblem::InvalidLayout)
    );
    let too_wide = [
        output("Virtual-9", (0, 0, 1920, 1080), (1920, 1080)),
        output("DP-1", (70_000, 0, 1920, 1080), (1920, 1080)),
    ];
    assert_eq!(
        output_geometry(&too_wide, "Virtual-9", mode(1920, 1080)),
        Err(GeometryProblem::InvalidLayout)
    );
    let oversized = [output("Virtual-9", (0, 0, 40_000, 1080), (1920, 1080))];
    assert_eq!(
        output_geometry(&oversized, "Virtual-9", mode(1920, 1080)),
        Err(GeometryProblem::InvalidLayout)
    );
}
