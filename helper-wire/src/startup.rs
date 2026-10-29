//! Process hardening performed before the helper serves any request.
use amaterasu_vkms_frame_helper::{HELPER_SOCKET_FD, channel::SeqpacketChannel, exit_status};
use std::{
    ffi::OsString,
    io,
    os::fd::{FromRawFd, OwnedFd},
};

/// The legacy helper and the protected broker use distinct launch contracts.
/// This flag does not authorize a caller: service mode additionally requires
/// an all-root process identity and a root-owned connected peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupMode {
    UserHelper,
    ApprovedService,
}

impl StartupMode {
    pub fn parse(mut arguments: impl Iterator<Item = OsString>) -> Result<Self, u8> {
        match (arguments.next(), arguments.next()) {
            (None, None) => Ok(Self::UserHelper),
            (Some(argument), None) if argument == "--approved-service" => Ok(Self::ApprovedService),
            _ => Err(exit_status::USAGE),
        }
    }

    pub fn validate_process_identity(self) -> Result<(), u8> {
        let (mut real, mut effective, mut saved) = (0, 0, 0);
        let (mut real_group, mut effective_group, mut saved_group) = (0, 0, 0);
        // SAFETY: each pointer names a writable uid/gid on this stack frame.
        let read = unsafe {
            libc::getresuid(&mut real, &mut effective, &mut saved) == 0
                && libc::getresgid(&mut real_group, &mut effective_group, &mut saved_group) == 0
        };
        if !read {
            return Err(exit_status::ROOT_IDENTITY);
        }
        self.validate_identity(
            [real, effective, saved],
            [real_group, effective_group, saved_group],
        )
    }

    fn validate_identity(self, users: [u32; 3], groups: [u32; 3]) -> Result<(), u8> {
        let valid = match self {
            Self::UserHelper => !users.contains(&0) && !groups.contains(&0),
            Self::ApprovedService => users == [0; 3] && groups == [0; 3],
        };
        valid.then_some(()).ok_or(exit_status::ROOT_IDENTITY)
    }

    fn accepts_peer(self, peer_uid: u32, process_uid: u32) -> bool {
        match self {
            Self::UserHelper => peer_uid == process_uid && process_uid != 0,
            Self::ApprovedService => peer_uid == 0 && process_uid == 0,
        }
    }
}

/// Forbids privilege gain through exec, ptrace attachment and core dumps,
/// and clears the ambient capability set.
pub fn harden_process() -> Result<(), u8> {
    // SAFETY: these prctl options take integer arguments only.
    let hardened = unsafe {
        libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0
            && libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) == 0
            && libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_CLEAR_ALL,
                0,
                0,
                0,
            ) == 0
    };
    if hardened {
        Ok(())
    } else {
        Err(exit_status::HARDENING)
    }
}

/// Adopts the inherited control socket, marks it close-on-exec and checks
/// the peer identity for the selected mode. Approved service sockets must
/// originate from the protected root broker, never a same-UID user process.
pub fn adopt_control_socket(mode: StartupMode) -> Result<SeqpacketChannel, u8> {
    // SAFETY: F_GETFD only queries the descriptor number.
    if unsafe { libc::fcntl(HELPER_SOCKET_FD, libc::F_GETFD) } < 0 {
        return Err(exit_status::SOCKET);
    }
    // SAFETY: the launch contract hands descriptor 3 to this process and
    // nothing else in the helper refers to it.
    let socket = unsafe { OwnedFd::from_raw_fd(HELPER_SOCKET_FD) };
    // SAFETY: F_SETFD only changes this descriptor's flags.
    if unsafe { libc::fcntl(HELPER_SOCKET_FD, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
        return Err(exit_status::SOCKET);
    }
    let channel = SeqpacketChannel::from_owned(socket).map_err(|_| exit_status::SOCKET)?;
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    match channel.peer_uid() {
        Ok(peer) if mode.accepts_peer(peer, uid) => Ok(channel),
        _ => Err(exit_status::SOCKET),
    }
}

/// Closes every inherited descriptor above the control socket.
pub fn close_inherited_descriptors() -> io::Result<()> {
    let first = HELPER_SOCKET_FD as libc::c_uint + 1;
    // SAFETY: close_range closes descriptors this process does not use; the
    // helper has created no descriptors of its own above the socket yet.
    let result = unsafe { libc::syscall(libc::SYS_close_range, first, libc::c_uint::MAX, 0) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> Result<StartupMode, u8> {
        StartupMode::parse(arguments.iter().map(OsString::from))
    }

    #[test]
    fn startup_modes_require_exact_argument_lists() {
        assert_eq!(parse(&[]), Ok(StartupMode::UserHelper));
        assert_eq!(
            parse(&["--approved-service"]),
            Ok(StartupMode::ApprovedService)
        );
        for arguments in [
            vec![""],
            vec!["--approved-service=1"],
            vec!["--approved-service", "--approved-service"],
            vec!["--approved-service", "path"],
            vec!["--unknown"],
        ] {
            assert_eq!(parse(&arguments), Err(exit_status::USAGE));
        }
    }

    #[test]
    fn user_helper_refuses_every_root_identity_field() {
        assert!(
            StartupMode::UserHelper
                .validate_identity([1000; 3], [1000; 3])
                .is_ok()
        );
        for field in 0..6 {
            let mut ids = [1000; 6];
            ids[field] = 0;
            assert_eq!(
                StartupMode::UserHelper
                    .validate_identity(ids[..3].try_into().unwrap(), ids[3..].try_into().unwrap(),),
                Err(exit_status::ROOT_IDENTITY),
            );
        }
    }

    #[test]
    fn approved_service_requires_every_identity_field_root() {
        assert!(
            StartupMode::ApprovedService
                .validate_identity([0; 3], [0; 3])
                .is_ok()
        );
        for field in 0..6 {
            let mut ids = [0; 6];
            ids[field] = 1000;
            assert_eq!(
                StartupMode::ApprovedService
                    .validate_identity(ids[..3].try_into().unwrap(), ids[3..].try_into().unwrap(),),
                Err(exit_status::ROOT_IDENTITY),
            );
        }
    }

    #[test]
    fn service_peer_check_does_not_accept_matching_nonroot_users() {
        assert!(StartupMode::UserHelper.accepts_peer(1000, 1000));
        assert!(!StartupMode::UserHelper.accepts_peer(1001, 1000));
        assert!(!StartupMode::UserHelper.accepts_peer(0, 0));
        assert!(StartupMode::ApprovedService.accepts_peer(0, 0));
        assert!(!StartupMode::ApprovedService.accepts_peer(1000, 1000));
        assert!(!StartupMode::ApprovedService.accepts_peer(1000, 0));
        assert!(!StartupMode::ApprovedService.accepts_peer(0, 1000));
    }
}
