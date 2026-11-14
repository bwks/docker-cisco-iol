//! Live captures on one interface, through libpcap: one that reads the frames arriving on it,
//! one that transmits.
//!
//! The kernel takes the 802.1Q tag off a received frame and passes it beside the frame, in the
//! packet socket's auxiliary data. libpcap puts the tag back, so IOL receives a trunk's frames
//! tagged; a plain packet socket would hand them over untagged.
//!
//! The reading capture takes only frames coming in (`PCAP_D_IN`). Frames going out on the
//! interface, which are the frames IOL transmits through the other capture and those of the
//! container's own network stack, are never read, so nothing IOL sends comes back to it.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use pcap::{Active, Capture, Direction, Error};

use crate::port::Wire;

/// Where the kernel describes the container's interfaces.
const SYS_CLASS_NET: &str = "/sys/class/net";
/// `IFF_UP`, the interface flag set when the interface is up.
const IFF_UP: u32 = 0x1;
/// The largest MTU Sherpa gives a link.
const MAX_MTU: i32 = 9600;
/// An Ethernet header, and the 802.1Q tag libpcap puts back.
const ETHERNET_HEADER: i32 = 14;
const VLAN_TAG: i32 = 4;
/// The longest frame read: the largest MTU, its Ethernet header and a VLAN tag.
pub const SNAPLEN: i32 = MAX_MTU + ETHERNET_HEADER + VLAN_TAG;
/// A filter no frame matches, for the capture that only transmits: the kernel then queues it
/// nothing.
const MATCH_NOTHING: &str = "less 1";

/// The index of interface `name` when it exists and is up. A capture on an interface that is
/// down fails, so a port waits for the interface to come up, and a port whose interface goes
/// down detaches. The index tells an interface made again under the same name from the old one.
pub fn find_interface(name: &str) -> Option<u32> {
    let dir = Path::new(SYS_CLASS_NET).join(name);
    let flags = parse_flags(&fs::read_to_string(dir.join("flags")).ok()?)?;
    if flags & IFF_UP == 0 {
        return None;
    }
    fs::read_to_string(dir.join("ifindex"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Interface flags as `/sys/class/net/<name>/flags` gives them, in hex: `0x1003`.
fn parse_flags(text: &str) -> Option<u32> {
    let hex = text.trim().strip_prefix("0x")?;
    u32::from_str_radix(hex, 16).ok()
}

/// A libpcap error as an I/O error.
fn to_io(error: Error) -> io::Error {
    io::Error::other(error)
}

/// Whether a frame of `len` bytes was read whole, `captured` bytes of it. A cut frame is not
/// worth relaying.
fn is_whole(captured: u32, len: u32) -> bool {
    captured >= len
}

/// The two captures on one interface.
pub struct PacketSocket {
    name: String,
    ifindex: u32,
    reader: Mutex<Capture<Active>>,
    writer: Mutex<Capture<Active>>,
}

impl PacketSocket {
    /// Open the captures on interface `name`, whose index is `ifindex`. Reads time out after
    /// `timeout`, so the reader can notice that the interface went away.
    pub fn open(name: &str, ifindex: u32, timeout: Duration) -> io::Result<Self> {
        let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        let reader = Capture::from_device(name)
            .and_then(|capture| {
                capture
                    .promisc(true)
                    .snaplen(SNAPLEN)
                    .immediate_mode(true)
                    .timeout(timeout_ms)
                    .open()
            })
            .map_err(to_io)?;
        reader.direction(Direction::In).map_err(to_io)?;
        let mut writer = Capture::from_device(name)
            .and_then(|capture| capture.snaplen(SNAPLEN).open())
            .map_err(to_io)?;
        writer.filter(MATCH_NOTHING, true).map_err(to_io)?;
        Ok(Self {
            name: name.to_owned(),
            ifindex,
            reader: Mutex::new(reader),
            writer: Mutex::new(writer),
        })
    }
}

impl Wire for PacketSocket {
    fn recv(&self, buf: &mut [u8]) -> io::Result<Option<usize>> {
        let Ok(mut reader) = self.reader.lock() else {
            return Err(io::Error::other("packet reader lock poisoned"));
        };
        let packet = match reader.next_packet() {
            Ok(packet) => packet,
            Err(Error::TimeoutExpired) => return Ok(None),
            Err(error) => return Err(to_io(error)),
        };
        if !is_whole(packet.header.caplen, packet.header.len) {
            eprintln!(
                "iol-bridge: {}: dropped a {}-byte frame longer than {SNAPLEN} bytes",
                self.name, packet.header.len
            );
            return Ok(None);
        }
        let len = packet.data.len().min(buf.len());
        buf[..len].copy_from_slice(&packet.data[..len]);
        Ok(Some(len))
    }

    fn send(&self, frame: &[u8]) -> io::Result<()> {
        let Ok(mut writer) = self.writer.lock() else {
            return Err(io::Error::other("packet writer lock poisoned"));
        };
        writer.sendpacket(frame).map_err(to_io)
    }

    fn is_attached(&self) -> bool {
        find_interface(&self.name) == Some(self.ifindex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_flags_are_read_as_hex() {
        assert_eq!(parse_flags("0x1003\n"), Some(0x1003));
        assert_eq!(parse_flags("0x1002"), Some(0x1002));
    }

    #[test]
    fn interface_flags_not_in_hex_are_refused() {
        for bad in ["", "1003", "0x", "0xzz"] {
            assert_eq!(parse_flags(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_snaplen_holds_a_jumbo_frame_with_its_vlan_tag() {
        // 9600-byte payload, destination and source MACs, the 802.1Q tag and the EtherType.
        assert_eq!(SNAPLEN, 9600 + 6 + 6 + 4 + 2);
    }

    #[test]
    fn a_frame_read_whole_is_relayed_and_a_cut_one_is_not() {
        assert!(is_whole(1518, 1518));
        assert!(!is_whole(9618, 9622));
    }

    #[test]
    fn the_loopback_interface_is_up_and_found() {
        assert_eq!(find_interface("lo"), Some(1));
    }

    #[test]
    fn a_missing_interface_is_not_found() {
        assert!(find_interface("no-such-if0").is_none());
    }
}
