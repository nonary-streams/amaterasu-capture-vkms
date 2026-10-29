//! `kwin:Virtual-N` output identities, shared with the virtual-display and
//! stream-session modules. N is the DRM connector type index the driver
//! assigned; it is not limited to 1..8 because a module reload renumbers.
use std::fmt;

const PREFIX: &str = "kwin:Virtual-";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OutputName {
    connector_index: u32,
}

impl OutputName {
    pub fn new(connector_index: u32) -> Option<Self> {
        (connector_index != 0).then_some(Self { connector_index })
    }

    /// Parses `kwin:Virtual-N` with a canonical decimal N >= 1.
    pub fn parse(output: &str) -> Option<Self> {
        let digits = output.strip_prefix(PREFIX)?;
        let canonical = !digits.is_empty()
            && digits.len() <= 10
            && digits.bytes().all(|byte| byte.is_ascii_digit())
            && !digits.starts_with('0');
        if !canonical {
            return None;
        }
        Self::new(digits.parse().ok()?)
    }

    /// The DRM connector type index (`connector_type_id`).
    pub fn connector_index(self) -> u32 {
        self.connector_index
    }

    /// The DRM connector name, `Virtual-N`.
    pub fn connector_name(self) -> String {
        format!("Virtual-{}", self.connector_index)
    }
}

impl fmt::Display for OutputName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{PREFIX}{}", self.connector_index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_virtual_outputs_parse_with_any_index() {
        for index in [1, 8, 9, 16, 4_000_000_000] {
            let name = OutputName::parse(&format!("kwin:Virtual-{index}")).unwrap();
            assert_eq!(name.connector_index(), index);
            assert_eq!(name.to_string(), format!("kwin:Virtual-{index}"));
            assert_eq!(name.connector_name(), format!("Virtual-{index}"));
        }
    }

    #[test]
    fn other_identities_are_rejected() {
        for output in [
            "kwin:Virtual-0",
            "kwin:Virtual-01",
            "kwin:Virtual-",
            "kwin:Virtual-+1",
            "kwin:Virtual-1 ",
            "kwin:Virtual-99999999999",
            "kwin:HDMI-A-1",
            "Virtual-1",
            "portal-selected-session-source",
            "kms:card0/conn=1",
        ] {
            assert_eq!(OutputName::parse(output), None, "{output}");
        }
    }
}
