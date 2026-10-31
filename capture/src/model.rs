use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub const MAX_STREAMS: usize = 4;
pub const MAX_LIFETIME_STREAMS: u64 = 128;
pub const MAX_ENUMERATION_LIMIT: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureError {
    pub code: &'static str,
    pub message: String,
}

impl CaptureError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// An error whose message is its code.
    pub fn code(code: &'static str) -> Self {
        Self::new(code, code)
    }

    pub fn frame_helper_unavailable(message: impl Into<String>) -> Self {
        Self::new("frame_helper_unavailable", message)
    }

    pub fn permission_denied(helper: &str) -> Self {
        Self::new(
            "permission_denied",
            format!(
                "the frame helper does not hold CAP_SYS_ADMIN; run `sudo setcap cap_sys_admin+p {helper}` \
                 and make sure the host service does not set NoNewPrivileges or drop CAP_SYS_ADMIN \
                 from its bounding set"
            ),
        )
    }

    pub fn driver_unavailable(message: impl Into<String>) -> Self {
        Self::new("driver_unavailable", message)
    }

    pub fn payload(&self) -> Value {
        json!({"ok":false,"error":{"code":self.code,"message":self.message}})
    }
}

pub type CaptureResult<T> = Result<T, CaptureError>;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    pub capture_principals: BTreeSet<String>,
    /// Absolute path of `amaterasu-vkms-frame-helper`. Capture is unavailable
    /// without it; the file is verified each time it is used.
    pub frame_helper: Option<String>,
    /// Explicitly selects package-approved authority. Only the host injects
    /// the inherited descriptor after the protected launcher authenticated it.
    pub authority_channel: bool,
    pub authority_channel_fd: Option<i32>,
    /// Copies scanout into independent GPU storage before advertising leases.
    pub gpu_snapshot: bool,
    /// Require immutable leases backed by a protected GPU snapshot. Never lend
    /// the ordinary borrowed-frame interface in this mode.
    pub require_lease: bool,
}

impl Settings {
    pub fn validate(&self) -> CaptureResult<()> {
        if self.require_lease && (!self.gpu_snapshot || !cfg!(feature = "driver-read-leases")) {
            return Err(CaptureError::new(
                "lease_source_unavailable",
                "require_lease needs gpu_snapshot and a driver-read-leases build",
            ));
        }
        let helper_valid = self
            .frame_helper
            .as_ref()
            .is_none_or(|path| !path.is_empty() && path.len() <= 4096 && !path.contains('\0'));
        if !helper_valid
            || !valid_principals(&self.capture_principals)
            || (self.authority_channel && self.frame_helper.is_some())
            || self
                .authority_channel_fd
                .is_some_and(|fd| !self.authority_channel || fd < 3)
        {
            return Err(CaptureError::code("invalid_settings"));
        }
        Ok(())
    }
}

/// A selected authority path. Approved channels never fall back to spawning
/// a capability-bearing executable, including after revocation or failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelperEndpoint<'a> {
    Executable(&'a str),
    ApprovedChannel(i32),
}

impl HelperEndpoint<'_> {
    pub fn label(self) -> String {
        match self {
            Self::Executable(path) => path.to_owned(),
            Self::ApprovedChannel(_) => "package-approved capture authority".into(),
        }
    }
}

impl Settings {
    pub fn helper_endpoint(&self) -> CaptureResult<HelperEndpoint<'_>> {
        if self.authority_channel {
            return self.authority_channel_fd.map(HelperEndpoint::ApprovedChannel).ok_or_else(|| {
                CaptureError::new("authority_unavailable", "package-approved capture authority was not supplied by the protected launcher")
            });
        }
        self.frame_helper
            .as_deref()
            .map(HelperEndpoint::Executable)
            .ok_or_else(|| {
                CaptureError::frame_helper_unavailable("the frame_helper setting is not configured")
            })
    }
}

fn valid_principals(principals: &BTreeSet<String>) -> bool {
    principals.len() <= 16
        && principals.iter().all(|name| {
            !name.is_empty()
                && name.len() <= 128
                && name.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'.' | b'-' | b'_' | b'/')
                })
        })
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Status,
    Enumerate {
        offset: Option<usize>,
        limit: Option<usize>,
    },
    Open {
        output: String,
        operation_id: Option<String>,
        constant_rate: Option<bool>,
    },
    Lookup {
        operation_id: String,
    },
    Close {
        stream: u64,
    },
}

pub fn validate_operation_id(id: &str) -> CaptureResult<()> {
    if id.is_empty() || id.len() > 128 || id.chars().any(char::is_control) {
        Err(CaptureError::code("invalid_operation_id"))
    } else {
        Ok(())
    }
}

pub fn new_id() -> CaptureResult<u64> {
    let mut bytes = [0; 8];
    getrandom::fill(&mut bytes).map_err(|_| CaptureError::code("entropy_error"))?;
    match u64::from_le_bytes(bytes) {
        0 => Err(CaptureError::code("entropy_error")),
        value => Ok(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_leases_need_both_a_snapshot_and_a_guarded_source() {
        let mut settings = Settings {
            require_lease: true,
            ..Settings::default()
        };
        assert!(settings.validate().is_err());
        settings.gpu_snapshot = true;
        assert_eq!(
            settings.validate().is_ok(),
            cfg!(feature = "driver-read-leases")
        );
    }

    #[test]
    fn settings_reject_unknown_fields_and_bad_values() {
        assert!(
            serde_json::from_value::<Settings>(json!({"enabled":true,"remember_source":true}))
                .is_err()
        );
        let settings: Settings = serde_json::from_value(json!({
            "enabled":true,"capture_principals":["amaterasu.video"],
            "frame_helper":"/usr/libexec/amaterasu/amaterasu-vkms-frame-helper"
        }))
        .unwrap();
        assert!(settings.validate().is_ok());
        let mut bad = settings.clone();
        bad.capture_principals.insert("Upper".into());
        assert!(bad.validate().is_err());
        let mut empty_path = settings;
        empty_path.frame_helper = Some(String::new());
        assert!(empty_path.validate().is_err());
    }

    #[test]
    fn approved_authority_requires_explicit_mode_and_never_uses_a_legacy_fallback() {
        let mut settings = Settings {
            authority_channel: true,
            ..Settings::default()
        };
        assert!(settings.validate().is_ok());
        assert_eq!(
            settings.helper_endpoint().err().unwrap().code,
            "authority_unavailable"
        );
        settings.frame_helper = Some("/opt/legacy-helper".into());
        assert!(settings.validate().is_err());
        // Even malformed mixed settings retain approved-channel selection.
        assert_eq!(
            settings.helper_endpoint().err().unwrap().code,
            "authority_unavailable"
        );
        settings.frame_helper = None;
        settings.authority_channel_fd = Some(198);
        assert!(settings.validate().is_ok());
        assert_eq!(
            settings.helper_endpoint(),
            Ok(HelperEndpoint::ApprovedChannel(198))
        );
        settings.authority_channel = false;
        assert!(settings.validate().is_err());
        settings.authority_channel = true;
        settings.authority_channel_fd = Some(2);
        assert!(settings.validate().is_err());
    }

    #[test]
    fn errors_use_the_shared_reply_shape() {
        let payload = CaptureError::permission_denied("/opt/helper").payload();
        assert_eq!(payload["ok"], false);
        assert_eq!(payload["error"]["code"], "permission_denied");
        let message = payload["error"]["message"].as_str().unwrap();
        assert!(message.contains("sudo setcap cap_sys_admin+p /opt/helper"));
    }

    #[test]
    fn pipewire_portal_actions_are_not_accepted() {
        assert!(
            serde_json::from_value::<Operation>(
                json!({"action":"select_source","source_type":"monitor","operation_id":"x"})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<Operation>(
                json!({"action":"open","output":"kwin:Virtual-1","operation_id":"x"})
            )
            .is_ok()
        );
    }
}
