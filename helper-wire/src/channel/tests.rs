use super::*;
use std::{
    fs::File,
    io::{Read, Write},
    os::fd::AsFd,
};

fn pipe() -> (File, File) {
    let mut fds = [-1; 2];
    // SAFETY: `fds` is a writable two-element array as pipe2 requires.
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    // SAFETY: pipe2 succeeded and returned two new descriptors owned here.
    unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
}

fn is_close_on_exec(descriptor: BorrowedFd<'_>) -> bool {
    // SAFETY: F_GETFD only reads flags of an open descriptor.
    let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFD) };
    flags >= 0 && flags & libc::FD_CLOEXEC != 0
}

#[test]
fn records_and_descriptors_cross_a_socketpair_and_stay_usable() {
    let (left, right) = SeqpacketChannel::pair().unwrap();
    let (first_read, mut first_write) = pipe();
    let (second_read, mut second_write) = pipe();

    left.send(b"record", &[first_read.as_fd(), second_read.as_fd()])
        .unwrap();
    // The sender may close its copies immediately after sendmsg returns.
    drop(first_read);
    drop(second_read);

    let mut buffer = [0; 16];
    let received = right.receive(&mut buffer).unwrap();
    assert_eq!(&buffer[..received.len], b"record");
    assert!(!received.truncated);
    assert!(!received.descriptors_truncated);
    assert_eq!(received.descriptors.len(), 2);
    assert!(
        received
            .descriptors
            .iter()
            .all(|descriptor| is_close_on_exec(descriptor.as_fd()))
    );

    first_write.write_all(b"a").unwrap();
    second_write.write_all(b"b").unwrap();
    let mut descriptors = received.descriptors.into_iter();
    let mut first = File::from(descriptors.next().unwrap());
    let mut second = File::from(descriptors.next().unwrap());
    let mut byte = [0; 1];
    first.read_exact(&mut byte).unwrap();
    assert_eq!(&byte, b"a");
    second.read_exact(&mut byte).unwrap();
    assert_eq!(&byte, b"b");
}

#[test]
fn oversized_records_are_reported_as_truncated() {
    let (left, right) = SeqpacketChannel::pair().unwrap();
    left.send(&[7; 32], &[]).unwrap();
    let mut buffer = [0; 8];
    let received = right.receive(&mut buffer).unwrap();
    assert!(received.truncated);
    assert_eq!(received.len, 8);
}

#[test]
fn peer_close_is_end_of_stream() {
    let (left, right) = SeqpacketChannel::pair().unwrap();
    drop(left);
    let mut buffer = [0; 8];
    assert!(right.receive(&mut buffer).unwrap().is_end_of_stream());
}

#[test]
fn sending_to_a_closed_peer_fails_without_a_signal() {
    let (left, right) = SeqpacketChannel::pair().unwrap();
    drop(right);
    assert!(left.send(b"x", &[]).is_err());
}

#[test]
fn receive_timeout_bounds_a_blocking_read() {
    let (_left, right) = SeqpacketChannel::pair().unwrap();
    right
        .set_receive_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    let mut buffer = [0; 8];
    let error = right.receive(&mut buffer).unwrap_err();
    assert!(matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    ));
}

#[test]
fn adoption_accepts_only_seqpacket_unix_sockets() {
    let (left, _right) = SeqpacketChannel::pair().unwrap();
    let adopted = SeqpacketChannel::from_owned(left.into_owned()).unwrap();
    // SAFETY: getuid has no preconditions.
    assert_eq!(adopted.peer_uid().unwrap(), unsafe { libc::getuid() });

    let (read, _write) = pipe();
    assert!(SeqpacketChannel::from_owned(OwnedFd::from(read)).is_err());

    let mut stream = [-1; 2];
    // SAFETY: `stream` is a writable two-element array.
    assert_eq!(
        unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                0,
                stream.as_mut_ptr(),
            )
        },
        0
    );
    // SAFETY: socketpair returned two new descriptors owned here.
    let (stream_left, _stream_right) = unsafe {
        (
            OwnedFd::from_raw_fd(stream[0]),
            OwnedFd::from_raw_fd(stream[1]),
        )
    };
    assert!(SeqpacketChannel::from_owned(stream_left).is_err());
}

#[test]
fn too_many_descriptors_are_refused_before_sending() {
    let (left, _right) = SeqpacketChannel::pair().unwrap();
    let (read, _write) = pipe();
    let many = vec![read.as_fd(); MAX_DESCRIPTORS + 1];
    assert_eq!(
        left.send(b"x", &many).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
}

#[test]
fn a_shutdown_handle_wakes_a_blocked_receive_and_ends_the_peer() {
    let (left, right) = SeqpacketChannel::pair().unwrap();
    let handle = left.shutdown_handle().unwrap();
    assert!(is_close_on_exec(handle.socket.as_fd()));
    let receiver = std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let mut buffer = [0; 8];
        let received = left.receive(&mut buffer).unwrap();
        (received.is_end_of_stream(), started.elapsed(), left)
    });
    std::thread::sleep(Duration::from_millis(20));
    handle.shutdown();
    let (ended, waited, left) = receiver.join().unwrap();
    assert!(ended);
    assert!(waited < Duration::from_secs(1));
    assert!(left.send(b"x", &[]).is_err());
    let mut buffer = [0; 8];
    assert!(right.receive(&mut buffer).unwrap().is_end_of_stream());
    // A second shutdown is harmless.
    handle.shutdown();
}
