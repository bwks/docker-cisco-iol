//! A raw `AF_PACKET` socket on one interface, through the few C library calls the standard
//! library does not wrap. The bridge has no dependencies, so the declarations are here.

use std::ffi::{CString, c_char, c_int, c_uint, c_void};
use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

use crate::port::Wire;

const AF_PACKET: c_int = 17;
const SOCK_RAW: c_int = 3;
const SOCK_CLOEXEC: c_int = 0o2_000_000;
/// `ETH_P_ALL`: every protocol.
const ETH_P_ALL: u16 = 0x0003;
const SOL_SOCKET: c_int = 1;
const SO_RCVTIMEO: c_int = 20;
const SOL_PACKET: c_int = 263;
const PACKET_ADD_MEMBERSHIP: c_int = 1;
const PACKET_MR_PROMISC: u16 = 1;
const PACKET_IGNORE_OUTGOING: c_int = 23;
/// `pkttype` of a frame this host sent.
const PACKET_OUTGOING: u8 = 4;

/// `struct sockaddr_ll`.
#[repr(C)]
#[derive(Default)]
struct SockaddrLl {
    family: u16,
    protocol: u16,
    ifindex: c_int,
    hatype: u16,
    pkttype: u8,
    halen: u8,
    addr: [u8; 8],
}

/// `struct packet_mreq`.
#[repr(C)]
struct PacketMreq {
    ifindex: c_int,
    kind: u16,
    alen: u16,
    address: [u8; 8],
}

/// `struct timeval` on 64-bit Linux.
#[repr(C)]
struct Timeval {
    tv_sec: i64,
    tv_usec: i64,
}

unsafe extern "C" {
    fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    fn bind(fd: c_int, addr: *const c_void, len: c_uint) -> c_int;
    fn setsockopt(fd: c_int, level: c_int, name: c_int, value: *const c_void, len: c_uint)
    -> c_int;
    fn recvfrom(
        fd: c_int,
        buf: *mut c_void,
        len: usize,
        flags: c_int,
        addr: *mut c_void,
        addr_len: *mut c_uint,
    ) -> isize;
    fn send(fd: c_int, buf: *const c_void, len: usize, flags: c_int) -> isize;
    fn if_nametoindex(name: *const c_char) -> c_uint;
}

/// The index of interface `name`, or `None` when it does not exist.
pub fn interface_index(name: &str) -> Option<u32> {
    let name = CString::new(name).ok()?;
    // SAFETY: `name` is a valid NUL-terminated string that outlives the call.
    let index = unsafe { if_nametoindex(name.as_ptr()) };
    (index != 0).then_some(index)
}

/// Whether a frame the packet socket read is one this host sent, which must not go back.
pub fn is_outgoing(pkttype: u8) -> bool {
    pkttype == PACKET_OUTGOING
}

/// The size of `T` as a socket length.
fn socklen<T>() -> c_uint {
    c_uint::try_from(size_of::<T>()).unwrap_or(c_uint::MAX)
}

/// A packet socket bound to interface `name`, promiscuous, reading every protocol.
pub struct PacketSocket {
    fd: OwnedFd,
    name: String,
    ifindex: u32,
}

impl PacketSocket {
    /// Open a packet socket on interface `name`, whose index is `ifindex`. Reads time out after
    /// `timeout`, so the reader can notice that the interface went away.
    pub fn open(name: &str, ifindex: u32, timeout: Duration) -> io::Result<Self> {
        let protocol = ETH_P_ALL.to_be();
        // SAFETY: plain system call with constant arguments.
        let raw = unsafe { socket(AF_PACKET, SOCK_RAW | SOCK_CLOEXEC, c_int::from(protocol)) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a socket this function just opened and nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let ifindex_c =
            c_int::try_from(ifindex).map_err(|error| io::Error::other(error.to_string()))?;

        let address = SockaddrLl {
            family: u16::try_from(AF_PACKET).unwrap_or_default(),
            protocol,
            ifindex: ifindex_c,
            ..SockaddrLl::default()
        };
        // SAFETY: `address` is a valid `sockaddr_ll` and the length is its size.
        let bound = unsafe {
            bind(
                fd.as_raw_fd(),
                (&raw const address).cast(),
                socklen::<SockaddrLl>(),
            )
        };
        if bound < 0 {
            return Err(io::Error::last_os_error());
        }

        let membership = PacketMreq {
            ifindex: ifindex_c,
            kind: PACKET_MR_PROMISC,
            alen: 0,
            address: [0; 8],
        };
        set_option(&fd, SOL_PACKET, PACKET_ADD_MEMBERSHIP, &membership)?;

        // Older kernels do not have it; outgoing frames are also skipped by type on read.
        let ignore: c_int = 1;
        let _ = set_option(&fd, SOL_PACKET, PACKET_IGNORE_OUTGOING, &ignore);

        let timeval = Timeval {
            tv_sec: i64::try_from(timeout.as_secs()).unwrap_or(i64::MAX),
            tv_usec: i64::from(timeout.subsec_micros()),
        };
        set_option(&fd, SOL_SOCKET, SO_RCVTIMEO, &timeval)?;

        Ok(Self {
            fd,
            name: name.to_owned(),
            ifindex,
        })
    }
}

/// Set socket option `name` at `level` to `value`.
fn set_option<T>(fd: &OwnedFd, level: c_int, name: c_int, value: &T) -> io::Result<()> {
    // SAFETY: `value` points to a live `T` and the length is its size.
    let result = unsafe {
        setsockopt(
            fd.as_raw_fd(),
            level,
            name,
            (&raw const *value).cast(),
            socklen::<T>(),
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl Wire for PacketSocket {
    fn recv(&self, buf: &mut [u8]) -> io::Result<Option<usize>> {
        let mut address = SockaddrLl::default();
        let mut address_len = socklen::<SockaddrLl>();
        // SAFETY: `buf` and `address` are valid for writes of the lengths given.
        let read = unsafe {
            recvfrom(
                self.fd.as_raw_fd(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                0,
                (&raw mut address).cast(),
                &raw mut address_len,
            )
        };
        if read < 0 {
            let error = io::Error::last_os_error();
            return match error.kind() {
                io::ErrorKind::WouldBlock
                | io::ErrorKind::TimedOut
                | io::ErrorKind::Interrupted => Ok(None),
                _ => Err(error),
            };
        }
        if is_outgoing(address.pkttype) {
            return Ok(None);
        }
        Ok(Some(usize::try_from(read).unwrap_or_default()))
    }

    fn send(&self, frame: &[u8]) -> io::Result<()> {
        // SAFETY: `frame` is valid for reads of its length.
        let sent = unsafe { send(self.fd.as_raw_fd(), frame.as_ptr().cast(), frame.len(), 0) };
        if sent < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn is_attached(&self) -> bool {
        interface_index(&self.name) == Some(self.ifindex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_this_host_sent_is_outgoing() {
        assert!(is_outgoing(4));
    }

    #[test]
    fn frames_for_this_host_and_others_are_not_outgoing() {
        // PACKET_HOST, PACKET_BROADCAST, PACKET_MULTICAST, PACKET_OTHERHOST
        for pkttype in [0, 1, 2, 3] {
            assert!(!is_outgoing(pkttype));
        }
    }

    #[test]
    fn the_loopback_interface_has_an_index() {
        assert!(interface_index("lo").is_some());
    }

    #[test]
    fn a_missing_interface_has_no_index() {
        assert_eq!(interface_index("no-such-if0"), None);
    }

    #[test]
    fn a_name_with_a_nul_has_no_index() {
        assert_eq!(interface_index("eth\0"), None);
    }

    #[test]
    fn the_c_structs_have_the_kernel_sizes() {
        assert_eq!(size_of::<SockaddrLl>(), 20);
        assert_eq!(size_of::<PacketMreq>(), 16);
        assert_eq!(size_of::<Timeval>(), 16);
    }
}
