//! One IOL port: the relay between interface `eth<N>` and the launcher's sockets for port `N`.
//!
//! Cisco's launcher binds `s<NN>.sock` for each port and sends the frames IOL transmits to
//! `c<NN>.sock`. The bridge binds `c<NN>.sock`, so it is on the wire, and sends the frames it
//! reads from the interface to `s<NN>.sock`. The interface may appear, go and come back while
//! IOL runs; the socket stays bound throughout, so IOL never notices.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::packet::{PacketSocket, find_interface};

/// Where the launcher keeps its port sockets.
pub const SOCKET_DIR: &str = "/tmp";
/// IOL has at most 16 slots of 4 ports.
pub const MAX_PORTS: u16 = 64;
/// How often a waiting port looks for its interface, and how long a read waits before the
/// relay checks that its interface is still the one it opened.
const POLL: Duration = Duration::from_millis(500);
/// The mode of the bridge's port sockets: any user may send to them.
const SOCKET_MODE: u32 = 0o666;
/// Larger than any frame: the container's veths can be 9600 bytes.
const FRAME_BUFFER: usize = 65_536;

/// The interface side of a port: a packet socket, or a socket in tests.
pub trait Wire: Sync {
    /// Read one frame into `buf`. `None` when the read timed out or the frame is to be skipped.
    fn recv(&self, buf: &mut [u8]) -> io::Result<Option<usize>>;
    /// Transmit `frame`.
    fn send(&self, frame: &[u8]) -> io::Result<()>;
    /// Whether the interface this wire was opened on is still there.
    fn is_attached(&self) -> bool;
}

/// One port: its interface and the two launcher sockets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Port {
    pub index: u16,
    pub interface: String,
    /// `c<NN>.sock`, bound by the bridge.
    pub listen: PathBuf,
    /// `s<NN>.sock`, bound by the launcher.
    pub launcher: PathBuf,
}

impl Port {
    pub fn new(index: u16, socket_dir: &Path) -> Self {
        Self {
            index,
            interface: format!("eth{index}"),
            listen: socket_dir.join(format!("c{index:02}.sock")),
            launcher: socket_dir.join(format!("s{index:02}.sock")),
        }
    }
}

/// The number of ports from the command line: one argument, 1 to `MAX_PORTS`.
pub fn parse_port_count(args: &[String]) -> Result<u16, String> {
    let [count] = args else {
        return Err(format!(
            "usage: iol-bridge <ports>, the number of IOL Ethernet ports (1 to {MAX_PORTS})"
        ));
    };
    match count.parse::<u16>() {
        Ok(ports) if (1..=MAX_PORTS).contains(&ports) => Ok(ports),
        _ => Err(format!(
            "invalid port count '{count}': expected 1 to {MAX_PORTS}"
        )),
    }
}

/// Run port `port` for good: bind its socket, then attach to its interface whenever it exists
/// and is up. Returns only when the socket cannot be bound.
pub fn run(port: &Port) -> io::Result<()> {
    match fs::remove_file(&port.listen) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let socket = UnixDatagram::bind(&port.listen)?;
    // The launcher may run as another user, which must be able to send to it.
    fs::set_permissions(&port.listen, fs::Permissions::from_mode(SOCKET_MODE))?;
    socket.set_read_timeout(Some(POLL))?;
    loop {
        let Some(ifindex) = find_interface(&port.interface) else {
            discard(&socket, POLL);
            continue;
        };
        match PacketSocket::open(&port.interface, ifindex, POLL) {
            Ok(wire) => {
                eprintln!(
                    "iol-bridge: {} attached (ifindex {ifindex})",
                    port.interface
                );
                relay(&socket, &port.launcher, &wire);
                eprintln!("iol-bridge: {} detached", port.interface);
            }
            Err(error) => {
                eprintln!("iol-bridge: cannot open {}: {error}", port.interface);
                discard(&socket, POLL);
            }
        }
    }
}

/// Drop the frames IOL sends on a port with no interface for `period`, as a wire with nothing on
/// it would. Left unread, they fill the socket and the launcher stalls on every port.
fn discard(socket: &UnixDatagram, period: Duration) {
    let deadline = Instant::now() + period;
    let mut buf = vec![0; FRAME_BUFFER];
    while Instant::now() < deadline {
        if let Err(error) = socket.recv(&mut buf)
            && !is_timeout(&error)
        {
            thread::sleep(deadline.saturating_duration_since(Instant::now()));
        }
    }
}

/// Relay frames both ways between `wire` and the launcher until the wire fails or its
/// interface goes away.
pub fn relay(socket: &UnixDatagram, launcher: &Path, wire: &impl Wire) {
    let attached = AtomicBool::new(true);
    thread::scope(|scope| {
        scope.spawn(|| wire_to_launcher(wire, socket, launcher, &attached));
        launcher_to_wire(socket, wire, &attached);
    });
}

/// Frames read on the interface go to the launcher. A launcher not listening yet drops them.
fn wire_to_launcher(
    wire: &impl Wire,
    socket: &UnixDatagram,
    launcher: &Path,
    attached: &AtomicBool,
) {
    let mut buf = vec![0; FRAME_BUFFER];
    while attached.load(Ordering::Relaxed) {
        match wire.recv(&mut buf) {
            Ok(Some(len)) => {
                // The launcher is not up yet, or restarting: the frame is lost, as on a wire.
                let _ = socket.send_to(&buf[..len], launcher);
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!("iol-bridge: read failed: {error}");
                attached.store(false, Ordering::Relaxed);
            }
        }
    }
}

/// Frames IOL transmits go out on the interface. Between frames the relay checks that its
/// interface is still the one it opened, as a veth made again has a new index.
fn launcher_to_wire(socket: &UnixDatagram, wire: &impl Wire, attached: &AtomicBool) {
    let mut buf = vec![0; FRAME_BUFFER];
    while attached.load(Ordering::Relaxed) {
        match socket.recv(&mut buf) {
            Ok(len) => {
                // A send fails for the frame, or because the interface is gone or down.
                if wire.send(&buf[..len]).is_err() && !wire.is_attached() {
                    attached.store(false, Ordering::Relaxed);
                }
            }
            Err(error) if is_timeout(&error) => {
                if !wire.is_attached() {
                    attached.store(false, Ordering::Relaxed);
                }
            }
            Err(error) => {
                eprintln!("iol-bridge: launcher socket read failed: {error}");
                thread::sleep(POLL);
            }
        }
    }
}

/// Whether a read ended without a frame because it timed out or was interrupted.
pub fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

    /// A fresh directory for one test's sockets.
    fn test_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "iol-bridge-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A wire made of one end of a socket pair; the test holds the other end.
    struct PairWire {
        socket: UnixDatagram,
        attached: AtomicBool,
    }

    impl Wire for PairWire {
        fn recv(&self, buf: &mut [u8]) -> io::Result<Option<usize>> {
            match self.socket.recv(buf) {
                Ok(len) => Ok(Some(len)),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
                Err(error) => Err(error),
            }
        }

        fn send(&self, frame: &[u8]) -> io::Result<()> {
            self.socket.send(frame).map(|_| ())
        }

        fn is_attached(&self) -> bool {
            self.attached.load(Ordering::Relaxed)
        }
    }

    /// Takes the wire's interface away when dropped, so the relay stops on every way out of a
    /// test, a failed assertion included. Without it a panic in `thread::scope` waits forever
    /// to join a relay that is still attached.
    struct Unplug<'a>(&'a PairWire);

    impl Drop for Unplug<'_> {
        fn drop(&mut self) {
            self.0.attached.store(false, Ordering::Relaxed);
        }
    }

    fn pair_wire() -> (PairWire, UnixDatagram) {
        let (wire, peer) = UnixDatagram::pair().unwrap();
        wire.set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        (
            PairWire {
                socket: wire,
                attached: AtomicBool::new(true),
            },
            peer,
        )
    }

    #[test]
    fn port_names_follow_the_launcher() {
        let port = Port::new(5, Path::new("/tmp"));
        assert_eq!(port.interface, "eth5");
        assert_eq!(port.listen, PathBuf::from("/tmp/c05.sock"));
        assert_eq!(port.launcher, PathBuf::from("/tmp/s05.sock"));
    }

    #[test]
    fn port_numbers_above_nine_keep_two_digits() {
        let port = Port::new(15, Path::new("/tmp"));
        assert_eq!(port.interface, "eth15");
        assert_eq!(port.listen, PathBuf::from("/tmp/c15.sock"));
    }

    #[test]
    fn the_port_count_is_one_to_sixty_four() {
        assert_eq!(parse_port_count(&["16".to_owned()]), Ok(16));
        assert_eq!(parse_port_count(&["1".to_owned()]), Ok(1));
        assert_eq!(parse_port_count(&["64".to_owned()]), Ok(64));
    }

    #[test]
    fn a_port_count_out_of_range_or_not_a_number_is_refused() {
        for bad in ["0", "65", "-1", "sixteen", ""] {
            assert!(parse_port_count(&[bad.to_owned()]).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_port_count_is_the_only_argument() {
        assert!(parse_port_count(&[]).is_err());
        assert!(parse_port_count(&["16".to_owned(), "1".to_owned()]).is_err());
    }

    #[test]
    fn frames_cross_both_ways_until_the_interface_goes() {
        let dir = test_dir();
        let port = Port::new(1, &dir);
        let launcher = UnixDatagram::bind(&port.launcher).unwrap();
        launcher
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let socket = UnixDatagram::bind(&port.listen).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let (wire, peer) = pair_wire();

        thread::scope(|scope| {
            let unplug = Unplug(&wire);
            let relay_thread = scope.spawn(|| relay(&socket, &port.launcher, &wire));

            peer.send(b"from the wire").unwrap();
            let mut buf = [0; 64];
            let len = launcher.recv(&mut buf).unwrap();
            assert_eq!(&buf[..len], b"from the wire");

            launcher.send_to(b"from IOL", &port.listen).unwrap();
            let len = peer.recv(&mut buf).unwrap();
            assert_eq!(&buf[..len], b"from IOL");

            drop(unplug);
            let started = Instant::now();
            while !relay_thread.is_finished() {
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "relay kept running"
                );
                thread::sleep(Duration::from_millis(10));
            }
        });
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_send_on_a_vanished_interface_detaches_while_iol_keeps_sending() {
        let dir = test_dir();
        let port = Port::new(5, &dir);
        let launcher = UnixDatagram::bind(&port.launcher).unwrap();
        // The relay stops reading once it detaches, so the frames sent after that stay queued.
        // A blocking send would wait for room that never comes.
        launcher.set_nonblocking(true).unwrap();
        let socket = UnixDatagram::bind(&port.listen).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let (wire, peer) = pair_wire();
        // The send fails with ECONNREFUSED, which alone does not mean the interface is gone.
        drop(peer);
        wire.attached.store(false, Ordering::Relaxed);

        thread::scope(|scope| {
            let relay_thread = scope.spawn(|| relay(&socket, &port.launcher, &wire));
            // No pause between frames, so the relay never reads a timeout. The wire is detached
            // already, so if an assertion fails and the frames stop, the relay stops too.
            let started = Instant::now();
            while !relay_thread.is_finished() {
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "relay kept running"
                );
                let _ = launcher.send_to(b"from IOL", &port.listen);
            }
        });
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn frames_from_the_wire_before_the_launcher_listens_are_dropped() {
        let dir = test_dir();
        let port = Port::new(2, &dir);
        let socket = UnixDatagram::bind(&port.listen).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let (wire, peer) = pair_wire();

        thread::scope(|scope| {
            let _unplug = Unplug(&wire);
            scope.spawn(|| relay(&socket, &port.launcher, &wire));
            peer.send(b"lost").unwrap();
            thread::sleep(Duration::from_millis(100));

            let launcher = UnixDatagram::bind(&port.launcher).unwrap();
            launcher
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            peer.send(b"kept").unwrap();
            let mut buf = [0; 64];
            let len = launcher.recv(&mut buf).unwrap();
            assert_eq!(&buf[..len], b"kept");
        });
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_port_socket_is_writable_by_a_launcher_running_as_another_user() {
        let dir = test_dir();
        let mut port = Port::new(4, &dir);
        port.interface = "no-such-if4".to_owned();
        let listen = port.listen.clone();
        thread::spawn(move || run(&port));

        let started = Instant::now();
        while fs::metadata(&listen).is_err() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "socket not bound"
            );
            thread::sleep(Duration::from_millis(10));
        }
        // Bound, then opened up: wait for the mode, not just the file.
        let mut mode = 0;
        while started.elapsed() < Duration::from_secs(5) {
            mode = fs::metadata(&listen).unwrap().permissions().mode() & 0o777;
            if mode == 0o666 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(mode, 0o666);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn running_a_port_replaces_a_stale_socket_file() {
        let dir = test_dir();
        let port = Port::new(3, &dir);
        fs::write(&port.listen, b"stale").unwrap();
        let mut port_for_thread = port.clone();
        // An interface that never exists keeps the port waiting after it has bound.
        port_for_thread.interface = "no-such-if3".to_owned();
        thread::spawn(move || run(&port_for_thread));

        let started = Instant::now();
        let probe = UnixDatagram::unbound().unwrap();
        while probe.send_to(b"x", &port.listen).is_err() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "socket not bound"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_port_waiting_for_its_interface_drops_what_iol_sends() {
        let dir = test_dir();
        let mut port = Port::new(6, &dir);
        port.interface = "no-such-if6".to_owned();
        let listen = port.listen.clone();
        thread::spawn(move || run(&port));

        let launcher = UnixDatagram::unbound().unwrap();
        let started = Instant::now();
        while launcher.send_to(b"x", &listen).is_err() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "socket not bound"
            );
            thread::sleep(Duration::from_millis(10));
        }
        // Far more than the socket queues: a port that does not read blocks the sender.
        launcher
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        for frame in 0..5_000 {
            assert!(
                launcher.send_to(&[0; 1500], &listen).is_ok(),
                "frame {frame} not taken"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
