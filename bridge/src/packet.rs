//! A raw `AF_PACKET` channel on one interface, through `pnet_datalink`: bound to the interface,
//! promiscuous, reading every protocol.
//!
//! The kernel never reads a frame back to the socket that sent it, so the frames IOL transmits
//! do not come back. Other frames sent on the interface come from the container's own network
//! stack, with the interface's MAC address as their source; they are skipped by that address,
//! as `pnet_datalink` does not report a frame's packet type.

use std::io;
use std::ops::Range;
use std::sync::Mutex;
use std::time::Duration;

use pnet_datalink::{
    Channel, Config, DataLinkReceiver, DataLinkSender, MacAddr, NetworkInterface, channel,
    interfaces,
};

use crate::port::{Wire, is_timeout};

/// Larger than any frame: the container's veths can be 9600 bytes.
const READ_BUFFER: usize = 65_536;
/// Where an Ethernet frame's source address is.
const SOURCE_MAC: Range<usize> = 6..12;

/// Interface `name` when it exists and is up. A packet socket bound to an interface that is down
/// reports an error on every read, so a port waits for the interface to come up, and a port whose
/// interface goes down detaches.
pub fn find_interface(name: &str) -> Option<NetworkInterface> {
    interfaces()
        .into_iter()
        .find(|interface| interface.name == name && interface.is_up())
}

/// Whether `frame` was sent by the interface whose address is `mac`, which must not go back.
pub fn is_sent_by(frame: &[u8], mac: MacAddr) -> bool {
    frame.get(SOURCE_MAC) == Some(mac.octets().as_slice())
}

/// A packet channel on one interface.
pub struct PacketSocket {
    name: String,
    ifindex: u32,
    mac: Option<MacAddr>,
    sender: Mutex<Box<dyn DataLinkSender>>,
    receiver: Mutex<Box<dyn DataLinkReceiver>>,
}

impl PacketSocket {
    /// Open a packet channel on `interface`. Reads time out after `timeout`, so the reader can
    /// notice that the interface went away.
    pub fn open(interface: &NetworkInterface, timeout: Duration) -> io::Result<Self> {
        let config = Config {
            read_buffer_size: READ_BUFFER,
            read_timeout: Some(timeout),
            promiscuous: true,
            ..Config::default()
        };
        let Channel::Ethernet(sender, receiver) = channel(interface, config)? else {
            return Err(io::Error::other("not an Ethernet channel"));
        };
        Ok(Self {
            name: interface.name.clone(),
            ifindex: interface.index,
            mac: interface.mac,
            sender: Mutex::new(sender),
            receiver: Mutex::new(receiver),
        })
    }
}

impl Wire for PacketSocket {
    fn recv(&self, buf: &mut [u8]) -> io::Result<Option<usize>> {
        let Ok(mut receiver) = self.receiver.lock() else {
            return Err(io::Error::other("packet receiver lock poisoned"));
        };
        let frame = match receiver.next() {
            Ok(frame) => frame,
            Err(error) if is_timeout(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        if self.mac.is_some_and(|mac| is_sent_by(frame, mac)) {
            return Ok(None);
        }
        let len = frame.len().min(buf.len());
        buf[..len].copy_from_slice(&frame[..len]);
        Ok(Some(len))
    }

    fn send(&self, frame: &[u8]) -> io::Result<()> {
        let Ok(mut sender) = self.sender.lock() else {
            return Err(io::Error::other("packet sender lock poisoned"));
        };
        sender
            .send_to(frame, None)
            .unwrap_or_else(|| Err(io::Error::other("packet channel cannot send")))
    }

    fn is_attached(&self) -> bool {
        find_interface(&self.name).is_some_and(|interface| interface.index == self.ifindex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWN: MacAddr = MacAddr(0x02, 0x42, 0xac, 0x11, 0x00, 0x02);

    /// An Ethernet frame from `source` to the broadcast address.
    fn frame_from(source: MacAddr) -> Vec<u8> {
        let mut frame = vec![0xff; 6];
        frame.extend_from_slice(&source.octets());
        frame.extend_from_slice(&[0x08, 0x06, 0, 1]);
        frame
    }

    #[test]
    fn a_frame_from_the_interfaces_own_address_is_sent_by_it() {
        assert!(is_sent_by(&frame_from(OWN), OWN));
    }

    #[test]
    fn a_frame_from_iol_or_the_peer_is_not_sent_by_the_interface() {
        let iol = MacAddr(0xaa, 0xbb, 0xcc, 0x00, 0x01, 0x00);
        assert!(!is_sent_by(&frame_from(iol), OWN));
    }

    #[test]
    fn a_frame_to_the_interfaces_address_is_not_sent_by_it() {
        let mut frame = OWN.octets().to_vec();
        frame.extend_from_slice(&[0xaa, 0xbb, 0xcc, 0x00, 0x01, 0x00, 0x08, 0x00]);
        assert!(!is_sent_by(&frame, OWN));
    }

    #[test]
    fn a_runt_frame_is_not_sent_by_the_interface() {
        assert!(!is_sent_by(&frame_from(OWN)[..10], OWN));
    }

    #[test]
    fn the_loopback_interface_is_up_and_found() {
        assert!(find_interface("lo").is_some());
    }

    #[test]
    fn a_missing_interface_is_not_found() {
        assert!(find_interface("no-such-if0").is_none());
    }
}
