use super::*;
use std::sync::Mutex;

pub struct FakeProbe {
    pub cards: Result<Vec<SysfsCard>, String>,
    pub answers: Vec<(String, Result<Vec<ConnectorState>, CardQueryError>)>,
    pub queried: Mutex<Vec<String>>,
}

impl DisplayProbe for FakeProbe {
    fn sysfs_cards(&self) -> Result<Vec<SysfsCard>, String> {
        self.cards.clone()
    }
    fn query_card(&self, card: &SysfsCard) -> Result<Vec<ConnectorState>, CardQueryError> {
        self.queried.lock().unwrap().push(card.name.clone());
        self.answers
            .iter()
            .find(|(name, _)| *name == card.name)
            .map(|(_, answer)| answer.clone())
            .unwrap_or(Err(CardQueryError::WrongDriver))
    }
}

fn card(minor: u32, connectors: &[u32]) -> SysfsCard {
    SysfsCard {
        name: format!("card{minor}"),
        device: DeviceNumber { major: 226, minor },
        virtual_connectors: connectors.to_vec(),
    }
}

fn active(connector_index: u32, crtc_id: u32) -> ConnectorState {
    ConnectorState {
        connector_id: 100 + connector_index,
        connector_index,
        connection: Connection::Connected,
        crtc: Some(CrtcState {
            crtc_id,
            mode: Some(DisplayMode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            }),
        }),
    }
}

fn probe(
    cards: Vec<SysfsCard>,
    answers: Vec<(&str, Result<Vec<ConnectorState>, CardQueryError>)>,
) -> FakeProbe {
    FakeProbe {
        cards: Ok(cards),
        answers: answers
            .into_iter()
            .map(|(name, answer)| (name.to_owned(), answer))
            .collect(),
        queried: Mutex::new(Vec::new()),
    }
}

fn name(index: u32) -> OutputName {
    OutputName::new(index).unwrap()
}

#[test]
fn an_active_output_resolves_to_its_card_and_crtc() {
    let probe = probe(
        vec![card(3, &[9, 10])],
        vec![("card3", Ok(vec![active(9, 41), active(10, 42)]))],
    );
    let binding = resolve(&probe, name(10)).unwrap();
    assert_eq!(binding.card, "card3");
    assert_eq!(
        binding.device,
        DeviceNumber {
            major: 226,
            minor: 3
        }
    );
    assert_eq!(binding.connector_id, 110);
    assert_eq!(binding.crtc_id, 42);
    assert_eq!(binding.mode.width, 1920);
}

#[test]
fn resolution_skips_cards_that_do_not_name_the_connector() {
    let probe = probe(
        vec![card(0, &[1]), card(3, &[9])],
        vec![("card3", Ok(vec![active(9, 41)]))],
    );
    resolve(&probe, name(9)).unwrap();
    assert_eq!(*probe.queried.lock().unwrap(), vec!["card3".to_owned()]);
}

#[test]
fn non_vibeshine_cards_are_ignored() {
    // A virtio-gpu guest also names its connector Virtual-1.
    let probe = probe(
        vec![card(0, &[1]), card(3, &[1])],
        vec![
            ("card0", Err(CardQueryError::WrongDriver)),
            ("card3", Ok(vec![active(1, 41)])),
        ],
    );
    assert_eq!(resolve(&probe, name(1)).unwrap().card, "card3");
}

#[test]
fn unusable_connectors_have_distinct_errors() {
    let disconnected = ConnectorState {
        connection: Connection::Disconnected,
        ..active(9, 41)
    };
    let without_crtc = ConnectorState {
        crtc: None,
        ..active(10, 0)
    };
    let without_mode = ConnectorState {
        crtc: Some(CrtcState {
            crtc_id: 43,
            mode: None,
        }),
        ..active(11, 43)
    };
    let probe = probe(
        vec![card(3, &[9, 10, 11, 12])],
        vec![("card3", Ok(vec![disconnected, without_crtc, without_mode]))],
    );
    let code = |index| resolve(&probe, name(index)).unwrap_err().code;
    assert_eq!(code(9), "output_disconnected");
    assert_eq!(code(10), "output_inactive");
    assert_eq!(code(11), "output_inactive");
    assert_eq!(code(12), "output_not_found");
}

#[test]
fn a_renumbered_connector_is_found_by_its_new_index() {
    let probe = probe(
        vec![card(3, &[9])],
        vec![("card3", Ok(vec![active(9, 41)]))],
    );
    assert_eq!(
        resolve(&probe, name(1)).unwrap_err().code,
        "output_not_found"
    );
    assert_eq!(resolve(&probe, name(9)).unwrap().crtc_id, 41);
}

#[test]
fn the_same_index_on_two_vibeshine_cards_is_ambiguous() {
    let probe = probe(
        vec![card(3, &[1]), card(4, &[1])],
        vec![
            ("card3", Ok(vec![active(1, 41)])),
            ("card4", Ok(vec![active(1, 51)])),
        ],
    );
    assert_eq!(
        resolve(&probe, name(1)).unwrap_err().code,
        "output_ambiguous"
    );
}

#[test]
fn no_vibeshine_card_is_driver_unavailable() {
    let none = probe(vec![], vec![]);
    assert_eq!(discover(&none).err().unwrap().code, "driver_unavailable");

    let unopenable = probe(
        vec![card(3, &[1])],
        vec![(
            "card3",
            Err(CardQueryError::Unavailable("Permission denied".into())),
        )],
    );
    let error = discover(&unopenable).err().unwrap();
    assert_eq!(error.code, "driver_unavailable");
    assert!(error.message.contains("card3: Permission denied"));

    let no_sysfs = FakeProbe {
        cards: Err("No such file or directory".into()),
        answers: vec![],
        queried: Mutex::new(Vec::new()),
    };
    assert_eq!(
        discover(&no_sysfs).err().unwrap().code,
        "driver_unavailable"
    );
}

#[test]
fn enumeration_metadata_reports_attachment_and_geometry() {
    let detached = ConnectorState {
        crtc: None,
        ..active(10, 0)
    };
    let probe = probe(
        vec![card(3, &[9, 10])],
        vec![("card3", Ok(vec![detached, active(9, 41)]))],
    );
    let discovery = discover(&probe).unwrap();
    let layout = [
        LayoutOutput {
            name: "eDP-1".into(),
            logical_left: 0,
            logical_top: 0,
            logical_width: 1210,
            logical_height: 756,
            pixel_mode: Some((3024, 1890)),
        },
        LayoutOutput {
            name: "Virtual-9".into(),
            logical_left: 1210,
            logical_top: 0,
            logical_width: 1920,
            logical_height: 1080,
            pixel_mode: Some((1920, 1080)),
        },
    ];
    let without_layout = discovery.outputs[0]
        .0
        .metadata(discovery.outputs[0].1, None);
    assert_eq!(without_layout["geometry_supported"], false);
    assert!(without_layout.get("desktop_geometry").is_none());
    let entries: Vec<_> = discovery
        .outputs
        .iter()
        .map(|(output, device)| output.metadata(*device, Some(&layout)))
        .collect();
    assert_eq!(entries[0]["id"], "kwin:Virtual-9");
    assert_eq!(entries[0]["device_name"], "Virtual-9");
    assert_eq!(entries[0]["attached"], true);
    assert_eq!(entries[0]["width"], 1920);
    assert_eq!(entries[0]["geometry_supported"], true);
    assert_eq!(entries[0]["desktop_geometry"]["output"]["left"], 1210);
    assert_eq!(
        entries[0]["desktop_geometry"]["virtual_desktop"]["width"],
        3130
    );
    assert_eq!(entries[1]["id"], "kwin:Virtual-10");
    assert_eq!(entries[1]["attached"], false);
    assert!(entries[1].get("desktop_geometry").is_none());
}
