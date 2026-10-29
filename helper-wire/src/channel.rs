//! One connected AF_UNIX SOCK_SEQPACKET endpoint that carries whole records
//! and SCM_RIGHTS descriptors.
use crate::protocol::MAX_RESPONSE_DESCRIPTORS;
use std::{
    io,
    mem::{size_of, zeroed},
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
    ptr,
    time::Duration,
};

/// Largest number of descriptors one record may carry in either direction.
pub const MAX_DESCRIPTORS: usize = MAX_RESPONSE_DESCRIPTORS;

/// Control buffer large enough for one SCM_RIGHTS message of
/// `MAX_DESCRIPTORS` descriptors plus slack for a second header, aligned for
/// `cmsghdr`.
#[repr(C, align(8))]
struct ControlBuffer([u8; 128]);

/// One received record. Descriptors are owned and close on drop, so a
/// rejected record never leaks what the peer sent.
#[derive(Debug)]
pub struct Received {
    pub len: usize,
    pub descriptors: Vec<OwnedFd>,
    /// The record was larger than the receive buffer.
    pub truncated: bool,
    /// The kernel dropped descriptors because the control buffer was full.
    pub descriptors_truncated: bool,
}

impl Received {
    /// A zero-length read on a sequenced-packet socket means the peer closed.
    pub fn is_end_of_stream(&self) -> bool {
        self.len == 0 && self.descriptors.is_empty() && !self.truncated
    }
}

#[derive(Debug)]
pub struct SeqpacketChannel {
    socket: OwnedFd,
}

impl SeqpacketChannel {
    /// Creates a connected close-on-exec pair.
    pub fn pair() -> io::Result<(Self, Self)> {
        let mut fds = [-1; 2];
        // SAFETY: `fds` is a writable two-element array as socketpair requires.
        let result = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: socketpair succeeded, so both descriptors are new and owned here.
        let (first, second) =
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        Ok((Self { socket: first }, Self { socket: second }))
    }

    /// Adopts an inherited socket after proving it is a connected AF_UNIX
    /// SOCK_SEQPACKET endpoint.
    pub fn from_owned(socket: OwnedFd) -> io::Result<Self> {
        let domain = socket_option(socket.as_fd(), libc::SO_DOMAIN)?;
        let kind = socket_option(socket.as_fd(), libc::SO_TYPE)?;
        if domain != libc::AF_UNIX || kind != libc::SOCK_SEQPACKET {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        Ok(Self { socket })
    }

    pub fn into_owned(self) -> OwnedFd {
        self.socket
    }

    /// A second reference to this socket that another thread can use to end
    /// the conversation while this endpoint is blocked in a receive.
    pub fn shutdown_handle(&self) -> io::Result<ChannelShutdown> {
        Ok(ChannelShutdown {
            socket: self.socket.try_clone()?,
        })
    }

    /// Effective user id of the connected peer.
    pub fn peer_uid(&self) -> io::Result<u32> {
        // SAFETY: ucred is plain data; zero is a valid initial value.
        let mut credentials: libc::ucred = unsafe { zeroed() };
        let mut len = size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: the output buffer and its length describe `credentials`.
        let result = unsafe {
            libc::getsockopt(
                self.socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(credentials.uid)
    }

    /// Bounds blocking receives. `None` waits indefinitely.
    pub fn set_receive_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        let value = match timeout {
            Some(duration) if duration.is_zero() => {
                return Err(io::Error::from(io::ErrorKind::InvalidInput));
            }
            Some(duration) => libc::timeval {
                tv_sec: duration.as_secs().min(i32::MAX as u64) as libc::time_t,
                tv_usec: duration.subsec_micros() as libc::suseconds_t,
            },
            None => libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
        };
        // SAFETY: the option value points to an initialized timeval of the stated size.
        let result = unsafe {
            libc::setsockopt(
                self.socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&value as *const libc::timeval).cast(),
                size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Sends one record and, when `descriptors` is nonempty, one SCM_RIGHTS
    /// message. The kernel duplicates the descriptors into the peer; the
    /// caller keeps and later closes its own copies.
    pub fn send(&self, bytes: &[u8], descriptors: &[BorrowedFd<'_>]) -> io::Result<()> {
        if descriptors.len() > MAX_DESCRIPTORS {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let mut payload = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: bytes.len(),
        };
        let mut control = ControlBuffer([0; 128]);
        // SAFETY: msghdr is plain data and is fully initialized below.
        let mut message: libc::msghdr = unsafe { zeroed() };
        message.msg_iov = &mut payload;
        message.msg_iovlen = 1;
        if !descriptors.is_empty() {
            let data_len = (descriptors.len() * size_of::<RawFd>()) as u32;
            // SAFETY: CMSG_SPACE is a pure size computation.
            let space = unsafe { libc::CMSG_SPACE(data_len) } as usize;
            message.msg_control = control.0.as_mut_ptr().cast();
            message.msg_controllen = space as _;
            // SAFETY: the control buffer is aligned for cmsghdr and at least
            // `space` bytes long, so the first header and its data fit.
            unsafe {
                let header = libc::CMSG_FIRSTHDR(&message);
                (*header).cmsg_level = libc::SOL_SOCKET;
                (*header).cmsg_type = libc::SCM_RIGHTS;
                (*header).cmsg_len = libc::CMSG_LEN(data_len) as _;
                let data = libc::CMSG_DATA(header).cast::<RawFd>();
                for (index, descriptor) in descriptors.iter().enumerate() {
                    ptr::write_unaligned(data.add(index), descriptor.as_raw_fd());
                }
            }
        }
        loop {
            // SAFETY: message, payload and control buffers outlive the call;
            // the borrowed descriptors stay open until sendmsg returns.
            let sent =
                unsafe { libc::sendmsg(self.socket.as_raw_fd(), &message, libc::MSG_NOSIGNAL) };
            if sent >= 0 {
                return if sent as usize == bytes.len() {
                    Ok(())
                } else {
                    Err(io::Error::from(io::ErrorKind::WriteZero))
                };
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    /// Receives one record into `buffer`. Descriptors arrive close-on-exec.
    pub fn receive(&self, buffer: &mut [u8]) -> io::Result<Received> {
        let mut payload = libc::iovec {
            iov_base: buffer.as_mut_ptr().cast(),
            iov_len: buffer.len(),
        };
        let mut control = ControlBuffer([0; 128]);
        // SAFETY: msghdr is plain data and is fully initialized below.
        let mut message: libc::msghdr = unsafe { zeroed() };
        message.msg_iov = &mut payload;
        message.msg_iovlen = 1;
        message.msg_control = control.0.as_mut_ptr().cast();
        message.msg_controllen = control.0.len() as _;
        let received = loop {
            // SAFETY: payload and control buffers are writable for their stated lengths.
            let received = unsafe {
                libc::recvmsg(
                    self.socket.as_raw_fd(),
                    &mut message,
                    libc::MSG_CMSG_CLOEXEC,
                )
            };
            if received >= 0 {
                break received as usize;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        };
        // Take ownership before any validation so every path closes them.
        let descriptors = take_descriptors(&message);
        Ok(Received {
            len: received.min(buffer.len()),
            descriptors,
            truncated: message.msg_flags & libc::MSG_TRUNC != 0,
            descriptors_truncated: message.msg_flags & libc::MSG_CTRUNC != 0,
        })
    }
}

/// Ends a channel's conversation from any thread. It holds its own
/// close-on-exec duplicate of the socket, so it stays valid however long it
/// outlives the channel, and a closed channel number can never be reused
/// under it.
#[derive(Debug)]
pub struct ChannelShutdown {
    socket: OwnedFd,
}

impl ChannelShutdown {
    /// Shuts the socket down in both directions. A receive blocked on the
    /// channel and every later one report end of stream at once, later sends
    /// fail, and the peer sees end of stream. Repeating it has no effect.
    pub fn shutdown(&self) {
        // SAFETY: shutdown(2) only changes the state of this owned, open
        // socket; it touches no memory. An ENOTCONN after an earlier
        // shutdown is harmless and ignored.
        unsafe { libc::shutdown(self.socket.as_raw_fd(), libc::SHUT_RDWR) };
    }
}

impl AsFd for SeqpacketChannel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.socket.as_fd()
    }
}

fn take_descriptors(message: &libc::msghdr) -> Vec<OwnedFd> {
    let mut descriptors = Vec::new();
    // SAFETY: recvmsg initialized the control buffer that `message` points
    // to and set msg_controllen; the CMSG macros stay within that length.
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(header).cast::<RawFd>();
                let data_len = (*header).cmsg_len as usize - (data as usize - header as usize);
                for index in 0..data_len / size_of::<RawFd>() {
                    let raw = ptr::read_unaligned(data.add(index));
                    if raw >= 0 {
                        // The kernel installed this descriptor for this process
                        // during recvmsg; nothing else owns it yet.
                        descriptors.push(OwnedFd::from_raw_fd(raw));
                    }
                }
            }
            header = libc::CMSG_NXTHDR(message, header);
        }
    }
    descriptors
}

fn socket_option(socket: BorrowedFd<'_>, option: libc::c_int) -> io::Result<libc::c_int> {
    let mut value: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: the output buffer and its length describe `value`.
    let result = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            option,
            (&mut value as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(value)
}

#[cfg(test)]
mod tests;
