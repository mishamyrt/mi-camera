use std::io;
use std::mem::{self, size_of};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr;
use std::time::{Duration, Instant};

/// An Annex B HEVC frame with its firmware presentation timestamp.
pub struct Frame {
    pub data: Vec<u8>,
    /// Presentation timestamp in microseconds.
    pub pts: u64,
}

/// A connection to a stock `fetch_av` video socket.
pub struct Capture {
    socket: OwnedFd,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn le32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn decode(buffer: &[u8]) -> io::Result<Frame> {
    if buffer.len() < 96
        || le32(buffer, 0) != u32::MAX
        || le32(buffer, 12) as usize != buffer.len()
        || le32(buffer, 64) != 1
    {
        return Err(invalid("unexpected shared video header"));
    }
    let logical = le32(buffer, 8) as usize;
    let length = le32(buffer, 80) as usize;
    if logical < 32 || logical > buffer.len() - 64 || length != logical - 32 || length < 5 {
        return Err(invalid("invalid video frame length"));
    }
    // libshbf contributes 64 bytes, fetch_av adds a 32-byte FrameInfo.
    let data = &buffer[96..96 + length];
    if !data.starts_with(&[0, 0, 1]) && !data.starts_with(&[0, 0, 0, 1]) {
        return Err(invalid("video frame has no Annex B prefix"));
    }
    Ok(Frame {
        data: data.to_vec(),
        pts: u64::from_le_bytes(buffer[72..80].try_into().unwrap()),
    })
}

impl Capture {
    /// Connect to a Unix sequenced-packet video control socket.
    pub fn connect(path: &str) -> io::Result<Self> {
        let mut address: libc::sockaddr_un = unsafe { mem::zeroed() };
        if path.is_empty() || path.len() >= address.sun_path.len() || path.as_bytes().contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid video socket path",
            ));
        }
        address.sun_family = libc::AF_UNIX as _;
        for (target, byte) in address.sun_path.iter_mut().zip(path.bytes()) {
            *target = byte as _;
        }
        let length = mem::offset_of!(libc::sockaddr_un, sun_path) + path.len() + 1;
        #[cfg(target_os = "macos")]
        {
            address.sun_len = length as _;
        }
        #[cfg(target_os = "linux")]
        let flags = libc::SOCK_CLOEXEC;
        #[cfg(not(target_os = "linux"))]
        let flags = 0;
        let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | flags, 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let socket = unsafe { OwnedFd::from_raw_fd(raw) };
        #[cfg(not(target_os = "linux"))]
        if unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe {
            libc::connect(
                raw,
                ptr::from_ref(&address).cast(),
                length as libc::socklen_t,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { socket })
    }

    /// Receive the next shared-memory frame within two seconds, copying only its payload.
    pub fn next_frame(&mut self) -> io::Result<Frame> {
        self.next_frame_until(Instant::now() + Duration::from_secs(2))
    }

    fn next_frame_until(&mut self, deadline: Instant) -> io::Result<Frame> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "video frame timed out",
                ));
            }
            let mut ready = libc::pollfd {
                fd: self.socket.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
            let polled = unsafe { libc::poll(&mut ready, 1, timeout) };
            if polled < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if polled == 0 {
                continue;
            }
            let mut body = [0u8; 4096];
            // cmsghdr storage supplies the alignment required by the CMSG macros.
            let mut control: [libc::cmsghdr; 16] = unsafe { mem::zeroed() };
            let mut iov = libc::iovec {
                iov_base: body.as_mut_ptr().cast(),
                iov_len: body.len(),
            };
            let mut message: libc::msghdr = unsafe { mem::zeroed() };
            message.msg_iov = &mut iov;
            message.msg_iovlen = 1;
            message.msg_control = control.as_mut_ptr().cast();
            message.msg_controllen = mem::size_of_val(&control) as _;
            #[cfg(target_os = "linux")]
            let flags = libc::MSG_CMSG_CLOEXEC | libc::MSG_DONTWAIT;
            #[cfg(not(target_os = "linux"))]
            let flags = libc::MSG_DONTWAIT;
            let received = unsafe { libc::recvmsg(self.socket.as_raw_fd(), &mut message, flags) };
            if received < 0 {
                let error = io::Error::last_os_error();
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) {
                    continue;
                }
                return Err(error);
            }
            // Own every received descriptor before rejecting a truncated message.
            let mut descriptors = Vec::new();
            let mut header = unsafe { libc::CMSG_FIRSTHDR(&message) };
            while !header.is_null() {
                let item = unsafe { &*header };
                let header_len = unsafe { libc::CMSG_LEN(0) } as usize;
                let offset = header as usize - message.msg_control as usize;
                let length = item.cmsg_len as usize;
                if length < header_len || length > message.msg_controllen as usize - offset {
                    return Err(invalid("invalid ancillary header"));
                }
                if item.cmsg_level == libc::SOL_SOCKET && item.cmsg_type == libc::SCM_RIGHTS {
                    let data = unsafe { libc::CMSG_DATA(header) };
                    for index in 0..(length - header_len) / size_of::<libc::c_int>() {
                        let raw =
                            unsafe { ptr::read_unaligned(data.cast::<libc::c_int>().add(index)) };
                        if raw < 0 {
                            return Err(invalid("invalid received descriptor"));
                        }
                        descriptors.push(unsafe { OwnedFd::from_raw_fd(raw) });
                    }
                }
                header = unsafe { libc::CMSG_NXTHDR(&message, header) };
            }
            if message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 || received < 4 {
                return Err(invalid("truncated video socket message"));
            }
            if le32(&body, 0) != 2 {
                continue;
            }
            if received != 12 || descriptors.len() != 1 {
                return Err(invalid("invalid video descriptor message"));
            }
            let descriptor = &descriptors[0];
            let length = le32(&body, 8) as usize;
            if !(96..=2 * 1024 * 1024).contains(&length) {
                return Err(invalid("invalid video mapping size"));
            }
            let mut stat: libc::stat = unsafe { mem::zeroed() };
            if unsafe { libc::fstat(descriptor.as_raw_fd(), &mut stat) } < 0 {
                return Err(io::Error::last_os_error());
            }
            if stat.st_size < length as libc::off_t {
                return Err(invalid("video mapping exceeds descriptor size"));
            }
            let mapping = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    length,
                    libc::PROT_READ,
                    libc::MAP_PRIVATE,
                    descriptor.as_raw_fd(),
                    0,
                )
            };
            if mapping == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            let frame = decode(unsafe { std::slice::from_raw_parts(mapping.cast(), length) });
            let unmap = unsafe { libc::munmap(mapping, length) };
            if unmap < 0 {
                return Err(io::Error::last_os_error());
            }
            return frame;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixDatagram;

    #[test]
    fn times_out_when_source_keeps_the_socket_open_without_frames() {
        let (receiver, _peer) = UnixDatagram::pair().unwrap();
        let mut capture = Capture {
            socket: receiver.into(),
        };
        let error = capture
            .next_frame_until(Instant::now() + Duration::from_millis(100))
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn service_messages_do_not_extend_the_frame_deadline() {
        let (receiver, peer) = UnixDatagram::pair().unwrap();
        let mut capture = Capture {
            socket: receiver.into(),
        };
        let sender = std::thread::spawn(move || {
            for _ in 0..20 {
                peer.send(&1u32.to_le_bytes()).unwrap();
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let error = capture
            .next_frame_until(Instant::now() + Duration::from_millis(100))
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        sender.join().unwrap();
    }

    fn buffer() -> Vec<u8> {
        let mut bytes = vec![0xee; 128];
        for (offset, value) in [(0, u32::MAX), (8, 38), (12, 128), (64, 1), (80, 6)] {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes[72..80].copy_from_slice(&1234567u64.to_le_bytes());
        bytes[96..102].copy_from_slice(&[0, 0, 0, 1, 0x40, 1]);
        bytes
    }

    #[test]
    fn extracts_exact_payload_and_pts_without_padding() {
        let frame = decode(&buffer()).unwrap();
        assert_eq!(frame.data, [0, 0, 0, 1, 0x40, 1]);
        assert_eq!(frame.pts, 1234567);
    }

    #[test]
    fn rejects_truncated_mappings() {
        let bytes = buffer();
        for length in 0..bytes.len() {
            assert!(decode(&bytes[..length]).is_err());
        }
    }

    #[test]
    fn rejects_malformed_headers_lengths_and_prefix() {
        for offset in [0, 8, 12, 64, 80, 96] {
            let mut bytes = buffer();
            bytes[offset] ^= 0xff;
            assert!(decode(&bytes).is_err(), "offset {offset}");
        }
        let mut bytes = buffer();
        bytes[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[80..84].copy_from_slice(&(u32::MAX - 32).to_le_bytes());
        assert!(decode(&bytes).is_err());
    }
}
