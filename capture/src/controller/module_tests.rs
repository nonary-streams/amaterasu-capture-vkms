//! Actual module lifecycle fixtures; no native device or helper is opened.
use super::*;
use crate::{
    backend::NativeBackend,
    test_support::{FakeBackend, eventually, settings},
};
use amaterasu_module_sdk::{ManagedModule, RuntimeClient, abi::*, capture::SourcePreflightFactory};
use std::{
    ffi::c_void,
    sync::{Mutex, atomic::Ordering},
    time::Instant,
};

const VIDEO: &str = "amaterasu.video";

#[derive(Default)]
struct HostFixture {
    completed: Mutex<Vec<serde_json::Value>>,
    polls: Mutex<std::collections::VecDeque<crate::Response>>,
    ended: Mutex<std::collections::BTreeSet<u64>>,
    fail_next_completion: std::sync::atomic::AtomicBool,
}

impl HostFixture {
    fn runtime(self: &Arc<Self>) -> RuntimeClient {
        unsafe extern "C" fn invoke(
            context: *mut c_void,
            request: BorrowedBytes,
            reply: ReplySink,
        ) -> Status {
            // SAFETY: tests retain this shared fixture and the serialized request
            // throughout the synchronous call; each collection has its own lock.
            let host = unsafe { &*context.cast::<HostFixture>() };
            let command: crate::Command =
                serde_json::from_slice(unsafe { request.read() }.unwrap()).unwrap();
            let response = match command {
                crate::Command::Poll {} => host
                    .polls
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(crate::Response::Pending),
                crate::Command::CheckRequest { request }
                    if host.ended.lock().unwrap().contains(&request) =>
                {
                    crate::Response::Error {
                        code: "request_ended".into(),
                        message: "ended fixture caller".into(),
                    }
                }
                crate::Command::CheckRequest { .. } => crate::Response::Ok,
                crate::Command::Complete { request, payload } => {
                    if host.fail_next_completion.swap(false, Ordering::SeqCst) {
                        return STATUS_INTERNAL_ERROR;
                    }
                    host.completed
                        .lock()
                        .unwrap()
                        .push(serde_json::json!({"request":request,"payload":payload}));
                    crate::Response::Ok
                }
                _ => return STATUS_INTERNAL_ERROR,
            };
            // SAFETY: the reply sink borrows these initialized bytes until return.
            unsafe { reply.send(&serde_json::to_vec(&response).unwrap()) };
            STATUS_OK
        }
        // SAFETY: every test retains its Arc until after the module is destroyed.
        unsafe {
            RuntimeClient::from_endpoint(HostApi {
                header: AbiHeader::of::<HostApi>(),
                context: Arc::as_ptr(self).cast_mut().cast(),
                invoke,
            })
        }
    }
}

fn module(controller: Controller<FakeBackend>) -> crate::CaptureModule {
    unsafe extern "C" fn unused_host(
        _context: *mut c_void,
        _request: BorrowedBytes,
        _reply: ReplySink,
    ) -> Status {
        STATUS_INTERNAL_ERROR
    }

    // Native constructors only store empty probe/layout state. All existing
    // stream workers and descriptors below come from the synthetic backend.
    let controller = Arc::new(Mutex::new(Controller {
        backend: NativeBackend::new(),
        logger: controller.logger,
        settings: controller.settings,
        streams: controller.streams,
        closed: controller.closed,
        opened: controller.opened,
        last_availability: controller.last_availability,
        retiring_admissions: controller.retiring_admissions,
    }));
    let factory = crate::native::Factory::new(controller.clone());
    // SAFETY: both stable boxed factories and the shared metadata controller
    // outlive the wrapper; preflight cannot open native resources here.
    let preflight_factory = unsafe {
        SourcePreflightFactory::new(
            &factory.api,
            Arc::new(crate::VkmsPreflight(controller.clone())),
        )
    };
    // SAFETY: this stateless no-call endpoint outlives the fixture. Quiescence
    // releases already-owned resources without making runtime requests.
    let runtime = unsafe {
        RuntimeClient::from_endpoint(HostApi {
            header: AbiHeader::of::<HostApi>(),
            context: std::ptr::null_mut(),
            invoke: unused_host,
        })
    };
    let (_, log_queue) = runtime.log_emitter();
    crate::CaptureModule {
        runtime,
        log_queue,
        controller,
        _factory: factory,
        _preflight_factory: preflight_factory,
        epoch: Instant::now(),
        pending_closes: Vec::new(),
    }
}

fn opened_module(
    operation: &str,
) -> (
    crate::CaptureModule,
    Arc<crate::test_support::SourceScript>,
    u64,
) {
    let backend = FakeBackend::new();
    let script = backend.script.clone();
    let mut controller = Controller::new(backend, settings());
    let stream = controller
        .handle(
            VIDEO,
            Operation::Open {
                output: "kwin:Virtual-9".into(),
                operation_id: Some(operation.into()),
                constant_rate: None,
            },
            0,
        )
        .unwrap()
        .created
        .unwrap();
    (module(controller), script, stream)
}

#[test]
fn module_quiescence_waits_for_admitted_and_retained_native_frames() {
    let _serial = crate::leases::TEST_LEASE_CALLBACKS.lock().unwrap();
    let backend = FakeBackend::new();
    let script = backend.script.clone();
    let mut controller = Controller::new(backend, settings());
    let stream = controller
        .handle(
            VIDEO,
            Operation::Open {
                output: "kwin:Virtual-9".into(),
                operation_id: Some("shutdown".into()),
                constant_rate: None,
            },
            0,
        )
        .unwrap()
        .created
        .unwrap();
    let frames = controller.reader(VIDEO, stream).unwrap().frames;
    script.present(1, 1_000_000);
    assert!(eventually(|| frames
        .latest_presentation()
        .unwrap()
        .sequence
        == 1));
    frames.store_snapshot(script.export(2, 2_000_000, false), 2);
    let admitted = frames.admission.enter().unwrap();
    let lent = frames.lend().unwrap();
    let lease = lent.lease().unwrap();
    // SAFETY: the lent frame pins its context through this retain; exactly one
    // matching release occurs below after provider shutdown has begun.
    assert_eq!(unsafe { lease.retain.unwrap()(lease.context) }, STATUS_OK);
    frames.finish(lent, STATUS_OK);
    let mut module = module(controller);

    let shared = module.controller.clone();
    let held = shared.lock().unwrap();
    assert_eq!(module.quiesce(), Ok(false));
    assert_eq!(script.source_drops.load(Ordering::SeqCst), 0);
    drop(held);
    assert_eq!(module.quiesce(), Ok(false));
    assert!(eventually(
        || script.source_drops.load(Ordering::SeqCst) == 1
    ));
    assert!(frames.admission.enter().is_none());
    drop(admitted);
    assert_eq!(module.quiesce(), Ok(false));
    assert_eq!(script.open_descriptors(), 1);
    // SAFETY: the simulated asynchronous GPU read is complete; consume the
    // sole retained reference without transferring a completion fence.
    unsafe { lease.release.unwrap()(lease.context, -1) };
    assert_eq!(module.quiesce(), Ok(true));
    assert_eq!(script.open_descriptors(), 0);
    assert_eq!(module.quiesce(), Ok(true));
    assert_eq!(script.source_drops.load(Ordering::SeqCst), 1);
}

#[test]
fn module_quiescence_waits_for_a_retained_producer_lease() {
    let _serial = crate::leases::TEST_LEASE_CALLBACKS.lock().unwrap();
    let backend = FakeBackend::new();
    let script = backend.script.clone();
    let mut controller = Controller::new(backend, settings());
    let stream = controller
        .handle(
            VIDEO,
            Operation::Open {
                output: "kwin:Virtual-9".into(),
                operation_id: Some("producer-lease".into()),
                constant_rate: None,
            },
            0,
        )
        .unwrap()
        .created
        .unwrap();
    let frames = controller.reader(VIDEO, stream).unwrap().frames;
    frames.store_leased(script.export(1, 1_000_000, false), 1, 41);
    let lent = frames.lend().unwrap();
    let lease = lent.lease().unwrap();
    // SAFETY: the borrowed producer lease is live through retain, then held
    // until the matching completion release below.
    assert_eq!(unsafe { lease.retain.unwrap()(lease.context) }, STATUS_OK);
    frames.finish(lent, STATUS_OK);
    let mut module = module(controller);

    assert_eq!(module.quiesce(), Ok(false));
    assert!(eventually(
        || script.source_drops.load(Ordering::SeqCst) == 1
    ));
    assert_eq!(script.open_descriptors(), 1);
    // SAFETY: consume exactly the retained producer lease after its GPU work.
    unsafe { lease.release.unwrap()(lease.context, -1) };
    assert_eq!(module.quiesce(), Ok(true));
    assert_eq!(script.open_descriptors(), 0);
}

#[test]
fn quarantine_keeps_module_retirement_failed() {
    const CHILD: &str = "AMATERASU_VKMS_QUARANTINE_QUIESCE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "controller::module_tests::quarantine_keeps_module_retirement_failed",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let _serial = crate::leases::TEST_LEASE_CALLBACKS.lock().unwrap();
    let backend = FakeBackend::new();
    let script = backend.script.clone();
    let frames = StreamFrames::new(41, 0);
    frames.store_snapshot(script.export(1, 0, false), 1);
    let lent = frames.lend().unwrap();
    let lease = lent.lease().unwrap();
    // SAFETY: the borrowed frame is live through retain. The intentionally
    // invalid completion value exercises the existing process-lifetime fence.
    unsafe {
        assert_eq!(lease.retain.unwrap()(lease.context), STATUS_OK);
        lease.release.unwrap()(lease.context, -2);
    }
    frames.finish(lent, STATUS_OK);
    drop(frames);
    let mut module = module(Controller::new(backend, settings()));

    let expected = Err("VKMS frame retirement is blocked by quarantined native storage".into());
    assert_eq!(module.quiesce(), expected);
    assert_eq!(module.quiesce(), expected);
    assert_eq!(script.open_descriptors(), 1);
}

#[test]
fn broker_close_remains_pending_until_the_exact_source_destructor_retires() {
    let _serial = crate::leases::TEST_LEASE_CALLBACKS.lock().unwrap();
    use crate::test_support::SourceDropGate;
    use serde_json::{Value, json};
    use std::time::Duration;

    unsafe extern "C" fn host(
        context: *mut c_void,
        request: BorrowedBytes,
        reply: ReplySink,
    ) -> Status {
        // SAFETY: the shared collector and request bytes live through every
        // synchronous call and the collector is protected by its own mutex.
        let commands = unsafe { &*context.cast::<Mutex<Vec<Value>>>() };
        let command: crate::Command =
            serde_json::from_slice(unsafe { request.read() }.unwrap()).unwrap();
        let response = match command {
            crate::Command::Complete { request, payload } => {
                commands
                    .lock()
                    .unwrap()
                    .push(json!({"request":request,"payload":payload}));
                crate::Response::Ok
            }
            crate::Command::CheckRequest { .. } => crate::Response::Ok,
            crate::Command::Poll {} => crate::Response::Pending,
            _ => return STATUS_INTERNAL_ERROR,
        };
        // SAFETY: reply borrows these initialized bytes only until it returns.
        unsafe { reply.send(&serde_json::to_vec(&response).unwrap()) };
        STATUS_OK
    }

    let completed = Arc::new(Mutex::new(Vec::<Value>::new()));
    let backend = FakeBackend::new();
    let script = backend.script.clone();
    let gate = Arc::new(SourceDropGate::default());
    *script.source_drop_gate.lock().unwrap() = Some(gate.clone());
    let mut controller = Controller::new(backend, settings());
    let stream = controller
        .handle(
            VIDEO,
            Operation::Open {
                output: "kwin:Virtual-9".into(),
                operation_id: Some("blocked-destructor".into()),
                constant_rate: None,
            },
            0,
        )
        .unwrap()
        .created
        .unwrap();
    let mut module = module(controller);
    // SAFETY: the collector is retained until after the module is dropped.
    module.runtime = unsafe {
        RuntimeClient::from_endpoint(HostApi {
            header: AbiHeader::of::<HostApi>(),
            context: Arc::as_ptr(&completed).cast_mut().cast(),
            invoke: host,
        })
    };

    let started = Instant::now();
    module
        .dispatch(71, VIDEO, Operation::Close { stream }, 0)
        .unwrap();
    assert!(started.elapsed() < Duration::from_millis(200));
    assert!(completed.lock().unwrap().is_empty());
    assert!(eventually(|| gate.entered()));
    assert_eq!(script.source_drops.load(Ordering::SeqCst), 0);
    assert_eq!(module.quiesce(), Ok(false));
    module
        .dispatch(72, VIDEO, Operation::Close { stream }, 0)
        .unwrap();
    assert!(completed.lock().unwrap().is_empty());

    gate.release();
    assert!(eventually(|| {
        module.tick().unwrap();
        completed.lock().unwrap().len() == 2
    }));
    let replies = completed.lock().unwrap();
    assert_eq!(
        replies
            .iter()
            .filter(|reply| reply["request"] == 71)
            .count(),
        1
    );
    assert_eq!(
        replies
            .iter()
            .filter(|reply| reply["request"] == 72)
            .count(),
        1
    );
    assert!(replies.iter().all(|reply| reply["payload"]["ok"] == true));
    assert_eq!(script.source_drops.load(Ordering::SeqCst), 1);
    assert_eq!(module.quiesce(), Ok(true));
    drop(replies);
    drop(module);
}

#[test]
fn failed_source_retirement_settles_close_without_starving_later_status() {
    let _serial = crate::leases::TEST_LEASE_CALLBACKS.lock().unwrap();
    let host = Arc::new(HostFixture::default());
    let backend = FakeBackend::new();
    backend
        .script
        .panic_on_source_drop
        .store(true, Ordering::SeqCst);
    let mut controller = Controller::new(backend, settings());
    let stream = controller
        .handle(
            VIDEO,
            Operation::Open {
                output: "kwin:Virtual-9".into(),
                operation_id: Some("failed-source".into()),
                constant_rate: None,
            },
            0,
        )
        .unwrap()
        .created
        .unwrap();
    let mut module = module(controller);
    module.runtime = host.runtime();
    module
        .dispatch(81, VIDEO, Operation::Close { stream }, 0)
        .unwrap();
    assert!(eventually(|| {
        let _ = module.tick();
        !host.completed.lock().unwrap().is_empty()
    }));
    assert_eq!(
        host.completed.lock().unwrap()[0]["payload"]["error"]["code"],
        "capture_retirement_failed"
    );
    host.polls
        .lock()
        .unwrap()
        .push_back(crate::Response::Request {
            id: 82,
            caller: VIDEO.into(),
            initiator: VIDEO.into(),
            service: crate::SERVICE.into(),
            payload: serde_json::json!({"action":"status"}),
            remaining_ms: 1000,
        });

    module.tick().unwrap();

    let completed = host.completed.lock().unwrap();
    assert_eq!(completed[1]["request"], 82);
    assert_eq!(completed[1]["payload"]["ok"], true);
    assert_eq!(
        completed[1]["payload"]["streams"][0]["retirement_failed"],
        true
    );
    assert!(module.quiesce().is_err());
    assert!(
        module
            .controller
            .lock()
            .unwrap()
            .streams
            .contains_key(&stream)
    );
    drop(completed);
    drop(module);
}

#[test]
fn cancelled_close_request_does_not_abandon_the_owned_source() {
    let _serial = crate::leases::TEST_LEASE_CALLBACKS.lock().unwrap();
    let host = Arc::new(HostFixture::default());
    let (mut module, script, stream) = opened_module("cancelled-close");
    let gate = Arc::new(crate::test_support::SourceDropGate::default());
    *script.source_drop_gate.lock().unwrap() = Some(gate.clone());
    module.runtime = host.runtime();
    module
        .dispatch(91, VIDEO, Operation::Close { stream }, 0)
        .unwrap();
    assert!(eventually(|| gate.entered()));
    host.ended.lock().unwrap().insert(91);

    module.tick().unwrap();

    assert!(module.pending_closes.is_empty());
    assert_eq!(module.quiesce(), Ok(false));
    assert_eq!(script.source_drops.load(Ordering::SeqCst), 0);
    gate.release();
    assert!(eventually(|| module.quiesce() == Ok(true)));
    assert_eq!(script.source_drops.load(Ordering::SeqCst), 1);
    assert!(host.completed.lock().unwrap().is_empty());
    drop(module);
}

#[test]
fn uncertain_close_completion_retains_the_exact_request_without_repeating_cleanup() {
    let _serial = crate::leases::TEST_LEASE_CALLBACKS.lock().unwrap();
    let host = Arc::new(HostFixture::default());
    let (mut module, script, stream) = opened_module("uncertain-completion");
    let gate = Arc::new(crate::test_support::SourceDropGate::default());
    *script.source_drop_gate.lock().unwrap() = Some(gate.clone());
    module.runtime = host.runtime();
    module
        .dispatch(101, VIDEO, Operation::Close { stream }, 0)
        .unwrap();
    assert!(eventually(|| gate.entered()));
    host.fail_next_completion.store(true, Ordering::SeqCst);
    gate.release();
    assert!(eventually(|| module.poll_pending_closes().is_err()));

    assert_eq!(module.pending_closes.len(), 1);
    assert_eq!(module.pending_closes[0].request, Some(101));
    assert!(host.completed.lock().unwrap().is_empty());
    // Once ready, the exact reply survives even if later native lookup no
    // longer has its tombstone; transport retry must not recompute the result.
    module.controller.lock().unwrap().closed.remove(&stream);
    assert_eq!(
        module
            .controller
            .lock()
            .unwrap()
            .close(VIDEO, stream)
            .unwrap_err()
            .code,
        "not_found"
    );
    assert_eq!(module.quiesce(), Ok(true));
    assert!(module.pending_closes.is_empty());
    assert_eq!(host.completed.lock().unwrap().len(), 1);
    assert_eq!(host.completed.lock().unwrap()[0]["request"], 101);
    assert_eq!(host.completed.lock().unwrap()[0]["payload"]["ok"], true);
    assert_eq!(module.quiesce(), Ok(true));
    assert_eq!(script.source_drops.load(Ordering::SeqCst), 1);
    drop(module);
}

#[test]
fn accepted_close_survives_cancellation_while_the_controller_is_busy() {
    use std::{sync::mpsc, time::Duration};
    let _serial = crate::leases::TEST_LEASE_CALLBACKS.lock().unwrap();
    let host = Arc::new(HostFixture::default());
    let (mut module, script, stream) = opened_module("busy-controller-close");
    module.runtime = host.runtime();
    let shared = module.controller.clone();
    let (ready, entered) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _held = shared.lock().unwrap();
        ready.send(()).unwrap();
        let _ = released.recv_timeout(Duration::from_secs(1));
    });
    entered.recv_timeout(Duration::from_secs(1)).unwrap();
    let started = Instant::now();

    module
        .dispatch(111, VIDEO, Operation::Close { stream }, 0)
        .unwrap();

    assert!(started.elapsed() < Duration::from_millis(200));
    host.ended.lock().unwrap().insert(111);
    assert_eq!(module.quiesce(), Ok(false));
    assert!(!module.pending_closes.is_empty());
    assert_eq!(script.source_drops.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    holder.join().unwrap();
    assert!(eventually(|| module.quiesce() == Ok(true)));
    assert_eq!(script.source_drops.load(Ordering::SeqCst), 1);
    assert!(host.completed.lock().unwrap().is_empty());
    drop(module);
}

#[test]
fn tick_leaves_requests_queued_while_preflight_holds_controller() {
    use std::{sync::mpsc, time::Duration};
    let host = Arc::new(HostFixture::default());
    let mut module = module(Controller::new(FakeBackend::new(), settings()));
    module.runtime = host.runtime();
    host.polls
        .lock()
        .unwrap()
        .push_back(crate::Response::Request {
            id: 121,
            caller: VIDEO.into(),
            initiator: VIDEO.into(),
            service: crate::SERVICE.into(),
            payload: serde_json::json!({"action":"status"}),
            remaining_ms: 1000,
        });
    let shared = module.controller.clone();
    let (ready, entered) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let _guard = shared.lock().unwrap();
        ready.send(()).unwrap();
        let _ = released.recv_timeout(Duration::from_secs(2));
    });
    entered.recv_timeout(Duration::from_secs(1)).unwrap();
    module.tick().unwrap();
    let queued = host.polls.lock().unwrap().len();
    let _ = release.send(());
    holder.join().unwrap();
    assert_eq!(queued, 1);
    module.tick().unwrap();
    assert!(host.polls.lock().unwrap().is_empty());
    assert_eq!(host.completed.lock().unwrap()[0]["request"], 121);
}

#[test]
fn close_waits_for_privileged_helper_retirement_after_the_local_worker_has_joined() {
    use std::sync::atomic::AtomicBool;
    struct Receipt(AtomicBool);
    impl crate::retirement::Retirement for Receipt {
        fn poll_retired(&self) -> std::io::Result<bool> {
            Ok(self.0.load(Ordering::SeqCst))
        }
    }
    let _serial = crate::leases::TEST_LEASE_CALLBACKS.lock().unwrap();
    let host = Arc::new(HostFixture::default());
    let backend = FakeBackend::new();
    let script = backend.script.clone();
    let receipt = Arc::new(Receipt(AtomicBool::new(false)));
    *script.retirement.lock().unwrap() = Some(receipt.clone());
    let mut controller = Controller::new(backend, settings());
    let stream = controller
        .handle(
            VIDEO,
            Operation::Open {
                output: "kwin:Virtual-9".into(),
                operation_id: Some("privileged-reaper".into()),
                constant_rate: None,
            },
            0,
        )
        .unwrap()
        .created
        .unwrap();
    let mut module = module(controller);
    module.runtime = host.runtime();
    module
        .dispatch(121, VIDEO, Operation::Close { stream }, 0)
        .unwrap();
    assert!(eventually(
        || script.source_drops.load(Ordering::SeqCst) == 1
    ));
    assert!(eventually(|| {
        module.tick().unwrap();
        module
            .controller
            .lock()
            .unwrap()
            .streams
            .get(&stream)
            .is_none_or(|stream| stream.presenter.is_finished())
    }));

    assert!(host.completed.lock().unwrap().is_empty());
    assert_eq!(module.quiesce(), Ok(false));
    receipt.0.store(true, Ordering::SeqCst);
    assert!(eventually(|| module.quiesce() == Ok(true)));
    assert_eq!(host.completed.lock().unwrap().len(), 1);
    assert_eq!(host.completed.lock().unwrap()[0]["request"], 121);
    assert_eq!(host.completed.lock().unwrap()[0]["payload"]["ok"], true);
    drop(module);
}

#[test]
fn a_failed_privileged_receipt_remains_a_cleanup_error() {
    struct FailedReceipt;
    impl crate::retirement::Retirement for FailedReceipt {
        fn poll_retired(&self) -> std::io::Result<bool> {
            Err(std::io::Error::other("synthetic privileged reap failure"))
        }
    }
    let _serial = crate::leases::TEST_LEASE_CALLBACKS.lock().unwrap();
    let host = Arc::new(HostFixture::default());
    let backend = FakeBackend::new();
    *backend.script.retirement.lock().unwrap() = Some(Arc::new(FailedReceipt));
    let mut controller = Controller::new(backend, settings());
    let stream = controller
        .handle(
            VIDEO,
            Operation::Open {
                output: "kwin:Virtual-9".into(),
                operation_id: Some("failed-privileged-reaper".into()),
                constant_rate: None,
            },
            0,
        )
        .unwrap()
        .created
        .unwrap();
    let mut module = module(controller);
    module.runtime = host.runtime();
    module
        .dispatch(131, VIDEO, Operation::Close { stream }, 0)
        .unwrap();
    assert!(eventually(|| {
        module.tick().unwrap();
        !host.completed.lock().unwrap().is_empty()
    }));
    assert_eq!(host.completed.lock().unwrap()[0]["payload"]["ok"], false);
    assert_eq!(
        host.completed.lock().unwrap()[0]["payload"]["error"]["code"],
        "capture_retirement_failed"
    );
    assert!(
        module
            .quiesce()
            .unwrap_err()
            .contains("synthetic privileged reap failure")
    );
    assert!(module.quiesce().is_err());
    assert!(
        module
            .controller
            .lock()
            .unwrap()
            .streams
            .contains_key(&stream)
    );
    drop(module);
}
