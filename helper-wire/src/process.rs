//! Verifying and spawning the helper executable from an unprivileged parent.
//!
//! The configured file is opened once and checked through that descriptor:
//! a regular, executable file owned by root, not writable by group or others,
//! without setuid or setgid bits. The child executes that same inode through
//! `/proc/self/fd`, so a later rename or replacement of the path cannot swap
//! in a different program. The child starts with an empty environment, null
//! standard streams, `/` as its directory, and the control socket at fd 3;
//! every other descriptor is close-on-exec.
use crate::{
    HELPER_PROGRAM_NAME, HELPER_SOCKET_FD, channel::SeqpacketChannel, client::HelperClient,
    exit_status,
};
use std::{
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{
            fs::{MetadataExt, OpenOptionsExt},
            process::{CommandExt, ExitStatusExt},
        },
    },
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const GROUP_OR_OTHER_WRITE: u32 = 0o022;
const SET_ID_BITS: u32 = 0o6000;
const ANY_EXECUTE: u32 = 0o111;
/// Spawn-time descriptors are moved at least this high so neither can
/// collide with fd 3 in the child.
const SPAWN_DESCRIPTOR_FLOOR: RawFd = 10;
const SHUTDOWN_GRACE: Duration = Duration::from_millis(50);
const STARTUP_EXIT_GRACE: Duration = Duration::from_millis(200);

/// What `fstat` says about the configured helper file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelperFileFacts {
    pub regular_file: bool,
    pub owner_uid: u32,
    pub mode: u32,
}

#[derive(Debug)]
pub enum HelperFileProblem {
    NotAbsolute,
    Unopenable(io::Error),
    NotRegularFile,
    NotOwnedByRoot,
    WritableByGroupOrOthers,
    SetIdBits,
    NotExecutable,
}

impl HelperFileProblem {
    pub fn describe(&self) -> String {
        match self {
            Self::NotAbsolute => "frame_helper must be an absolute path".into(),
            Self::Unopenable(error) => format!("frame_helper cannot be opened: {error}"),
            Self::NotRegularFile => "frame_helper is not a regular file".into(),
            Self::NotOwnedByRoot => "frame_helper must be owned by root".into(),
            Self::WritableByGroupOrOthers => {
                "frame_helper must not be writable by group or others".into()
            }
            Self::SetIdBits => "frame_helper must not be setuid or setgid".into(),
            Self::NotExecutable => "frame_helper is not executable".into(),
        }
    }
}

pub fn check_helper_file(facts: HelperFileFacts) -> Result<(), HelperFileProblem> {
    if !facts.regular_file {
        Err(HelperFileProblem::NotRegularFile)
    } else if facts.owner_uid != 0 {
        Err(HelperFileProblem::NotOwnedByRoot)
    } else if facts.mode & GROUP_OR_OTHER_WRITE != 0 {
        Err(HelperFileProblem::WritableByGroupOrOthers)
    } else if facts.mode & SET_ID_BITS != 0 {
        Err(HelperFileProblem::SetIdBits)
    } else if facts.mode & ANY_EXECUTE == 0 {
        Err(HelperFileProblem::NotExecutable)
    } else {
        Ok(())
    }
}

/// Opens `path` and verifies the opened file, not the name.
pub fn open_verified_helper(path: &Path) -> Result<File, HelperFileProblem> {
    if !path.is_absolute() {
        return Err(HelperFileProblem::NotAbsolute);
    }
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOCTTY)
        .open(path)
        .map_err(HelperFileProblem::Unopenable)?;
    let metadata = file.metadata().map_err(HelperFileProblem::Unopenable)?;
    check_helper_file(HelperFileFacts {
        regular_file: metadata.file_type().is_file(),
        owner_uid: metadata.uid(),
        mode: metadata.mode(),
    })?;
    Ok(file)
}

/// A running helper and the client connected to it.
pub struct HelperProcess {
    client: Option<HelperClient>,
    child: Child,
}

impl HelperProcess {
    /// Spawns `executable`, which the caller verified with
    /// [`open_verified_helper`]. The returned helper has not been greeted.
    pub fn spawn(executable: File) -> io::Result<Self> {
        let (ours, theirs) = SeqpacketChannel::pair()?;
        let theirs = duplicate_above_floor(&theirs.into_owned())?;
        let executable = duplicate_above_floor(&OwnedFd::from(executable))?;
        let socket = theirs.as_raw_fd();
        let mut command = Command::new(format!("/proc/self/fd/{}", executable.as_raw_fd()));
        command
            .arg0(HELPER_PROGRAM_NAME)
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: the hook runs in the forked child before exec and only
        // calls the async-signal-safe dup2, fcntl and close_range syscalls on
        // descriptor numbers captured by value.
        unsafe {
            command.pre_exec(move || place_control_socket(socket));
        }
        let child = command.spawn()?;
        // The child holds its own copies now; close the parent's.
        drop(theirs);
        drop(executable);
        Ok(Self {
            client: Some(HelperClient::new(ours)),
            child,
        })
    }

    pub fn client(&mut self) -> &mut HelperClient {
        self.client.as_mut().expect("client lives until drop")
    }

    /// After the helper closed its socket during startup, waits briefly for
    /// its exit and explains a refusal to start.
    pub fn startup_refusal(&mut self) -> String {
        let deadline = Instant::now() + STARTUP_EXIT_GRACE;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    return match (status.code(), status.signal()) {
                        (Some(code), _) => exit_status::describe(code as u8).into(),
                        (None, Some(signal)) => {
                            format!("the frame helper was killed by signal {signal}")
                        }
                        (None, None) => "the frame helper exited".into(),
                    };
                }
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(2)),
                _ => return "the frame helper closed its control socket".into(),
            }
        }
    }
}

impl Drop for HelperProcess {
    fn drop(&mut self) {
        // Closing the socket is the helper's signal to exit.
        drop(self.client.take());
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        while Instant::now() < deadline {
            if !matches!(self.child.try_wait(), Ok(None)) {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn duplicate_above_floor(descriptor: &OwnedFd) -> io::Result<OwnedFd> {
    // SAFETY: F_DUPFD_CLOEXEC creates a new descriptor for an open one.
    let raw = unsafe {
        libc::fcntl(
            descriptor.as_raw_fd(),
            libc::F_DUPFD_CLOEXEC,
            SPAWN_DESCRIPTOR_FLOOR,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fcntl returned a new descriptor owned only by this value.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Runs in the forked child: moves the socket to fd 3 (dup2 leaves the copy
/// inheritable) and marks every descriptor above it close-on-exec. The
/// executable's descriptor is close-on-exec too; exec resolves its
/// `/proc/self/fd` path before closing it.
fn place_control_socket(socket: RawFd) -> io::Result<()> {
    // SAFETY: dup2 on two descriptor numbers; `socket` is open in the child.
    if unsafe { libc::dup2(socket, HELPER_SOCKET_FD) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let first = HELPER_SOCKET_FD as libc::c_uint + 1;
    // SAFETY: close_range with CLOSE_RANGE_CLOEXEC only sets flags.
    let result = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            first,
            libc::c_uint::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(owner_uid: u32, mode: u32) -> HelperFileFacts {
        HelperFileFacts {
            regular_file: true,
            owner_uid,
            mode: 0o100000 | mode,
        }
    }

    #[test]
    fn only_root_owned_unshared_executables_are_accepted() {
        assert!(check_helper_file(facts(0, 0o755)).is_ok());
        assert!(check_helper_file(facts(0, 0o500)).is_ok());
        assert!(matches!(
            check_helper_file(facts(1000, 0o755)),
            Err(HelperFileProblem::NotOwnedByRoot)
        ));
        assert!(matches!(
            check_helper_file(facts(0, 0o775)),
            Err(HelperFileProblem::WritableByGroupOrOthers)
        ));
        assert!(matches!(
            check_helper_file(facts(0, 0o757)),
            Err(HelperFileProblem::WritableByGroupOrOthers)
        ));
        assert!(matches!(
            check_helper_file(facts(0, 0o4755)),
            Err(HelperFileProblem::SetIdBits)
        ));
        assert!(matches!(
            check_helper_file(facts(0, 0o644)),
            Err(HelperFileProblem::NotExecutable)
        ));
        assert!(matches!(
            check_helper_file(HelperFileFacts {
                regular_file: false,
                ..facts(0, 0o755)
            }),
            Err(HelperFileProblem::NotRegularFile)
        ));
    }

    #[test]
    fn relative_missing_and_user_owned_paths_are_refused() {
        assert!(matches!(
            open_verified_helper(Path::new("helper")),
            Err(HelperFileProblem::NotAbsolute)
        ));
        assert!(matches!(
            open_verified_helper(Path::new("/nonexistent/amaterasu-helper")),
            Err(HelperFileProblem::Unopenable(_))
        ));
        let directory =
            std::env::temp_dir().join(format!("amaterasu-helper-check-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join("helper");
        std::fs::write(&file, b"#!/bin/false\n").unwrap();
        // SAFETY: getuid has no preconditions.
        let expected_root = unsafe { libc::getuid() } == 0;
        let result = open_verified_helper(&file);
        std::fs::remove_dir_all(&directory).unwrap();
        if !expected_root {
            assert!(matches!(result, Err(HelperFileProblem::NotOwnedByRoot)));
        }
    }

    #[test]
    fn a_root_owned_system_directory_is_not_a_regular_file() {
        assert!(matches!(
            open_verified_helper(Path::new("/")),
            Err(HelperFileProblem::NotRegularFile)
        ));
    }
}
