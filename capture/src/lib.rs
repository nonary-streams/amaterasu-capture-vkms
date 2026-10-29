//! DMA-BUF capture of owned `kwin:Virtual-N` outputs on vibeshine_drm.
//!
//! Frames are the driver's completed primary-plane scanouts, exported as
//! DMA-BUFs by the separately installed `amaterasu-vkms-frame-helper`, which
//! alone holds CAP_SYS_ADMIN. Optional private GPU copies support immutable
//! frame leases. This module never holds that capability.
//! Native modules are trusted code; runtime API grants are not a native-code
//! sandbox. The provider exists only on Linux.
#![cfg(target_os = "linux")]

mod admission;
mod backend;
mod controller;
mod discovery;
mod frames;
mod geometry;
mod leases;
mod model;
mod native;
mod output_name;
mod presenter;
mod snapshot;

use amaterasu_module_sdk::{Command, ManagedModule, ModuleResult, Response, RuntimeClient};
use backend::NativeBackend;
use controller::Controller;
use model::{CaptureError, Operation, Settings};
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

const SERVICE: &str = "amaterasu.capture-vkms/1";
const REQUESTS_PER_TICK: usize = 8;

struct CaptureModule {
    runtime: RuntimeClient,
    controller: native::Shared<NativeBackend>,
    _factory: Box<native::Factory<NativeBackend>>,
    epoch: Instant,
}

impl ManagedModule for CaptureModule {
    fn start(runtime: RuntimeClient) -> ModuleResult<Self> {
        let Response::Result { payload } = runtime.send(Command::GetSettings {})? else {
            return Err("vibeshine_drm capture settings unavailable".into());
        };
        let settings: Settings =
            serde_json::from_value(payload).map_err(|error| error.to_string())?;
        settings.validate().map_err(|error| error.message)?;
        let controller = Arc::new(Mutex::new(Controller::new(NativeBackend::new(), settings)));
        let factory = native::Factory::new(controller.clone());
        runtime.require_success(Command::Register {
            service: SERVICE.into(),
        })?;
        // SAFETY: the boxed factory stays at a stable address until this module
        // is destroyed; every binding retains its own shared controller.
        unsafe { runtime.register_native(SERVICE, &factory.api) }?;
        Ok(Self {
            runtime,
            controller,
            _factory: factory,
            epoch: Instant::now(),
        })
    }

    fn tick(&mut self) -> ModuleResult<()> {
        let now = self.epoch.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        self.lock_controller()?.refresh(now);
        for _ in 0..REQUESTS_PER_TICK {
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
                Ok(operation) if service == SERVICE => {
                    self.dispatch(id, &caller, operation, now)?
                }
                _ => self.complete(id, CaptureError::code("invalid_request").payload())?,
            }
        }
        Ok(())
    }
}

impl CaptureModule {
    fn lock_controller(
        &self,
    ) -> ModuleResult<std::sync::MutexGuard<'_, Controller<NativeBackend>>> {
        self.controller
            .lock()
            .map_err(|_| "vibeshine_drm capture controller poisoned".into())
    }

    fn dispatch(
        &mut self,
        id: u64,
        caller: &str,
        operation: Operation,
        now: u64,
    ) -> ModuleResult<()> {
        let reply = self.lock_controller()?.handle(caller, operation, now);
        let (payload, created) = match reply {
            Ok(reply) => (reply.payload, reply.created),
            Err(error) => (error.payload(), None),
        };
        let completion = self.runtime.send(Command::Complete {
            request: id,
            payload,
        });
        if !matches!(completion, Ok(Response::Ok)) {
            self.lock_controller()?.rollback(caller, created);
            if !matches!(&completion, Ok(Response::Error { code, .. }) if code == "request_ended" || code == "not_found")
            {
                return Err(format!(
                    "vibeshine_drm capture completion failed: {completion:?}"
                ));
            }
        }
        Ok(())
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
    }
}

amaterasu_module_sdk::export_module!(CaptureModule);

#[cfg(test)]
mod test_support;
