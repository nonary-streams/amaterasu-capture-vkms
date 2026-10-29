//! `amaterasu-vkms-frame-helper`: exports vibeshine_drm presentations for the
//! capture module, which spawns it with one SOCK_SEQPACKET socket at fd 3.
//!
//! The operator grants exactly `cap_sys_admin+p`. The helper keeps that
//! capability permitted but not effective, raises it only around the two
//! private driver ioctls, and drops every other capability at startup. The
//! separate `--approved-service` mode accepts only a root broker peer and is
//! installed root-only; it does not turn a user helper into a public service.
//! Neither mode reads configuration from its environment.
#[cfg(target_os = "linux")]
mod capabilities;
#[cfg(target_os = "linux")]
mod device;
#[cfg(target_os = "linux")]
mod service;
#[cfg(target_os = "linux")]
mod startup;

use std::process::ExitCode;

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(status) => ExitCode::from(status),
    }
}

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    ExitCode::from(amaterasu_vkms_frame_helper::exit_status::UNSUPPORTED_PLATFORM)
}

#[cfg(target_os = "linux")]
fn run() -> Result<(), u8> {
    use amaterasu_vkms_frame_helper::exit_status;

    let mode = startup::StartupMode::parse(std::env::args_os().skip(1))?;
    startup::close_inherited_descriptors().map_err(|_| exit_status::HARDENING)?;
    startup::harden_process()?;
    mode.validate_process_identity()?;
    let privilege = capabilities::AdminPrivilege::reduce(capabilities::ProcessCapabilities)
        .map_err(|_| exit_status::CAPABILITIES)?;
    let channel = startup::adopt_control_socket(mode)?;
    serve(
        &channel,
        service::Service::new(device::DeviceOpener, privilege),
    );
    Ok(())
}

/// Answers requests one at a time until the peer closes the socket.
#[cfg(target_os = "linux")]
fn serve<O: service::CardOpener, P: service::Privilege>(
    channel: &amaterasu_vkms_frame_helper::channel::SeqpacketChannel,
    mut service: service::Service<O, P>,
) {
    use amaterasu_vkms_frame_helper::protocol::{REQUEST_LEN, encode_response};
    use std::os::fd::{AsFd, BorrowedFd};

    // One spare byte exposes oversized requests as a length mismatch.
    let mut buffer = [0u8; REQUEST_LEN + 1];
    loop {
        let Ok(received) = channel.receive(&mut buffer) else {
            return;
        };
        if received.is_end_of_stream() {
            return;
        }
        let transport_malformed = received.truncated
            || received.descriptors_truncated
            || !received.descriptors.is_empty();
        // Requests never carry descriptors; close anything the peer attached.
        drop(received.descriptors);
        let (response, descriptors) = service.respond(&buffer[..received.len], transport_malformed);
        let encoded = encode_response(&response);
        let borrowed: Vec<BorrowedFd<'_>> = descriptors.iter().map(AsFd::as_fd).collect();
        let sent = channel.send(&encoded, &borrowed);
        // The kernel duplicated the descriptors into the peer; close ours.
        drop(borrowed);
        drop(descriptors);
        if sent.is_err() {
            return;
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use amaterasu_vkms_frame_helper::{
        channel::SeqpacketChannel,
        protocol::{
            Failure, HELPER_ADMIN_PERMITTED, Reply, Request, RequestRecord, decode_response,
            encode_request,
        },
    };
    use std::{
        io,
        os::fd::{AsFd, OwnedFd},
    };

    struct NoCard;
    impl service::PresentationCard for NoCard {
        fn export_frame(&self, _: u32) -> io::Result<service::DriverFrame> {
            unreachable!("no card is opened in these tests")
        }
        fn wait_present(&self, _: u32, _: u64, _: u32) -> io::Result<service::DriverPresentation> {
            unreachable!("no card is opened in these tests")
        }
    }
    struct RefusingOpener;
    impl service::CardOpener for RefusingOpener {
        type Card = NoCard;
        fn open(&self, _: u32, _: u32) -> Result<NoCard, service::OpenFailure> {
            Err(service::OpenFailure {
                failure: Failure::DeviceRejected,
                os_error: 0,
            })
        }
    }
    struct Permitted;
    impl service::Privilege for Permitted {
        fn admin_permitted(&self) -> bool {
            true
        }
        fn with_admin<T>(&self, operation: impl FnOnce() -> T) -> Option<T> {
            Some(operation())
        }
    }

    fn exchange(client: &SeqpacketChannel, request: Request) -> Reply {
        let bytes = encode_request(&RequestRecord {
            request_id: 1,
            request,
        });
        client.send(&bytes, &[]).unwrap();
        let mut buffer = [0; 256];
        let received = client.receive(&mut buffer).unwrap();
        decode_response(&buffer[..received.len]).unwrap().reply
    }

    #[test]
    fn serve_answers_until_the_peer_closes() {
        let (client, helper) = SeqpacketChannel::pair().unwrap();
        let server = std::thread::spawn(move || {
            serve(&helper, service::Service::new(RefusingOpener, Permitted));
        });
        assert_eq!(
            exchange(&client, Request::Hello),
            Reply::Hello {
                capabilities: HELPER_ADMIN_PERMITTED
            }
        );
        assert!(matches!(
            exchange(
                &client,
                Request::OpenCard {
                    major: 226,
                    minor: 0
                }
            ),
            Reply::Failure {
                failure: Failure::DeviceRejected,
                ..
            }
        ));

        // A request carrying a descriptor is malformed even if its bytes are valid.
        let attached: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        let bytes = encode_request(&RequestRecord {
            request_id: 2,
            request: Request::Hello,
        });
        client.send(&bytes, &[attached.as_fd()]).unwrap();
        let mut buffer = [0; 256];
        let received = client.receive(&mut buffer).unwrap();
        assert!(matches!(
            decode_response(&buffer[..received.len]).unwrap().reply,
            Reply::Failure {
                failure: Failure::MalformedRequest,
                ..
            }
        ));

        // An oversized request is malformed rather than silently truncated.
        client.send(&[0; 200], &[]).unwrap();
        let received = client.receive(&mut buffer).unwrap();
        assert!(matches!(
            decode_response(&buffer[..received.len]).unwrap().reply,
            Reply::Failure {
                failure: Failure::MalformedRequest,
                ..
            }
        ));

        drop(client);
        server.join().unwrap();
    }
}
