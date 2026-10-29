//! Unprivileged Wayland client for the compositor's output layout. It binds
//! only wl_registry, wl_output and xdg-output; no screencast protocol.
use super::{DesktopLayout, LayoutOutput};
use std::{
    ffi::{CStr, c_char, c_int, c_void},
    ptr::NonNull,
    time::{Duration, Instant},
};

/// Bound for connecting and the two initial roundtrips.
const CONNECT_TIMEOUT_MS: c_int = 200;
/// A missing or failed display is retried at most this often.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);

#[repr(C)]
struct NativeOutput {
    name: [c_char; 128],
    logical_left: i32,
    logical_top: i32,
    logical_width: i32,
    logical_height: i32,
    mode_width: i32,
    mode_height: i32,
    has_mode: u32,
}

unsafe extern "C" {
    fn am_wl_open(timeout_ms: c_int) -> *mut c_void;
    fn am_wl_poll(connection: *mut c_void) -> c_int;
    fn am_wl_generation(connection: *const c_void) -> u64;
    fn am_wl_output_slots(connection: *const c_void) -> usize;
    fn am_wl_output_at(connection: *const c_void, slot: usize, output: *mut NativeOutput) -> c_int;
    fn am_wl_close(connection: *mut c_void);
}

struct Connection(NonNull<c_void>);

impl Connection {
    fn open() -> Option<Self> {
        // SAFETY: the adapter returns a new connection owned by this value, or null.
        NonNull::new(unsafe { am_wl_open(CONNECT_TIMEOUT_MS) }).map(Self)
    }

    fn outputs(&self) -> Vec<LayoutOutput> {
        // SAFETY: the connection is live; the adapter only reads its table.
        let slots = unsafe { am_wl_output_slots(self.0.as_ptr()) };
        (0..slots)
            .filter_map(|slot| {
                let mut native = NativeOutput {
                    name: [0; 128],
                    logical_left: 0,
                    logical_top: 0,
                    logical_width: 0,
                    logical_height: 0,
                    mode_width: 0,
                    mode_height: 0,
                    has_mode: 0,
                };
                // SAFETY: the connection is live and `native` is a writable record.
                if unsafe { am_wl_output_at(self.0.as_ptr(), slot, &mut native) } != 0 {
                    return None;
                }
                Some(layout_output(&native))
            })
            .collect()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // SAFETY: closes the uniquely owned connection exactly once.
        unsafe { am_wl_close(self.0.as_ptr()) };
    }
}

fn layout_output(native: &NativeOutput) -> LayoutOutput {
    // SAFETY: the adapter always NUL-terminates the fixed name buffer.
    let name = unsafe { CStr::from_ptr(native.name.as_ptr()) };
    let pixel_mode = (native.has_mode != 0 && native.mode_width > 0 && native.mode_height > 0)
        .then_some((native.mode_width as u32, native.mode_height as u32));
    LayoutOutput {
        name: name.to_string_lossy().into_owned(),
        logical_left: native.logical_left,
        logical_top: native.logical_top,
        logical_width: native.logical_width,
        logical_height: native.logical_height,
        pixel_mode,
    }
}

pub struct WaylandLayout {
    connection: Option<Connection>,
    generation: u64,
    last_attempt: Option<Instant>,
}

// SAFETY: the connection and its proxies are only touched through `&mut
// self`; the controller that owns this value serializes every call under its
// mutex, and libwayland permits use from any one thread at a time.
unsafe impl Send for WaylandLayout {}

impl WaylandLayout {
    pub fn new() -> Self {
        Self {
            connection: None,
            generation: 0,
            last_attempt: None,
        }
    }

    fn ensure_connected(&mut self) {
        if self.connection.is_some()
            || self
                .last_attempt
                .is_some_and(|attempt| attempt.elapsed() < RECONNECT_INTERVAL)
        {
            return;
        }
        self.last_attempt = Some(Instant::now());
        self.connection = Connection::open();
    }
}

impl DesktopLayout for WaylandLayout {
    fn poll(&mut self) -> bool {
        let Some(connection) = &self.connection else {
            return false;
        };
        // SAFETY: the connection is live and used only from this call.
        if unsafe { am_wl_poll(connection.0.as_ptr()) } < 0 {
            self.connection = None;
            return true;
        }
        // SAFETY: as above.
        let generation = unsafe { am_wl_generation(connection.0.as_ptr()) };
        let changed = generation != self.generation;
        self.generation = generation;
        changed
    }

    fn outputs(&mut self) -> Option<Vec<LayoutOutput>> {
        self.ensure_connected();
        self.connection.as_ref().map(Connection::outputs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_records_convert_names_and_modes() {
        let mut native = NativeOutput {
            name: [0; 128],
            logical_left: 1210,
            logical_top: 0,
            logical_width: 1920,
            logical_height: 1080,
            mode_width: 1920,
            mode_height: 1080,
            has_mode: 1,
        };
        for (slot, byte) in native.name.iter_mut().zip(b"Virtual-9") {
            *slot = *byte as c_char;
        }
        let output = layout_output(&native);
        assert_eq!(output.name, "Virtual-9");
        assert_eq!(output.pixel_mode, Some((1920, 1080)));
        native.has_mode = 0;
        assert_eq!(layout_output(&native).pixel_mode, None);
    }

    #[test]
    #[ignore = "connects to the user's Wayland compositor"]
    fn reads_the_live_compositor_layout() {
        let mut layout = WaylandLayout::new();
        let outputs = layout.outputs().expect("a reachable Wayland display");
        assert!(!outputs.is_empty());
    }
}
