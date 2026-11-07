//! Keep process ownership until the operating system has actually reaped it.
use std::{
    io,
    process::{Child, ExitStatus},
    sync::{
        Arc, Mutex, TryLockError,
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Default)]
struct ProcessStatus {
    retired: bool,
    failure: Option<(io::ErrorKind, String)>,
    exit: Option<ExitStatus>,
}

/// Owns the local reaper worker as well as its exact child retirement outcome.
/// Retain this before helper setup. Polling never waits; last-owner Drop joins
/// the reaper, so code and process ownership cannot outlive provider unload.
pub struct ProcessRetirementReceipt {
    status: Arc<Mutex<ProcessStatus>>,
    stop: SyncSender<()>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl ProcessRetirementReceipt {
    pub(super) fn prepare() -> io::Result<(Arc<Self>, SyncSender<Child>)> {
        let status = Arc::new(Mutex::new(ProcessStatus::default()));
        let observed = status.clone();
        let (stop, stopped) = mpsc::sync_channel(1);
        let (adopt, process) = mpsc::sync_channel(1);
        // Create the retirement owner before allocating any native process.
        let worker = thread::Builder::new()
            .name("vkms-helper-reaper".into())
            .spawn(move || {
                reap_owned_process(process, stopped, &observed);
            })?;
        Ok((
            Arc::new(Self {
                status,
                stop,
                worker: Mutex::new(Some(worker)),
            }),
            adopt,
        ))
    }

    pub(super) fn begin(&self) {
        let _ = self.stop.try_send(());
    }

    pub(super) fn observed_exit(&self) -> Option<ExitStatus> {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .exit
    }

    pub fn poll_retired(&self) -> io::Result<bool> {
        let mut worker = match self.worker.try_lock() {
            Ok(worker) => worker,
            Err(TryLockError::WouldBlock) => return Ok(false),
            Err(TryLockError::Poisoned(_)) => {
                return Err(io::Error::other("helper reaper lock poisoned"));
            }
        };
        if worker.as_ref().is_some_and(JoinHandle::is_finished)
            && worker.take().unwrap().join().is_err()
        {
            record_process_failure(
                &self.status,
                &io::Error::other("helper reaper thread panicked"),
            );
        }
        let status = match self.status.try_lock() {
            Ok(status) => status,
            Err(TryLockError::WouldBlock) => return Ok(false),
            Err(TryLockError::Poisoned(_)) => {
                return Err(io::Error::other("helper reaper status poisoned"));
            }
        };
        if let Some((kind, message)) = &status.failure {
            return Err(io::Error::new(*kind, message.clone()));
        }
        Ok(worker.is_none() && status.retired)
    }
}

impl Drop for ProcessRetirementReceipt {
    fn drop(&mut self) {
        self.begin();
        if let Some(worker) = self
            .worker
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            && worker.join().is_err()
        {
            eprintln!("frame helper reaper thread panicked");
        }
    }
}

fn record_process_failure(status: &Mutex<ProcessStatus>, error: &io::Error) {
    let mut status = status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if status.failure.is_none() {
        status.failure = Some((error.kind(), error.to_string()));
    }
}

struct OwnedProcess {
    child: Child,
    identity_lost: bool,
    exit: Option<ExitStatus>,
}

impl ReapProcess for OwnedProcess {
    fn reaped(&mut self) -> io::Result<bool> {
        if self.identity_lost {
            return Err(io::Error::from_raw_os_error(libc::ECHILD));
        }
        match self.child.try_wait() {
            Ok(exit) => {
                self.exit = exit;
                Ok(exit.is_some())
            }
            Err(error) => {
                self.identity_lost = error.raw_os_error() == Some(libc::ECHILD);
                Err(error)
            }
        }
    }
    fn terminate(&mut self) -> io::Result<()> {
        if self.identity_lost {
            return Err(io::Error::from_raw_os_error(libc::ECHILD));
        }
        self.child.kill()
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if let Err(error) = retire(self, Duration::ZERO, &mut |_| {}) {
            eprintln!("helper final retirement failed: {error}");
        }
    }
}

fn reap_owned_process(
    process: Receiver<Child>,
    stopped: Receiver<()>,
    status: &Mutex<ProcessStatus>,
) {
    let Ok(child) = process.recv() else {
        status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retired = true;
        return;
    };
    let mut child = OwnedProcess {
        child,
        identity_lost: false,
        exit: None,
    };
    loop {
        match child.reaped() {
            Ok(true) => {
                let mut status = status
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                status.exit = child.exit;
                status.retired = true;
                return;
            }
            Ok(false) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                record_process_failure(status, &error);
                break;
            }
        }
        match stopped.recv_timeout(Duration::from_millis(10)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    let result = retire(&mut child, Duration::from_millis(50), &mut |error| {
        record_process_failure(status, error)
    });
    if let Err(error) = result {
        record_process_failure(status, &error);
    }
    let mut status = status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    status.retired = true;
}

/// Retires a child on its dedicated owner thread. The caller must first close
/// the protocol channel. A deadline starts termination; it never proves exit.
/// Errors are reported immediately, ownership is retained and reaping continues.
/// Even after eventual reaping, an observed syscall failure remains an error.
pub fn retire_child(
    child: &mut Child,
    grace: Duration,
    mut report: impl FnMut(&io::Error),
) -> io::Result<()> {
    retire(child, grace, &mut report)
}

trait ReapProcess {
    fn reaped(&mut self) -> io::Result<bool>;
    fn terminate(&mut self) -> io::Result<()>;
}

impl ReapProcess for Child {
    fn reaped(&mut self) -> io::Result<bool> {
        self.try_wait().map(|status| status.is_some())
    }
    fn terminate(&mut self) -> io::Result<()> {
        self.kill()
    }
}

#[derive(Default)]
struct RetirementProgress {
    terminated: bool,
    identity_lost: bool,
    failure: Option<io::Error>,
}

fn retire(
    process: &mut impl ReapProcess,
    grace: Duration,
    report: &mut impl FnMut(&io::Error),
) -> io::Result<()> {
    let deadline = Instant::now() + grace;
    let mut progress = RetirementProgress::default();
    loop {
        if poll_process_retirement(process, &mut progress, Instant::now() >= deadline, report) {
            return progress.failure.map_or(Ok(()), Err);
        }
        // ECHILD loses PID ownership, not merely readiness. Never signal or
        // wait on that reusable numeric identity again. Keep the unresolved
        // owner and its code loaded; only external recovery can resolve it.
        thread::sleep(if progress.identity_lost {
            Duration::from_secs(1)
        } else {
            Duration::from_millis(2)
        });
    }
}

fn poll_process_retirement(
    process: &mut impl ReapProcess,
    progress: &mut RetirementProgress,
    terminate: bool,
    report: &mut impl FnMut(&io::Error),
) -> bool {
    if progress.identity_lost {
        return false;
    }
    match process.reaped() {
        Ok(true) => return true,
        Ok(false) => {}
        Err(error) if error.kind() == io::ErrorKind::Interrupted => return false,
        Err(error) => {
            progress.identity_lost = error.raw_os_error() == Some(libc::ECHILD);
            remember_failure(error, &mut progress.failure, report);
            return false;
        }
    }
    if terminate && !progress.terminated {
        match process.terminate() {
            Ok(()) => progress.terminated = true,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                // Resolve an exit racing kill only from positive reap proof.
                match process.reaped() {
                    Ok(true) => return true,
                    Err(wait) if wait.raw_os_error() == Some(libc::ECHILD) => {
                        progress.identity_lost = true;
                    }
                    _ => {}
                }
                remember_failure(error, &mut progress.failure, report);
            }
        }
    }
    false
}

fn remember_failure(
    error: io::Error,
    failure: &mut Option<io::Error>,
    report: &mut impl FnMut(&io::Error),
) {
    if failure.is_none() {
        report(&error);
        *failure = Some(error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    struct FakeProcess {
        waits: VecDeque<io::Result<bool>>,
        kills: VecDeque<io::Result<()>>,
        reaped: bool,
        attempts: usize,
    }
    impl ReapProcess for FakeProcess {
        fn reaped(&mut self) -> io::Result<bool> {
            let result = self
                .waits
                .pop_front()
                .expect("retirement must retain ownership until final reap");
            if matches!(result, Ok(true)) {
                self.reaped = true;
            }
            result
        }
        fn terminate(&mut self) -> io::Result<()> {
            self.attempts += 1;
            self.kills.pop_front().expect("unexpected kill")
        }
    }
    #[test]
    fn kill_failure_and_wait_failure_never_release_unreaped_process() {
        let mut process = FakeProcess {
            waits: VecDeque::from([
                Err(io::Error::from(io::ErrorKind::PermissionDenied)),
                Ok(false),
                Ok(false),
                Ok(false),
                Ok(true),
            ]),
            kills: VecDeque::from([
                Err(io::Error::from(io::ErrorKind::PermissionDenied)),
                Ok(()),
            ]),
            reaped: false,
            attempts: 0,
        };
        let mut reported = 0;
        let result = retire(&mut process, Duration::ZERO, &mut |_| {
            reported += 1;
        });
        assert!(result.is_err());
        assert!(process.reaped);
        assert_eq!(process.attempts, 2);
        assert_eq!(reported, 1);
    }
    #[test]
    fn already_reaped_process_does_not_need_a_kill() {
        let mut process = FakeProcess {
            waits: VecDeque::from([Ok(true)]),
            kills: VecDeque::new(),
            reaped: false,
            attempts: 0,
        };
        assert!(retire(&mut process, Duration::ZERO, &mut |_| panic!("no error")).is_ok());
        assert_eq!(process.attempts, 0);
    }
    #[test]
    fn lost_child_identity_never_signals_a_reused_numeric_pid() {
        let mut process = FakeProcess {
            waits: VecDeque::from([Err(io::Error::from_raw_os_error(libc::ECHILD))]),
            kills: VecDeque::new(),
            reaped: false,
            attempts: 0,
        };
        let mut progress = RetirementProgress::default();
        let mut failures = 0;
        assert!(!poll_process_retirement(
            &mut process,
            &mut progress,
            true,
            &mut |_| {
                failures += 1;
            }
        ));
        assert!(progress.identity_lost);
        assert!(!poll_process_retirement(
            &mut process,
            &mut progress,
            true,
            &mut |_| {
                failures += 1;
            }
        ));
        assert_eq!(process.attempts, 0);
        assert_eq!(failures, 1);
        assert!(!process.reaped);
    }

    #[test]
    fn interrupted_wait_and_exit_racing_kill_are_not_false_failures() {
        let mut process = FakeProcess {
            waits: VecDeque::from([
                Err(io::Error::from(io::ErrorKind::Interrupted)),
                Ok(false),
                Ok(true),
            ]),
            kills: VecDeque::from([Err(io::Error::from(io::ErrorKind::InvalidInput))]),
            reaped: false,
            attempts: 0,
        };
        assert!(
            retire(&mut process, Duration::ZERO, &mut |_| panic!(
                "exact reap resolves raced kill"
            ))
            .is_ok()
        );
        assert!(process.reaped);
        assert_eq!(process.attempts, 1);
    }

    #[test]
    fn failed_setup_drop_signals_reaper_without_waiting_on_host_thread() {
        let (retirement, adoption) = ProcessRetirementReceipt::prepare().unwrap();
        let (frame, _peer) = crate::channel::SeqpacketChannel::pair().unwrap();
        let process = super::super::HelperProcess {
            client: Some(crate::client::HelperClient::new(frame)),
            retirement: retirement.clone(),
        };
        let started = Instant::now();
        drop(process);
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(
            !retirement.poll_retired().unwrap(),
            "live adoption owner keeps retirement pending"
        );
        drop(adoption); // No native process was ever allocated in this fixture.
        let deadline = Instant::now() + Duration::from_secs(1);
        while !retirement.poll_retired().unwrap() {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
    }
}
