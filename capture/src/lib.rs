//! DMA-BUF capture of owned `kwin:Virtual-N` outputs on vibeshine_drm.
//!
//! Frames are the driver's completed primary-plane scanouts, exported as
//! DMA-BUFs by the separately installed `amaterasu-vkms-frame-helper`, which
//! alone holds CAP_SYS_ADMIN. The default path lends ordinary scanout descriptors
//! through capture v1.5. Optional private copies support retained frames; experimental
//! driver read leases require an explicit matching build feature. This
//! module never holds that capability.
//! Native modules are trusted code; runtime API grants are not a native-code
//! sandbox. The provider exists only on Linux.
#![cfg(target_os = "linux")]

mod admission;
mod backend;
mod controller;
mod discovery;
mod events;
mod frames;
mod geometry;
mod leases;
mod model;
mod native;
mod output_name;
mod presenter;
mod retirement;
mod snapshot;
mod threading;

use amaterasu_module_sdk::capture::{
    SourcePreflightFactory, SourcePreflightProvider, SourcePreflightRequest,
    SourcePreflightResponse,
};
use amaterasu_module_sdk::{
    Command, LogQueue, ManagedModule, ModuleResult, Response, RuntimeClient,
};
use backend::NativeBackend;
use controller::Controller;
use model::{CaptureError, Operation, Settings};
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

const SERVICE: &str = "amaterasu.capture-vkms/1";
const REQUESTS_PER_TICK: usize = 8;
const MAX_PENDING_CLOSES: usize = 32;

/// Package composition may request the reserved false settings placeholder
/// only when the selected artifact supports that field. This marker establishes
/// schema/protocol support, not native admission or source readiness; those
/// still require the adopted v2 root-peer channel and actual PRECHECK.
#[used]
#[unsafe(no_mangle)]
pub static AMATERASU_CAPTURE_AUTHORITY_PRECHECK_CAPABILITY: [u8;
    b"AMATERASU_CAPTURE_AUTHORITY_PRECHECK=1\0".len()] =
    *b"AMATERASU_CAPTURE_AUTHORITY_PRECHECK=1\0";

struct VkmsPreflight(native::Shared<NativeBackend>);
impl SourcePreflightProvider for VkmsPreflight {
    fn query(
        &self,
        caller: &str,
        request: &SourcePreflightRequest,
    ) -> Result<SourcePreflightResponse, amaterasu_module_sdk::abi::Status> {
        let controller = self.0.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => amaterasu_media::MEDIA_BUSY,
            std::sync::TryLockError::Poisoned(_) => amaterasu_media::MEDIA_BACKEND_ERROR,
        })?;
        Ok(controller.preflight(caller, request))
    }
}

struct CaptureModule {
    runtime: RuntimeClient,
    log_queue: LogQueue,
    controller: native::Shared<NativeBackend>,
    _factory: Box<native::Factory<NativeBackend>>,
    _preflight_factory: Box<SourcePreflightFactory<VkmsPreflight>>,
    epoch: Instant,
    pending_closes: Vec<PendingClose>,
}

struct PendingClose {
    request: Option<u64>,
    caller: String,
    stream: u64,
    cleanup_started: bool,
    reply: Option<serde_json::Value>,
}

impl ManagedModule for CaptureModule {
    fn start(runtime: RuntimeClient) -> ModuleResult<Self> {
        let Response::Result { payload } = runtime.send(Command::GetSettings {})? else {
            return Err("vibeshine_drm capture settings unavailable".into());
        };
        let settings: Settings =
            serde_json::from_value(payload).map_err(|error| error.to_string())?;
        settings.validate().map_err(|error| error.message)?;
        let (logger, log_queue) = runtime.log_emitter();
        let mut controller = Controller::new(NativeBackend::new(), settings);
        controller.set_logger(logger);
        let controller = Arc::new(Mutex::new(controller));
        let factory = native::Factory::new(controller.clone());
        // SAFETY: the retained inner factory binds controller metadata only.
        // Source preflight uses read-only KMS inventory and authority PRECHECK,
        // never the old helper probe which starts a privileged helper session.
        let preflight_factory = unsafe {
            SourcePreflightFactory::new(&factory.api, Arc::new(VkmsPreflight(controller.clone())))
        };
        runtime.require_success(Command::Register {
            service: SERVICE.into(),
        })?;
        // SAFETY: the boxed factory stays at a stable address until this module
        // is destroyed; every binding retains its own shared controller.
        unsafe {
            runtime.register_native_typed(
                SERVICE,
                amaterasu_module_sdk::media::native_contracts::CAPTURE,
                &preflight_factory.api,
            )
        }?;
        Ok(Self {
            runtime,
            log_queue,
            controller,
            _factory: factory,
            _preflight_factory: preflight_factory,
            epoch: Instant::now(),
            pending_closes: Vec::new(),
        })
    }

    fn tick(&mut self) -> ModuleResult<()> {
        self.log_queue.flush(&self.runtime);
        self.poll_pending_closes()?;
        let now = self.epoch.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        {
            let Some(mut controller) = Self::try_controller(&self.controller)? else {
                return Ok(());
            };
            // Per-stream failures stay visible to their Close waiters and in
            // status. Continue independent requests; quiescence aggregates them.
            let _ = controller.poll_retirement();
            controller.refresh(now);
        }
        for _ in 0..REQUESTS_PER_TICK {
            // A preflight callback may be in a slow native probe. Acquire the
            // controller before polling so no request is stranded behind it.
            let shared = Arc::clone(&self.controller);
            let Some(mut controller) = Self::try_controller(&shared)? else {
                return Ok(());
            };
            let (id, caller, service, payload) = match self.runtime.send(Command::Poll {})? {
                Response::Pending => break,
                Response::Request {
                    id,
                    caller,
                    service,
                    payload,
                    ..
                } => (id, caller, service, payload),
                response => return Err(format!("vibeshine_drm capture poll failed: {response:?}")),
            };
            if !matches!(
                self.runtime.send(Command::CheckRequest { request: id })?,
                Response::Ok
            ) {
                continue;
            }
            match serde_json::from_value::<Operation>(payload) {
                Ok(Operation::Close { stream }) if service == SERVICE => {
                    drop(controller);
                    self.enqueue_close(id, &caller, stream)?;
                }
                Ok(operation) if service == SERVICE => {
                    Self::dispatch_locked(
                        &self.runtime,
                        &mut controller,
                        id,
                        &caller,
                        operation,
                        now,
                    )?;
                }
                _ => self.complete(id, CaptureError::code("invalid_request").payload())?,
            }
        }
        Ok(())
    }

    fn quiesce(&mut self) -> ModuleResult<bool> {
        let pending_result = self.poll_pending_closes();
        let callbacks_result = match self.controller.try_lock() {
            Ok(mut controller) => controller.quiesce().map_err(|error| error.message),
            Err(std::sync::TryLockError::WouldBlock) => Ok(false),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                Err("vibeshine_drm capture controller poisoned during shutdown".into())
            }
        };
        let retained_result = crate::leases::retirement_ready().map_err(str::to_owned);
        let snapshots_retired = crate::snapshot::retirement_ready().map_err(str::to_owned)?;
        let callbacks_retired = callbacks_result?;
        let retained_frames_retired = retained_result?;
        pending_result?;
        Ok(callbacks_retired
            && retained_frames_retired
            && snapshots_retired
            && self.pending_closes.is_empty())
    }
}

impl CaptureModule {
    fn try_controller(
        shared: &native::Shared<NativeBackend>,
    ) -> ModuleResult<Option<std::sync::MutexGuard<'_, Controller<NativeBackend>>>> {
        match shared.try_lock() {
            Ok(controller) => Ok(Some(controller)),
            Err(std::sync::TryLockError::WouldBlock) => Ok(None),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                Err("vibeshine_drm capture controller poisoned".into())
            }
        }
    }

    #[cfg(test)]
    fn lock_controller(
        &self,
    ) -> ModuleResult<std::sync::MutexGuard<'_, Controller<NativeBackend>>> {
        self.controller
            .lock()
            .map_err(|_| "vibeshine_drm capture controller poisoned".into())
    }

    #[cfg(test)]
    fn dispatch(
        &mut self,
        id: u64,
        caller: &str,
        operation: Operation,
        now: u64,
    ) -> ModuleResult<()> {
        if let Operation::Close { stream } = operation {
            return self.enqueue_close(id, caller, stream);
        }
        let mut controller = self.lock_controller()?;
        Self::dispatch_locked(&self.runtime, &mut controller, id, caller, operation, now)
    }

    fn enqueue_close(&mut self, id: u64, caller: &str, stream: u64) -> ModuleResult<()> {
        if self.pending_closes.len() >= MAX_PENDING_CLOSES {
            return self.complete(id, CaptureError::code("resource_exhausted").payload());
        }
        self.pending_closes.push(PendingClose {
            request: Some(id),
            caller: caller.into(),
            stream,
            cleanup_started: false,
            reply: None,
        });
        self.poll_pending_closes()
    }

    fn dispatch_locked(
        runtime: &RuntimeClient,
        controller: &mut Controller<NativeBackend>,
        id: u64,
        caller: &str,
        operation: Operation,
        now: u64,
    ) -> ModuleResult<()> {
        let reply = controller.handle(caller, operation, now);
        let (payload, created) = match reply {
            Ok(reply) => (reply.payload, reply.created),
            Err(error) => (error.payload(), None),
        };
        let completion = runtime.send(Command::Complete {
            request: id,
            payload,
        });
        if !matches!(completion, Ok(Response::Ok)) {
            controller.rollback(caller, created);
            if !matches!(&completion, Ok(Response::Error { code, .. }) if code == "request_ended" || code == "not_found")
            {
                return Err(format!(
                    "vibeshine_drm capture completion failed: {completion:?}"
                ));
            }
        }
        Ok(())
    }

    fn poll_pending_closes(&mut self) -> ModuleResult<()> {
        let mut index = 0;
        let mut failure = None;
        while index < self.pending_closes.len() {
            let pending = &mut self.pending_closes[index];
            let progress = Self::poll_close_target(&self.controller, pending)
                .and_then(|()| Self::settle_close_request(&self.runtime, pending));
            match progress {
                Ok(true) => {
                    self.pending_closes.remove(index);
                }
                Ok(false) => {
                    index += 1;
                }
                Err(error) => {
                    failure.get_or_insert(error);
                    index += 1;
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }

    fn poll_close_target(
        controller: &native::Shared<NativeBackend>,
        pending: &mut PendingClose,
    ) -> ModuleResult<()> {
        if pending.reply.is_some() {
            return Ok(());
        }
        let mut controller = match controller.try_lock() {
            Ok(controller) => controller,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(()),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err("vibeshine_drm capture controller poisoned during close".into());
            }
        };
        let closed = controller.close(&pending.caller, pending.stream);
        pending.cleanup_started = true;
        pending.reply = match closed {
            Ok(false) => None,
            Ok(true) => Some(serde_json::json!({"ok":true})),
            Err(error) => Some(error.payload()),
        };
        Ok(())
    }

    fn settle_close_request(
        runtime: &RuntimeClient,
        pending: &mut PendingClose,
    ) -> ModuleResult<bool> {
        let Some(request) = pending.request else {
            return Ok(pending.cleanup_started || pending.reply.is_some());
        };
        let checked = runtime.send(Command::CheckRequest { request })?;
        if matches!(&checked, Response::Error { code, .. } if code == "request_ended" || code == "not_found")
        {
            pending.request = None;
            return Ok(pending.cleanup_started || pending.reply.is_some());
        }
        if !matches!(checked, Response::Ok) {
            return Err(format!(
                "vibeshine_drm close request check failed: {checked:?}"
            ));
        }
        let Some(payload) = pending.reply.clone() else {
            return Ok(false);
        };
        let completed = runtime.send(Command::Complete { request, payload })?;
        if matches!(&completed, Response::Ok)
            || matches!(&completed, Response::Error { code, .. } if code == "request_ended" || code == "not_found")
        {
            Ok(true)
        } else {
            Err(format!(
                "vibeshine_drm close completion failed: {completed:?}"
            ))
        }
    }

    fn complete(&self, id: u64, payload: serde_json::Value) -> ModuleResult<()> {
        self.runtime.send(Command::Complete {
            request: id,
            payload,
        })?;
        Ok(())
    }
}

impl Drop for CaptureModule {
    fn drop(&mut self) {
        self.controller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .shutdown();
        self.log_queue.flush(&self.runtime);
    }
}

amaterasu_module_sdk::export_module!(CaptureModule);

#[cfg(test)]
mod test_support;
