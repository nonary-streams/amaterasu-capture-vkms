//! Wait for compositor geometry without blocking the host or replaying effects.
use crate::{
    Command, Response,
    backend::Backend,
    controller::{Controller, Reply},
    model::{CaptureError, CaptureResult, Operation},
};
use amaterasu_module_sdk::{ModuleResult, RuntimeClient};
use std::collections::VecDeque;

const MAX_PENDING_OPENS: usize = 32;
const OPENS_PER_TICK: usize = 8;
const GEOMETRY_RETRY_MS: u64 = 50;
// The broker's capture-open request has a five-second deadline. Settle a
// persistently inconsistent layout before that deadline so the caller sees
// the actual reason and can clean up its other resources.
const GEOMETRY_DEADLINE_MS: u64 = 4_000;

struct PendingOpen {
    request: u64,
    caller: String,
    operation: Operation,
    deadline: u64,
    next_attempt: u64,
    last_geometry_error: Option<CaptureError>,
}

#[derive(Default)]
pub(crate) struct PendingOpens {
    entries: VecDeque<PendingOpen>,
}

enum Progress {
    Waiting,
    Retry(String),
    Finished(ModuleResult<()>),
}

impl PendingOpens {
    pub(crate) fn enqueue(
        &mut self,
        request: u64,
        caller: &str,
        operation: Operation,
        now: u64,
    ) -> bool {
        debug_assert!(matches!(operation, Operation::Open { .. }));
        if self.entries.len() >= MAX_PENDING_OPENS {
            return false;
        }
        self.entries.push_back(PendingOpen {
            request,
            caller: caller.into(),
            operation,
            deadline: now.saturating_add(GEOMETRY_DEADLINE_MS),
            next_attempt: now,
            last_geometry_error: None,
        });
        true
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn poll<B: Backend>(
        &mut self,
        runtime: &RuntimeClient,
        controller: &mut Controller<B>,
        now: u64,
        stopping: bool,
    ) -> ModuleResult<()> {
        let mut failure = None;
        // Rotate the queue so a busy or cancelled request cannot starve later
        // owners, and leave time for closes and other module requests.
        for _ in 0..self.entries.len().min(OPENS_PER_TICK) {
            let mut pending = self.entries.pop_front().expect("bounded queue length");
            match pending.progress(runtime, controller, now, stopping) {
                Progress::Waiting => self.entries.push_back(pending),
                Progress::Retry(error) => {
                    failure.get_or_insert(error);
                    self.entries.push_back(pending);
                }
                Progress::Finished(result) => {
                    if let Err(error) = result {
                        failure.get_or_insert(error);
                    }
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

impl PendingOpen {
    fn progress<B: Backend>(
        &mut self,
        runtime: &RuntimeClient,
        controller: &mut Controller<B>,
        now: u64,
        stopping: bool,
    ) -> Progress {
        if !stopping && now < self.next_attempt {
            return Progress::Waiting;
        }
        match runtime.send(Command::CheckRequest {
            request: self.request,
        }) {
            Ok(Response::Ok) => {}
            Ok(Response::Error { code, .. }) if code == "request_ended" || code == "not_found" => {
                return Progress::Finished(Ok(()));
            }
            response => {
                return Progress::Retry(format!("capture open request check failed: {response:?}"));
            }
        }

        if stopping || now >= self.deadline {
            let error = if stopping {
                CaptureError::code("capture_stopping")
            } else {
                self.last_geometry_error
                    .take()
                    .unwrap_or_else(|| CaptureError::code("capture_open_timeout"))
            };
            if error.code == "output_geometry_unavailable" {
                eprintln!(
                    "amaterasu capture geometry did not settle: {}",
                    error.message
                );
            }
            return Progress::Finished(complete_reply(
                runtime,
                controller,
                self.request,
                &self.caller,
                Err(error),
            ));
        }

        match controller.handle(&self.caller, self.operation.clone(), now) {
            // This error is produced before a helper, stream, or native lease
            // is created. Only this definite no-effect outcome may be retried.
            Err(error) if error.code == "output_geometry_unavailable" => {
                self.last_geometry_error = Some(error);
                self.next_attempt = now.saturating_add(GEOMETRY_RETRY_MS);
                Progress::Waiting
            }
            reply => Progress::Finished(complete_reply(
                runtime,
                controller,
                self.request,
                &self.caller,
                reply,
            )),
        }
    }
}

pub(crate) fn complete_reply<B: Backend>(
    runtime: &RuntimeClient,
    controller: &mut Controller<B>,
    request: u64,
    caller: &str,
    reply: CaptureResult<Reply>,
) -> ModuleResult<()> {
    let (payload, created) = match reply {
        Ok(reply) => (reply.payload, reply.created),
        Err(error) => (error.payload(), None),
    };
    let completion = runtime.send(Command::Complete { request, payload });
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
