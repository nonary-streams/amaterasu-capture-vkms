//! Caller-scoped capture streams for owned `kwin:Virtual-N` outputs.
use crate::{
    backend::Backend,
    discovery::OutputBinding,
    frames::{StreamFault, StreamFrames},
    geometry::{self, LayoutOutput},
    model::{
        CaptureError, CaptureResult, MAX_ENUMERATION_LIMIT, MAX_LIFETIME_STREAMS, MAX_STREAMS,
        Operation, ResourceSetup, Settings, new_id, validate_operation_id,
    },
    output_name::OutputName,
    presenter::PresentationThread,
};
use amaterasu_media::CaptureGeometry;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

/// How often an open stream re-resolves its connector's CRTC and mode.
pub const RESOLVE_INTERVAL_MS: u64 = 1_000;

/// Desktop geometry of one stream, decided at open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamGeometry {
    /// No Wayland display was reachable at open; never reported.
    Unsupported,
    Ready(CaptureGeometry),
    /// The layout is momentarily unknown or disagrees with the scanout mode.
    NotReady,
}

impl StreamGeometry {
    fn supported(self) -> bool {
        self != Self::Unsupported
    }

    /// Recomputes a supported geometry from the latest layout.
    fn updated(self, layout: Option<&[LayoutOutput]>, binding: &OutputBinding) -> Self {
        if self == Self::Unsupported {
            return self;
        }
        layout
            .and_then(|outputs| {
                geometry::output_geometry(outputs, &binding.output.connector_name(), binding.mode)
                    .ok()
            })
            .map_or(Self::NotReady, Self::Ready)
    }
}

struct Stream {
    owner: String,
    operation: String,
    binding: OutputBinding,
    geometry: StreamGeometry,
    frames: Arc<StreamFrames>,
    presenter: PresentationThread,
    resource_setup: ResourceSetup,
    next_resolve_ms: u64,
}

pub struct Reply {
    pub payload: Value,
    /// The stream a successful open created, rolled back if the reply is lost.
    pub created: Option<u64>,
}

impl Reply {
    fn existing(payload: Value) -> Self {
        Self {
            payload,
            created: None,
        }
    }
}

/// What a native read needs, copied out so no controller lock is held while
/// a sink runs.
pub struct Reader {
    pub frames: Arc<StreamFrames>,
    pub geometry: StreamGeometry,
    pub resource_setup: ResourceSetup,
}

#[derive(Clone, Debug)]
struct Availability {
    available: bool,
    reason: Option<CaptureError>,
}

impl Availability {
    fn json(&self) -> Value {
        json!({
            "native_supported": true,
            "available": self.available,
            "reason": self.reason.as_ref().map(|error| error.code),
            "message": self.reason.as_ref().map(|error| error.message.as_str()),
        })
    }
}

pub struct Controller<B: Backend> {
    backend: B,
    settings: Settings,
    streams: BTreeMap<u64, Stream>,
    closed: BTreeMap<u64, String>,
    opened: u64,
    last_availability: Option<Availability>,
}

impl<B: Backend> Controller<B> {
    pub fn supports_retained_frames(&self) -> bool {
        self.settings.gpu_snapshot || cfg!(feature = "driver-read-leases")
    }

    pub fn requires_lease(&self) -> bool {
        self.settings.require_lease
    }

    pub fn new(backend: B, settings: Settings) -> Self {
        Self {
            backend,
            settings,
            streams: BTreeMap::new(),
            closed: BTreeMap::new(),
            opened: 0,
            last_availability: None,
        }
    }

    pub fn handle(&mut self, caller: &str, operation: Operation, now: u64) -> CaptureResult<Reply> {
        match operation {
            Operation::Status => Ok(Reply::existing(self.status(caller))),
            Operation::Enumerate { offset, limit } => {
                self.enumerate(caller, offset, limit).map(Reply::existing)
            }
            Operation::Open {
                output,
                operation_id,
                constant_rate,
                resource_capture_clock,
                resource_callback_contract,
            } => self.open(
                caller,
                &output,
                operation_id,
                constant_rate.unwrap_or(false),
                now,
                ResourceSetup::new(resource_capture_clock, resource_callback_contract)?,
            ),
            Operation::Lookup { operation_id } => {
                validate_operation_id(&operation_id)?;
                self.find_operation(caller, &operation_id)
                    .map(|(id, stream)| Reply::existing(stream_reply(id, stream)))
                    .ok_or_else(|| CaptureError::code("not_found"))
            }
            Operation::Close { stream } => {
                self.close(caller, stream)?;
                Ok(Reply::existing(json!({"ok":true})))
            }
        }
    }

    fn authorize_capture(&self, caller: &str) -> CaptureResult<()> {
        if !self.settings.enabled {
            return Err(CaptureError::new(
                "capture_disabled",
                "vibeshine_drm capture requires an enabled startup policy",
            ));
        }
        if !self.settings.capture_principals.contains(caller) {
            return Err(CaptureError::code("capture_permission_denied"));
        }
        Ok(())
    }

    fn open(
        &mut self,
        caller: &str,
        output: &str,
        operation: Option<String>,
        constant_rate: bool,
        now: u64,
        resource_setup: ResourceSetup,
    ) -> CaptureResult<Reply> {
        self.authorize_capture(caller)?;
        if constant_rate {
            return Err(CaptureError::code("unsupported_constant_rate"));
        }
        let operation = operation.ok_or_else(|| CaptureError::code("operation_id_required"))?;
        validate_operation_id(&operation)?;
        let name =
            OutputName::parse(output).ok_or_else(|| CaptureError::code("invalid_named_output"))?;
        if let Some((id, existing)) = self.find_operation(caller, &operation) {
            if existing.binding.output != name || existing.resource_setup != resource_setup {
                return Err(CaptureError::code("operation_conflict"));
            }
            return Ok(Reply::existing(stream_reply(id, existing)));
        }
        if self.streams.len() >= MAX_STREAMS || self.opened >= MAX_LIFETIME_STREAMS {
            return Err(CaptureError::code("resource_exhausted"));
        }
        let helper = self.settings.helper_endpoint()?;
        let binding = self.backend.resolve(name)?;
        let geometry = self.open_geometry(&binding)?;
        let id = new_id()?;
        if self.streams.contains_key(&id) || self.closed.contains_key(&id) {
            return Err(CaptureError::code("entropy_error"));
        }
        let source = self.backend.connect(helper, &binding)?;
        let frames = Arc::new(StreamFrames::new(
            binding.crtc_id,
            self.backend.monotonic_now_ns(),
        ));
        let presenter = PresentationThread::spawn_with_snapshot(
            source,
            frames.clone(),
            self.settings.gpu_snapshot,
        )
        .map_err(|error| {
            CaptureError::new(
                if self.settings.gpu_snapshot {
                    "gpu_snapshot_unavailable"
                } else {
                    "resource_exhausted"
                },
                format!("cannot start the presentation thread: {error}"),
            )
        })?;
        let stream = Stream {
            owner: caller.to_owned(),
            operation,
            binding,
            geometry,
            frames,
            presenter,
            next_resolve_ms: now + RESOLVE_INTERVAL_MS,
            resource_setup,
        };
        let payload = stream_reply(id, &stream);
        self.streams.insert(id, stream);
        self.opened += 1;
        Ok(Reply {
            payload,
            created: Some(id),
        })
    }

    /// Geometry at open: unsupported without a Wayland display, otherwise
    /// it must be consistent now so the open reply never reports a wrong one.
    fn open_geometry(&self, binding: &OutputBinding) -> CaptureResult<StreamGeometry> {
        let Some(layout) = self.backend.desktop_layout() else {
            return Ok(StreamGeometry::Unsupported);
        };
        geometry::output_geometry(&layout, &binding.output.connector_name(), binding.mode)
            .map(StreamGeometry::Ready)
            .map_err(|problem| {
                CaptureError::new(
                    "output_geometry_unavailable",
                    format!("{}: {}", binding.output, problem.describe()),
                )
            })
    }

    fn find_operation(&self, caller: &str, operation: &str) -> Option<(u64, &Stream)> {
        self.streams
            .iter()
            .find(|(_, stream)| stream.owner == caller && stream.operation == operation)
            .map(|(id, stream)| (*id, stream))
    }

    pub fn reader(&self, caller: &str, stream: u64) -> CaptureResult<Reader> {
        let stream = self
            .streams
            .get(&stream)
            .filter(|stream| stream.owner == caller)
            .ok_or_else(|| CaptureError::code("not_found"))?;
        Ok(Reader {
            frames: stream.frames.clone(),
            geometry: stream.geometry,
            resource_setup: stream.resource_setup.clone(),
        })
    }

    pub fn close(&mut self, caller: &str, id: u64) -> CaptureResult<()> {
        if self.closed.get(&id).is_some_and(|owner| owner == caller) {
            return Ok(());
        }
        if !self
            .streams
            .get(&id)
            .is_some_and(|stream| stream.owner == caller)
        {
            return Err(CaptureError::code("not_found"));
        }
        self.end_stream(id);
        Ok(())
    }

    /// Stops admitting native reads, then stops the presentation thread,
    /// which ends the helper. A read admitted before close keeps the stored
    /// frame alive until it ends.
    fn end_stream(&mut self, id: u64) {
        if let Some(mut stream) = self.streams.remove(&id) {
            stream.frames.admission.close();
            stream.presenter.stop();
            self.closed.insert(id, stream.owner);
        }
    }

    pub fn rollback(&mut self, caller: &str, created: Option<u64>) {
        if let Some(id) = created {
            let _ = self.close(caller, id);
        }
    }

    /// Re-resolves each stream's output on its interval, or at once after
    /// its presentation thread reported that the CRTC disappeared.
    pub fn refresh(&mut self, now: u64) {
        let layout_changed = self.backend.poll_layout();
        let due: Vec<u64> = self
            .streams
            .iter()
            .filter(|(_, stream)| {
                stream.frames.fault().is_none()
                    && (stream.frames.take_resolve_request()
                        || layout_changed
                        || now >= stream.next_resolve_ms)
            })
            .map(|(id, _)| *id)
            .collect();
        if due.is_empty() {
            return;
        }
        let layout = self.backend.desktop_layout();
        for id in due {
            let output = self.streams[&id].binding.output;
            let resolved = self.backend.resolve(output);
            let stream = self.streams.get_mut(&id).expect("due stream exists");
            stream.next_resolve_ms = now + RESOLVE_INTERVAL_MS;
            match resolved {
                Ok(binding) => apply_binding(stream, binding, layout.as_deref()),
                Err(error) if error.code == "driver_unavailable" => {}
                Err(error) => stream.frames.set_fault(StreamFault::OutputLost(error.code)),
            }
            // A thread paused for a missing CRTC retries at once on the new
            // binding, and a faulted one exits without finishing its pause.
            stream.presenter.wake();
        }
    }

    fn enumerate(
        &mut self,
        caller: &str,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> CaptureResult<Value> {
        if offset.unwrap_or(0) != 0 || !(1..=MAX_ENUMERATION_LIMIT).contains(&limit.unwrap_or(1)) {
            return Err(CaptureError::code("invalid_request"));
        }
        if !self.settings.capture_principals.contains(caller) {
            return Err(CaptureError::code("capture_permission_denied"));
        }
        let limit = limit.unwrap_or(MAX_ENUMERATION_LIMIT);
        let layout = self.backend.desktop_layout();
        let (outputs, driver) = match self.backend.discover() {
            Ok(discovery) => (
                discovery
                    .outputs
                    .iter()
                    .take(limit)
                    .map(|(output, device)| output.metadata(*device, layout.as_deref()))
                    .collect(),
                Ok(()),
            ),
            Err(error) => (Vec::new(), Err(error)),
        };
        let helper = self
            .settings
            .helper_endpoint()
            .and_then(|helper| self.backend.probe_helper(helper));
        let snapshot = if self.settings.gpu_snapshot && !cfg!(amaterasu_vkms_gpu_snapshot) {
            Err(CaptureError::new(
                "gpu_snapshot_unavailable",
                "the capture provider was built without linux-gpu-snapshot",
            ))
        } else {
            Ok(())
        };
        let reason = helper
            .clone()
            .err()
            .or_else(|| driver.clone().err())
            .or_else(|| snapshot.clone().err());
        let availability = Availability {
            available: reason.is_none(),
            reason,
        };
        let payload = json!({
            "ok": true,
            "outputs": outputs,
            "availability": availability.json(),
            "checks": {
                "frame_helper": check_json(&helper),
                "driver": check_json(&driver),
                "gpu_snapshot_build": check_json(&snapshot),
            },
            "geometry_supported": layout.is_some(),
            "frame_transport": "dmabuf",
        });
        self.last_availability = Some(availability);
        Ok(payload)
    }

    fn status(&self, caller: &str) -> Value {
        let streams: Vec<Value> = self
            .streams
            .iter()
            .filter(|(_, stream)| stream.owner == caller)
            .map(|(id, stream)| {
                let mut reply = stream_reply(*id, stream);
                reply["frames_delivered"] = json!(stream.frames.delivered());
                if let Some((color, metadata)) = stream.frames.color_status() {
                    reply["color"] = json!(color);
                    reply["hdr_metadata"] = json!(metadata);
                }
                reply["fault"] = json!(stream.frames.fault().map(StreamFault::code));
                reply
            })
            .collect();
        json!({
            "ok": true,
            "enabled": self.settings.enabled,
            "native_supported": true,
            "frame_helper_configured": self.settings.frame_helper.is_some() || self.settings.authority_channel,
            "authority_mode": if self.settings.authority_channel { "package_approved" } else { "legacy_helper" },
            "gpu_snapshot": self.settings.gpu_snapshot,
            "frame_delivery": if self.settings.gpu_snapshot {
                "gpu_snapshot"
            } else if cfg!(feature = "driver-read-leases") {
                "driver_read_lease"
            } else {
                "borrowed_dmabuf"
            },
            "driver_read_leases": cfg!(feature = "driver-read-leases"),
            "require_lease": self.settings.require_lease,
            "availability": self.last_availability.as_ref().map(Availability::json),
            "streams": streams,
            "opened": self.opened,
            "closed": self.closed.len(),
            "active": self.streams.len(),
        })
    }

    pub fn shutdown(&mut self) {
        self.settings.enabled = false;
        let ids: Vec<u64> = self.streams.keys().copied().collect();
        for id in ids {
            self.end_stream(id);
        }
    }
}

/// Follows a CRTC or mode change of the same connector. Moving to another
/// card or connector object is a different output and ends delivery.
fn apply_binding(stream: &mut Stream, binding: OutputBinding, layout: Option<&[LayoutOutput]>) {
    if binding.device != stream.binding.device
        || binding.connector_id != stream.binding.connector_id
    {
        stream
            .frames
            .set_fault(StreamFault::OutputLost("output_replaced"));
        return;
    }
    stream.geometry = stream.geometry.updated(layout, &binding);
    stream.frames.retarget(binding.crtc_id);
    stream.binding = binding;
}

fn check_json(result: &CaptureResult<()>) -> Value {
    match result {
        Ok(()) => json!({"ok": true}),
        Err(error) => json!({"ok": false, "code": error.code, "message": error.message}),
    }
}

fn stream_reply(id: u64, stream: &Stream) -> Value {
    let mut reply = json!({
        "ok": true,
        "stream": id,
        "output": stream.binding.output.to_string(),
        "operation_id": stream.operation,
        "card": stream.binding.card,
        "connector_id": stream.binding.connector_id,
        "crtc_id": stream.frames.crtc_id(),
        "width": stream.binding.mode.width,
        "height": stream.binding.mode.height,
        "refresh_hz": stream.binding.mode.refresh_hz,
        "frame_transport": "dmabuf",
        "geometry_supported": stream.geometry.supported(),
    });
    if let Some(clock) = &stream.resource_setup.clock {
        reply["resource_capture_clock"] = json!(clock);
        reply["resource_callback_contract"] = json!("cross_service_safe");
    }
    if let StreamGeometry::Ready(geometry) = stream.geometry {
        reply["desktop_geometry"] = json!(geometry);
    }
    reply
}

#[cfg(test)]
mod tests;
