//! Unprivileged half of the vibeshine_drm frame helper: the wire protocol and
//! the descriptor channel. The capture module links this library; only the
//! `amaterasu-vkms-frame-helper` binary holds CAP_SYS_ADMIN, and its privileged
//! code is not part of this library.
//!
//! The legacy helper inherits one connected AF_UNIX SOCK_SEQPACKET socket
//! at descriptor [`HELPER_SOCKET_FD`], takes no arguments, and ignores its
//! environment. The protected broker instead uses `--approved-service`, whose
//! root-only entry point requires a root peer and is never executed by a module.
//! Both modes exit when the peer closes the socket.
#[cfg(target_os = "linux")]
pub mod authority;
#[cfg(target_os = "linux")]
pub mod channel;
#[cfg(target_os = "linux")]
pub mod client;
#[cfg(target_os = "linux")]
pub mod drm_node;
#[cfg(target_os = "linux")]
pub mod process;
pub mod protocol;

/// Descriptor number of the inherited socket in the helper process.
pub const HELPER_SOCKET_FD: i32 = 3;
/// Program name the capture module passes as argv[0].
pub const HELPER_PROGRAM_NAME: &str = "amaterasu-vkms-frame-helper";

/// Exit statuses of a helper that refused to start. A clean shutdown after
/// the peer closes exits with zero.
pub mod exit_status {
    pub const UNSUPPORTED_PLATFORM: u8 = 64;
    /// Arguments do not select exactly one supported startup mode.
    pub const USAGE: u8 = 65;
    /// Descriptor 3 is not a valid socket with the peer required by this mode.
    pub const SOCKET: u8 = 66;
    /// Capability sets could not be reduced to at most CAP_SYS_ADMIN permitted.
    pub const CAPABILITIES: u8 = 67;
    /// Process identities do not match the selected startup mode.
    pub const ROOT_IDENTITY: u8 = 68;
    /// PR_SET_NO_NEW_PRIVS or another process hardening step failed.
    pub const HARDENING: u8 = 69;

    /// Operator-facing reason for a startup refusal.
    pub fn describe(status: u8) -> &'static str {
        match status {
            UNSUPPORTED_PLATFORM => "the frame helper only runs on Linux",
            USAGE => "the frame helper was started with unexpected arguments",
            SOCKET => "the frame helper did not inherit a valid control socket",
            CAPABILITIES => {
                "the frame helper holds capabilities other than CAP_SYS_ADMIN; grant only cap_sys_admin+p"
            }
            ROOT_IDENTITY => "the frame helper identities do not match its startup mode",
            HARDENING => "the frame helper could not harden its process",
            _ => "the frame helper exited during startup",
        }
    }
}
