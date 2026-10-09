//! A minimal Wayland client wire-protocol implementation.
//!
//! Only what the screencopy capture needs: connecting to the compositor
//! socket, encoding requests (including passing a file descriptor with
//! SCM_RIGHTS) and decoding events. Messages are an 8-byte header (object id,
//! then size << 16 | opcode) followed by 32-bit-aligned arguments in native
//! byte order.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// `wl_display` is always object 1.
pub const DISPLAY_ID: u32 = 1;
/// Largest message libwayland accepts.
const MAX_MESSAGE_SIZE: usize = 4096;
/// Client-allocated ids must stay below the server range (0xff000000..).
const MAX_CLIENT_ID: u32 = 0xfeff_ffff;

/// One decoded event: target object, opcode and raw argument bytes.
#[derive(Debug, Clone)]
pub struct Event {
    pub object: u32,
    pub opcode: u16,
    pub args: Vec<u8>,
}

impl Event {
    pub fn reader(&self) -> ArgReader<'_> {
        ArgReader { data: &self.args, pos: 0 }
    }
}

pub struct Connection {
    socket: UnixStream,
    next_id: u32,
    free_ids: Vec<u32>,
    /// Bytes received but not yet decoded into events.
    inbuf: Vec<u8>,
    events: VecDeque<Event>,
}

impl Connection {
    /// Connects to `$WAYLAND_DISPLAY` (default `wayland-0`) in `$XDG_RUNTIME_DIR`.
    pub fn connect() -> io::Result<Connection> {
        let display = std::env::var_os("WAYLAND_DISPLAY").unwrap_or_else(|| "wayland-0".into());
        let mut path = PathBuf::from(&display);
        if !path.is_absolute() {
            let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set (not in a Wayland session?)")
            })?;
            path = PathBuf::from(runtime).join(display);
        }
        let socket = UnixStream::connect(&path)
            .map_err(|e| io::Error::new(e.kind(), format!("cannot connect to {}: {e}", path.display())))?;
        Ok(Connection { socket, next_id: 2, free_ids: Vec::new(), inbuf: Vec::new(), events: VecDeque::new() })
    }

    /// A fresh object id for a new_id argument.
    pub fn new_id(&mut self) -> io::Result<u32> {
        if let Some(id) = self.free_ids.pop() {
            return Ok(id);
        }
        if self.next_id > MAX_CLIENT_ID {
            return Err(io::Error::other("ran out of Wayland object ids"));
        }
        let id = self.next_id;
        self.next_id += 1;
        Ok(id)
    }

    /// Marks an id reusable; call on `wl_display.delete_id`.
    pub fn release_id(&mut self, id: u32) {
        if (2..=MAX_CLIENT_ID).contains(&id) && !self.free_ids.contains(&id) {
            self.free_ids.push(id);
        }
    }

    /// Sends one request, optionally passing a file descriptor alongside it.
    pub fn send(&mut self, msg: &Message, fd: Option<RawFd>) -> io::Result<()> {
        let bytes = msg.finish();
        match fd {
            None => io::Write::write_all(&mut self.socket, &bytes),
            Some(fd) => send_with_fd(self.socket.as_raw_fd(), &bytes, fd),
        }
    }

    /// Returns the next event, waiting at most until `deadline`.
    pub fn next_event(&mut self, deadline: Instant) -> io::Result<Event> {
        loop {
            if let Some(event) = self.events.pop_front() {
                return Ok(event);
            }
            let timeout = deadline.saturating_duration_since(Instant::now());
            if timeout.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out waiting for the compositor"));
            }
            self.fill(timeout)?;
        }
    }

    /// Waits for readable data (up to `timeout`), reads it and decodes complete messages.
    fn fill(&mut self, timeout: Duration) -> io::Result<()> {
        let fd = self.socket.as_raw_fd();
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let ms = timeout.as_millis().clamp(1, i32::MAX as u128) as i32;
        // SAFETY: one valid pollfd.
        let ready = unsafe { libc::poll(&mut pfd, 1, ms) };
        if ready < 0 {
            let err = io::Error::last_os_error();
            return if err.kind() == io::ErrorKind::Interrupted { Ok(()) } else { Err(err) };
        }
        if ready == 0 {
            return Ok(());
        }

        let mut buf = [0u8; 8192];
        let n = recv_discarding_fds(fd, &mut buf)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "compositor closed the Wayland connection"));
        }
        self.inbuf.extend_from_slice(&buf[..n]);

        let mut pos = 0;
        while self.inbuf.len() - pos >= 8 {
            let object = u32::from_ne_bytes(self.inbuf[pos..pos + 4].try_into().unwrap());
            let word = u32::from_ne_bytes(self.inbuf[pos + 4..pos + 8].try_into().unwrap());
            let size = (word >> 16) as usize;
            let opcode = (word & 0xffff) as u16;
            if size < 8 || size % 4 != 0 || size > MAX_MESSAGE_SIZE {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("bad Wayland message size {size}")));
            }
            if self.inbuf.len() - pos < size {
                break;
            }
            let args = self.inbuf[pos + 8..pos + size].to_vec();
            self.events.push_back(Event { object, opcode, args });
            pos += size;
        }
        self.inbuf.drain(..pos);
        Ok(())
    }
}

/// Builder for one request.
pub struct Message {
    object: u32,
    opcode: u16,
    body: Vec<u8>,
}

impl Message {
    pub fn new(object: u32, opcode: u16) -> Message {
        Message { object, opcode, body: Vec::with_capacity(32) }
    }

    pub fn uint(mut self, v: u32) -> Message {
        self.body.extend_from_slice(&v.to_ne_bytes());
        self
    }

    pub fn int(mut self, v: i32) -> Message {
        self.body.extend_from_slice(&v.to_ne_bytes());
        self
    }

    /// Object ids and new_ids are plain u32s on the wire.
    pub fn object(self, id: u32) -> Message {
        self.uint(id)
    }

    /// Length (including the NUL), bytes, NUL, padding to 4 bytes.
    pub fn string(mut self, s: &str) -> Message {
        let len = s.len() + 1;
        self.body.extend_from_slice(&(len as u32).to_ne_bytes());
        self.body.extend_from_slice(s.as_bytes());
        self.body.push(0);
        while self.body.len() % 4 != 0 {
            self.body.push(0);
        }
        self
    }

    fn finish(&self) -> Vec<u8> {
        let size = 8 + self.body.len();
        debug_assert!(size <= MAX_MESSAGE_SIZE);
        let mut out = Vec::with_capacity(size);
        out.extend_from_slice(&self.object.to_ne_bytes());
        out.extend_from_slice(&(((size as u32) << 16) | self.opcode as u32).to_ne_bytes());
        out.extend_from_slice(&self.body);
        out
    }
}

/// Sequential reader for event arguments.
pub struct ArgReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl ArgReader<'_> {
    pub fn uint(&mut self) -> io::Result<u32> {
        let bytes = self.data.get(self.pos..self.pos + 4).ok_or_else(truncated)?;
        self.pos += 4;
        Ok(u32::from_ne_bytes(bytes.try_into().unwrap()))
    }

    pub fn int(&mut self) -> io::Result<i32> {
        self.uint().map(|v| v as i32)
    }

    /// Returns the string without its NUL terminator; a null string reads as "".
    pub fn string(&mut self) -> io::Result<String> {
        let len = self.uint()? as usize;
        if len == 0 {
            return Ok(String::new());
        }
        let padded = len.div_ceil(4) * 4;
        let bytes = self.data.get(self.pos..self.pos + padded).ok_or_else(truncated)?;
        self.pos += padded;
        Ok(String::from_utf8_lossy(&bytes[..len - 1]).into_owned())
    }
}

fn truncated() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "truncated Wayland event")
}

/// `sendmsg` with one fd in an SCM_RIGHTS control message.
fn send_with_fd(socket: RawFd, bytes: &[u8], fd: RawFd) -> io::Result<()> {
    let mut iov = libc::iovec { iov_base: bytes.as_ptr() as *mut libc::c_void, iov_len: bytes.len() };
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    // u64 storage keeps the control buffer aligned for cmsghdr.
    let mut control = vec![0u64; space.div_ceil(8)];
    // SAFETY: msghdr is plain data; every pointer set below outlives the call.
    let sent = unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = space as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg) as *mut RawFd, fd);
        libc::sendmsg(socket, &msg, libc::MSG_NOSIGNAL)
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    // The fd travels with the first byte; send whatever did not fit normally.
    if (sent as usize) < bytes.len() {
        // SAFETY: borrowing the fd only for this write; ownership stays with the caller.
        let mut stream = std::mem::ManuallyDrop::new(unsafe { UnixStream::from_raw_fd(socket) });
        io::Write::write_all(&mut *stream, &bytes[sent as usize..])?;
    }
    Ok(())
}

/// `recvmsg` that closes any file descriptors the compositor sends (none of
/// the events we use carry one) so they don't leak.
fn recv_discarding_fds(socket: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut libc::c_void, iov_len: buf.len() };
    let mut control = [0u64; 32];
    loop {
        // SAFETY: msghdr points at live buffers for the duration of the call.
        let (n, msg) = unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = std::mem::size_of_val(&control) as _;
            let n = libc::recvmsg(socket, &mut msg, libc::MSG_CMSG_CLOEXEC);
            (n, msg)
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        // SAFETY: walking the control messages recvmsg just filled in.
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                    let data = libc::CMSG_DATA(cmsg) as *const RawFd;
                    let count = ((*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / std::mem::size_of::<RawFd>();
                    for i in 0..count {
                        drop(OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i))));
                    }
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
        }
        return Ok(n as usize);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_header_and_string_padding() {
        let bytes = Message::new(2, 0).uint(7).string("wl_shm").uint(1).object(3).finish();
        // header + uint + (len + "wl_shm\0" padded to 8) + uint + uint
        assert_eq!(bytes.len(), 8 + 4 + 4 + 8 + 4 + 4);
        assert_eq!(u32::from_ne_bytes(bytes[0..4].try_into().unwrap()), 2);
        assert_eq!(u32::from_ne_bytes(bytes[4..8].try_into().unwrap()), 32 << 16); // size 32, opcode 0
        assert_eq!(u32::from_ne_bytes(bytes[12..16].try_into().unwrap()), 7); // "wl_shm" + NUL
        assert_eq!(&bytes[16..23], b"wl_shm\0");
        assert_eq!(bytes[23], 0);
    }

    #[test]
    fn reads_event_arguments() {
        let msg = Message::new(9, 3).int(-5).string("HDMI-A-1").uint(42);
        let bytes = msg.finish();
        let event = Event { object: 9, opcode: 3, args: bytes[8..].to_vec() };
        let mut r = event.reader();
        assert_eq!(r.int().unwrap(), -5);
        assert_eq!(r.string().unwrap(), "HDMI-A-1");
        assert_eq!(r.uint().unwrap(), 42);
        assert!(r.uint().is_err());
    }

    #[test]
    fn ids_are_reused_after_release() {
        let (a, _b) = UnixStream::pair().unwrap();
        let mut c =
            Connection { socket: a, next_id: 2, free_ids: Vec::new(), inbuf: Vec::new(), events: VecDeque::new() };
        assert_eq!(c.new_id().unwrap(), 2);
        assert_eq!(c.new_id().unwrap(), 3);
        c.release_id(2);
        assert_eq!(c.new_id().unwrap(), 2);
        assert_eq!(c.new_id().unwrap(), 4);
    }

    #[test]
    fn decodes_split_messages_and_passes_fds() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut client =
            Connection { socket: a, next_id: 2, free_ids: Vec::new(), inbuf: Vec::new(), events: VecDeque::new() };
        let mut server = b;
        let ev = Message::new(5, 1).uint(11).string("x").finish();
        // Send the message in two pieces to exercise partial reads.
        io::Write::write_all(&mut server, &ev[..6]).unwrap();
        let deadline = Instant::now() + Duration::from_millis(200);
        assert!(client.next_event(deadline).is_err()); // incomplete: times out
        io::Write::write_all(&mut server, &ev[6..]).unwrap();
        let e = client.next_event(Instant::now() + Duration::from_secs(1)).unwrap();
        assert_eq!((e.object, e.opcode), (5, 1));
        assert_eq!(e.reader().uint().unwrap(), 11);

        // fd passing: the peer receives the message bytes and a working fd.
        let file = std::fs::File::open("/dev/null").unwrap();
        client.send(&Message::new(1, 1).uint(99), Some(file.as_raw_fd())).unwrap();
        let mut buf = [0u8; 64];
        let n = recv_discarding_fds(server.as_raw_fd(), &mut buf).unwrap();
        assert_eq!(n, 12);
        assert_eq!(u32::from_ne_bytes(buf[8..12].try_into().unwrap()), 99);
    }
}
