//! Fakes for ownership and state tests. They do not qualify real capture.
use crate::{
    backend::Backend,
    discovery::{
        Connection, ConnectorState, CrtcState, DiscoveredOutput, Discovery, DisplayMode,
        OutputBinding,
    },
    geometry::LayoutOutput,
    model::{CaptureError, CaptureResult, HelperEndpoint, Settings},
    output_name::OutputName,
    presenter::{FrameSource, SourceInterrupt},
};
use amaterasu_vkms_frame_helper::{
    client::{ClientError, ExportedFrame, FrameExport, Presentation},
    drm_node::DeviceNumber,
    protocol::{Failure, FrameDescriptor, MAX_PLANES, PRESENT_CHANGED, PRESENT_TIMEOUT},
};
use std::{
    collections::VecDeque,
    fs::File,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// One scripted FRAME answer. Without one, FRAME reports an empty CRTC.
#[derive(Clone, Copy, Debug)]
pub enum Scripted {
    Frame { sequence: u64, timestamp_ns: u64 },
    FencedFrame { sequence: u64 },
    Empty,
    Refuse(Failure),
    HelperGone,
}

/// One scripted WAIT answer. Without one, WAIT blocks until its timeout
/// (or an interrupt) like the driver.
#[derive(Clone, Copy, Debug)]
pub enum ScriptedWait {
    Presented(u64),
    Refuse(Failure),
    HelperGone,
}

#[derive(Default)]
struct FakeDriver {
    waits: VecDeque<ScriptedWait>,
    frames: VecDeque<Scripted>,
}

/// Shared script and observations for every source a fake backend creates.
#[derive(Default)]
pub struct SourceScript {
    driver: Mutex<FakeDriver>,
    answered: Condvar,
    /// Write ends of every exported descriptor; each reports when the read
    /// end (the descriptor the module owned or lent) has been closed.
    pub writers: Mutex<Vec<File>>,
    /// The CRTC of every FRAME request.
    pub requested_crtcs: Mutex<Vec<u32>>,
    /// The CRTC and last sequence of every WAIT request.
    pub waits: Mutex<Vec<(u32, u64)>>,
    /// Models a helper stuck in the driver: interrupts are ignored and a
    /// WAIT returns only at its timeout.
    pub ignore_interrupts: AtomicBool,
}

impl SourceScript {
    pub fn push(&self, answer: Scripted) {
        self.driver_state().frames.push_back(answer);
        self.answered.notify_all();
    }

    pub fn push_wait(&self, answer: ScriptedWait) {
        self.driver_state().waits.push_back(answer);
        self.answered.notify_all();
    }

    /// A completed presentation: WAIT reports it and FRAME exports it.
    pub fn present(&self, sequence: u64, timestamp_ns: u64) {
        self.present_answer(
            sequence,
            Scripted::Frame {
                sequence,
                timestamp_ns,
            },
        );
    }

    /// Like [`Self::present`] with an acquire fence on the plane.
    pub fn present_fenced(&self, sequence: u64) {
        self.present_answer(sequence, Scripted::FencedFrame { sequence });
    }

    fn present_answer(&self, sequence: u64, frame: Scripted) {
        let mut driver = self.driver_state();
        driver.frames.push_back(frame);
        driver.waits.push_back(ScriptedWait::Presented(sequence));
        drop(driver);
        self.answered.notify_all();
    }

    pub fn all_lent_descriptors_closed(&self) -> bool {
        self.open_descriptors() == 0
    }

    /// Exported descriptors the module has not closed yet.
    pub fn open_descriptors(&self) -> usize {
        self.writers
            .lock()
            .unwrap()
            .iter()
            .filter(|writer| !reader_closed(writer))
            .count()
    }

    fn driver_state(&self) -> std::sync::MutexGuard<'_, FakeDriver> {
        self.driver.lock().unwrap()
    }

    fn tracked_descriptor(&self) -> OwnedFd {
        let mut fds = [-1; 2];
        // SAFETY: `fds` is a writable two-element array as pipe2 requires.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: pipe2 succeeded and returned two new descriptors owned here.
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) };
        self.writers.lock().unwrap().push(write);
        read
    }

    /// An exported 1920x1080 frame whose descriptors are tracked pipes.
    pub fn export(&self, sequence: u64, timestamp_ns: u64, fenced: bool) -> ExportedFrame {
        let mut pitches = [0; MAX_PLANES];
        pitches[0] = 7680;
        let mut fences: [Option<OwnedFd>; MAX_PLANES] = Default::default();
        if fenced {
            fences[0] = Some(self.tracked_descriptor());
        }
        ExportedFrame {
            descriptor: FrameDescriptor {
                crtc_id: 41,
                sequence,
                timestamp_ns,
                width: 1920,
                height: 1080,
                fourcc: u32::from_le_bytes(*b"XR24"),
                modifier: 0x0100_0000_0000_0004,
                plane_count: 1,
                pitches,
                offsets: [0; MAX_PLANES],
                fence_mask: u32::from(fenced),
                guard_count: 0,

                color: 1,
                hdr_metadata: [0; 12],
            },
            dma_bufs: vec![self.tracked_descriptor()],
            read_guards: Vec::new(),
            fences,
        }
    }
}

fn reader_closed(writer: &File) -> bool {
    let mut poll = libc::pollfd {
        fd: writer.as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: one initialized pollfd is passed with a zero timeout.
    assert_eq!(unsafe { libc::poll(&mut poll, 1, 0) }, 1);
    poll.revents & libc::POLLERR != 0
}

/// Polls `condition` until it holds or two seconds pass.
pub fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    condition()
}

/// Interrupts one fake source; the script's other sources are unaffected.
struct FakeInterrupt {
    interrupted: AtomicBool,
    script: Arc<SourceScript>,
}

impl SourceInterrupt for FakeInterrupt {
    fn interrupt(&self) {
        let _driver = self.script.driver_state();
        self.interrupted.store(true, Ordering::SeqCst);
        self.script.answered.notify_all();
    }
}

pub struct FakeSource {
    script: Arc<SourceScript>,
    interrupt: Arc<FakeInterrupt>,
}

impl FakeSource {
    pub fn new(script: Arc<SourceScript>) -> Self {
        Self {
            interrupt: Arc::new(FakeInterrupt {
                interrupted: AtomicBool::new(false),
                script: script.clone(),
            }),
            script,
        }
    }

    fn interrupted(&self) -> bool {
        self.interrupt.interrupted.load(Ordering::SeqCst)
            && !self.script.ignore_interrupts.load(Ordering::SeqCst)
    }
}

impl FrameSource for FakeSource {
    fn wait(
        &mut self,
        crtc_id: u32,
        last_sequence: u64,
        timeout_ms: u32,
    ) -> Result<Presentation, ClientError> {
        self.script
            .waits
            .lock()
            .unwrap()
            .push((crtc_id, last_sequence));
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.into());
        let mut driver = self.script.driver_state();
        loop {
            if self.interrupted() {
                return Err(ClientError::Closed);
            }
            if let Some(answer) = driver.waits.pop_front() {
                return match answer {
                    ScriptedWait::Presented(sequence) => Ok(Presentation {
                        sequence,
                        timestamp_ns: 0,
                        flags: PRESENT_CHANGED,
                    }),
                    ScriptedWait::Refuse(failure) => Err(ClientError::Refused {
                        failure,
                        os_error: 0,
                    }),
                    ScriptedWait::HelperGone => Err(ClientError::Closed),
                };
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(Presentation {
                    sequence: last_sequence,
                    timestamp_ns: 0,
                    flags: PRESENT_TIMEOUT,
                });
            }
            driver = self
                .script
                .answered
                .wait_timeout(driver, deadline - now)
                .unwrap()
                .0;
        }
    }

    fn frame(&mut self, crtc_id: u32) -> Result<FrameExport, ClientError> {
        self.script.requested_crtcs.lock().unwrap().push(crtc_id);
        if self.interrupted() {
            return Err(ClientError::Closed);
        }
        let answer = self
            .script
            .driver_state()
            .frames
            .pop_front()
            .unwrap_or(Scripted::Empty);
        match answer {
            Scripted::Frame {
                sequence,
                timestamp_ns,
            } => Ok(FrameExport::Ready(self.script.export(
                sequence,
                timestamp_ns,
                false,
            ))),
            Scripted::FencedFrame { sequence } => Ok(FrameExport::Ready(
                self.script.export(sequence, 1_000, true),
            )),
            Scripted::Empty => Ok(FrameExport::Empty),
            Scripted::Refuse(failure) => Err(ClientError::Refused {
                failure,
                os_error: 0,
            }),
            Scripted::HelperGone => Err(ClientError::Closed),
        }
    }

    fn interrupter(&self) -> Arc<dyn SourceInterrupt> {
        self.interrupt.clone()
    }
}

pub const CARD3: DeviceNumber = DeviceNumber {
    major: 226,
    minor: 3,
};

pub fn connector(index: u32, crtc_id: u32, width: u32) -> ConnectorState {
    ConnectorState {
        connector_id: 100 + index,
        connector_index: index,
        connection: Connection::Connected,
        crtc: Some(CrtcState {
            crtc_id,
            mode: Some(DisplayMode {
                width,
                height: 1080,
                refresh_hz: 60,
            }),
        }),
    }
}

/// A backend over a mutable list of connectors on card3.
pub struct FakeBackend {
    pub connectors: Arc<Mutex<Result<Vec<ConnectorState>, CaptureError>>>,
    pub helper: Arc<Mutex<CaptureResult<()>>>,
    pub script: Arc<SourceScript>,
    pub connects: Arc<Mutex<Vec<(String, u32)>>>,
    /// `None` models a host without a reachable Wayland display.
    pub layout: Arc<Mutex<Option<Vec<LayoutOutput>>>>,
    pub layout_changed: Arc<Mutex<bool>>,
}

/// A scaled primary with the owned outputs appended to its right.
pub fn appended_layout(virtual_9_width: u32) -> Vec<LayoutOutput> {
    let output =
        |name: &str, left: i32, width: i32, height: i32, pixels: (u32, u32)| LayoutOutput {
            name: name.into(),
            logical_left: left,
            logical_top: 0,
            logical_width: width,
            logical_height: height,
            pixel_mode: Some(pixels),
        };
    vec![
        output("eDP-1", 0, 1210, 756, (3024, 1890)),
        output(
            "Virtual-9",
            1210,
            virtual_9_width as i32,
            1080,
            (virtual_9_width, 1080),
        ),
        output(
            "Virtual-10",
            1210 + virtual_9_width as i32,
            1920,
            1080,
            (1920, 1080),
        ),
    ]
}

impl FakeBackend {
    pub fn new() -> Self {
        Self {
            connectors: Arc::new(Mutex::new(Ok(vec![
                connector(9, 41, 1920),
                connector(10, 42, 1920),
            ]))),
            helper: Arc::new(Mutex::new(Ok(()))),
            script: Arc::new(SourceScript::default()),
            connects: Arc::new(Mutex::new(Vec::new())),
            layout: Arc::new(Mutex::new(Some(appended_layout(1920)))),
            layout_changed: Arc::new(Mutex::new(false)),
        }
    }

    pub fn handles(&self) -> Self {
        Self {
            connectors: self.connectors.clone(),
            helper: self.helper.clone(),
            script: self.script.clone(),
            connects: self.connects.clone(),
            layout: self.layout.clone(),
            layout_changed: self.layout_changed.clone(),
        }
    }
}

impl Backend for FakeBackend {
    fn discover(&self) -> CaptureResult<Discovery> {
        let connectors = self.connectors.lock().unwrap().clone()?;
        Ok(Discovery {
            outputs: connectors
                .into_iter()
                .map(|connector| {
                    (
                        DiscoveredOutput {
                            output: OutputName::new(connector.connector_index).unwrap(),
                            card: "card3".into(),
                            connector,
                        },
                        CARD3,
                    )
                })
                .collect(),
        })
    }

    fn resolve(&self, output: OutputName) -> CaptureResult<OutputBinding> {
        let probe = FakeProbe(self.connectors.lock().unwrap().clone());
        crate::discovery::resolve(&probe, output)
    }

    fn probe_helper(&self, _helper: HelperEndpoint<'_>) -> CaptureResult<()> {
        self.helper.lock().unwrap().clone()
    }

    fn connect(
        &self,
        helper: HelperEndpoint<'_>,
        binding: &OutputBinding,
    ) -> CaptureResult<Box<dyn FrameSource>> {
        self.helper.lock().unwrap().clone()?;
        self.connects
            .lock()
            .unwrap()
            .push((helper.label(), binding.crtc_id));
        Ok(Box::new(FakeSource::new(self.script.clone())))
    }

    fn monotonic_now_ns(&self) -> u64 {
        1_000_000
    }

    fn poll_layout(&self) -> bool {
        std::mem::take(&mut *self.layout_changed.lock().unwrap())
    }

    fn desktop_layout(&self) -> Option<Vec<LayoutOutput>> {
        self.layout.lock().unwrap().clone()
    }
}

struct FakeProbe(Result<Vec<ConnectorState>, CaptureError>);

impl crate::discovery::DisplayProbe for FakeProbe {
    fn sysfs_cards(&self) -> Result<Vec<crate::discovery::SysfsCard>, String> {
        let connectors = self.0.clone().map_err(|error| error.message)?;
        Ok(vec![crate::discovery::SysfsCard {
            name: "card3".into(),
            device: CARD3,
            virtual_connectors: connectors.iter().map(|c| c.connector_index).collect(),
        }])
    }
    fn query_card(
        &self,
        _: &crate::discovery::SysfsCard,
    ) -> Result<Vec<ConnectorState>, crate::discovery::CardQueryError> {
        self.0
            .clone()
            .map_err(|error| crate::discovery::CardQueryError::Unavailable(error.message))
    }
}

pub const HELPER: &str = "/usr/libexec/amaterasu/amaterasu-vkms-frame-helper";

pub fn settings() -> Settings {
    Settings {
        enabled: true,
        capture_principals: ["amaterasu.video".into()].into(),
        frame_helper: Some(HELPER.into()),
        ..Settings::default()
    }
}
