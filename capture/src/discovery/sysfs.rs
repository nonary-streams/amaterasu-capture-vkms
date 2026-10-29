//! Reads `/sys/class/drm` entry names and `cardN/dev` only. Connector
//! attributes are not read here; KMS reports their state.
use super::SysfsCard;
use amaterasu_vkms_frame_helper::drm_node::DeviceNumber;
use std::{collections::BTreeMap, fs, path::Path};

pub const DRM_SYSFS_ROOT: &str = "/sys/class/drm";

/// Cards with at least one `cardN-Virtual-M` connector, ordered by number.
pub fn scan(root: &Path) -> Result<Vec<SysfsCard>, String> {
    let entries = fs::read_dir(root).map_err(|error| error.to_string())?;
    let mut connectors: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if let Some((card, index)) = parse_virtual_connector(&name) {
            connectors.entry(card).or_default().push(index);
        }
    }
    let mut cards = Vec::new();
    for (card, mut indexes) in connectors {
        let name = format!("card{card}");
        let Ok(device) = read_device_number(&root.join(&name).join("dev")) else {
            continue;
        };
        if device.minor != card {
            continue;
        }
        indexes.sort_unstable();
        cards.push(SysfsCard {
            name,
            device,
            virtual_connectors: indexes,
        });
    }
    Ok(cards)
}

fn parse_decimal(text: &str) -> Option<u32> {
    let canonical = !text.is_empty()
        && text.bytes().all(|byte| byte.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    canonical.then(|| text.parse().ok()).flatten()
}

/// `card3-Virtual-9` → (3, 9).
fn parse_virtual_connector(name: &str) -> Option<(u32, u32)> {
    let rest = name.strip_prefix("card")?;
    let (card, connector) = rest.split_once('-')?;
    let index = connector.strip_prefix("Virtual-")?;
    let index = parse_decimal(index).filter(|index| *index != 0)?;
    Some((parse_decimal(card)?, index))
}

/// Parses the `MAJOR:MINOR` contents of a sysfs `dev` attribute.
fn read_device_number(path: &Path) -> Result<DeviceNumber, ()> {
    let text = fs::read_to_string(path).map_err(|_| ())?;
    let (major, minor) = text.trim_end().split_once(':').ok_or(())?;
    Ok(DeviceNumber {
        major: parse_decimal(major).ok_or(())?,
        minor: parse_decimal(minor).ok_or(())?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    struct FakeSysfs(PathBuf);

    impl FakeSysfs {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "amaterasu-capture-vkms-sysfs-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn card(&self, name: &str, dev: &str) -> &Self {
            fs::create_dir_all(self.0.join(name)).unwrap();
            fs::write(self.0.join(name).join("dev"), dev).unwrap();
            self
        }
        fn entry(&self, name: &str) -> &Self {
            fs::create_dir_all(self.0.join(name)).unwrap();
            self
        }
    }

    impl Drop for FakeSysfs {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn only_cards_with_virtual_connectors_are_candidates() {
        let sysfs = FakeSysfs::new();
        sysfs
            .card("card0", "226:0\n")
            .entry("card0-DP-4")
            .entry("card0-HDMI-A-2")
            .card("card3", "226:3\n")
            .entry("card3-Virtual-16")
            .entry("card3-Virtual-9")
            .entry("card3-Virtual-10")
            .entry("renderD128")
            .entry("version");
        let cards = scan(&sysfs.0).unwrap();
        assert_eq!(
            cards,
            vec![SysfsCard {
                name: "card3".into(),
                device: DeviceNumber {
                    major: 226,
                    minor: 3
                },
                virtual_connectors: vec![9, 10, 16],
            }]
        );
    }

    #[test]
    fn cards_with_missing_or_inconsistent_device_numbers_are_skipped() {
        let sysfs = FakeSysfs::new();
        sysfs
            .entry("card1")
            .entry("card1-Virtual-1")
            .card("card2", "226:7\n")
            .entry("card2-Virtual-1")
            .card("card4", "garbage")
            .entry("card4-Virtual-1")
            .card("card5", "226:5")
            .entry("card5-Virtual-2");
        let cards = scan(&sysfs.0).unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].name, "card5");
    }

    #[test]
    fn connector_names_are_parsed_strictly() {
        assert_eq!(parse_virtual_connector("card3-Virtual-9"), Some((3, 9)));
        assert_eq!(parse_virtual_connector("card10-Virtual-1"), Some((10, 1)));
        for name in [
            "card3-Virtual-0",
            "card3-Virtual-09",
            "card3-Virtual-",
            "card03-Virtual-1",
            "card-Virtual-1",
            "card3-DP-1",
            "renderD128",
        ] {
            assert_eq!(parse_virtual_connector(name), None, "{name}");
        }
    }

    #[test]
    fn a_missing_root_is_an_error() {
        assert!(scan(Path::new("/nonexistent/amaterasu-sysfs")).is_err());
    }
}
