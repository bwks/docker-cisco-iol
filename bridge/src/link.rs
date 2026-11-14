//! The link monitor: makes IOL follow the carrier of its container interfaces.
//!
//! Run with `-keep`, Cisco's launcher answers IOL's layer 1 keepalives and listens on a control
//! socket for `up <port>` and `down <port>`. After `down N` it stops answering port N's
//! keepalives, and IOL takes the interface down about 12 seconds later; after `up N` IOL brings
//! it back in about 2 seconds. The launcher's ports start enabled.
//!
//! The monitor reads the carrier of every port's interface from sysfs each tick and sends the
//! launcher the state of each port whose carrier differs from what the launcher last
//! acknowledged. Ports with carrier at start are never touched, so they do not flap at boot.

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::port::Port;

/// The launcher's control socket.
pub const CONTROL_SOCKET: &str = "/tmp/control.sock";
/// Where the kernel shows the interfaces.
pub const SYSFS_NET: &str = "/sys/class/net";
/// How often the monitor reads the carriers.
const TICK: Duration = Duration::from_millis(500);
/// How long the monitor waits for the control socket to appear before it decides the launcher
/// runs without `-keep`. The launcher binds it at start, within a second of the bridge.
const SOCKET_WAIT: Duration = Duration::from_secs(30);
/// How long a command waits for the launcher's reply.
const REPLY_TIMEOUT: Duration = Duration::from_secs(2);

/// The state of a link, as seen on the interface or as set in the launcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    Up,
    Down,
}

impl Link {
    /// The launcher's word for this state.
    fn command(self) -> &'static str {
        match self {
            Link::Up => "up",
            Link::Down => "down",
        }
    }
}

/// One change the launcher is to make: set port `index` to `link`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    pub index: u16,
    pub link: Link,
}

impl Change {
    /// The control socket line for this change.
    pub fn line(self) -> String {
        format!("{} {}\n", self.link.command(), self.index)
    }
}

/// The link state of an interface from what reading its `carrier` file gave: up only when the
/// read worked and says `1`. The read fails when the interface is missing or admin-down.
pub fn link_from_carrier(carrier: Option<&str>) -> Link {
    match carrier.map(str::trim) {
        Some("1") => Link::Up,
        _ => Link::Down,
    }
}

/// The changes to send: one for each port whose carrier differs from the launcher's state.
/// `launcher` and `carrier` hold one entry per port, in port order, for ports `0..`.
pub fn changes(launcher: &[Link], carrier: &[Link]) -> Vec<Change> {
    launcher
        .iter()
        .zip(carrier)
        .zip(0_u16..)
        .filter(|((set, seen), _)| set != seen)
        .map(|((_, seen), index)| Change { index, link: *seen })
        .collect()
}

/// Whether the launcher accepted a command: its reply starts with `OK:`.
pub fn accepted(reply: &str) -> bool {
    reply.starts_with("OK:")
}

/// What the monitor does after a tick in which the control socket could not be reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Absent {
    /// Try again on the next tick.
    Retry,
    /// The socket has not appeared in time: the launcher runs without `-keep`.
    GiveUp,
}

/// Whether to keep waiting for a control socket that is not there. Once it has been seen the
/// monitor never gives up: the launcher may be busy, and is the container's main process.
pub fn on_absent(seen: bool, waited: Duration) -> Absent {
    if seen || waited < SOCKET_WAIT {
        Absent::Retry
    } else {
        Absent::GiveUp
    }
}

/// The link monitor for `ports`.
pub struct Monitor {
    /// Each port's `carrier` file, in port order.
    carriers: Vec<PathBuf>,
    control: PathBuf,
    /// The state of each port as the launcher last acknowledged it.
    launcher: Vec<Link>,
}

impl Monitor {
    pub fn new(ports: &[Port], sysfs_net: &Path, control: &Path) -> Self {
        Self {
            carriers: ports
                .iter()
                .map(|port| sysfs_net.join(&port.interface).join("carrier"))
                .collect(),
            control: control.to_path_buf(),
            // The launcher starts with every port enabled.
            launcher: vec![Link::Up; ports.len()],
        }
    }

    /// Run until the launcher turns out to have no control socket.
    pub fn run(&mut self) {
        let started = Instant::now();
        let mut seen = false;
        loop {
            if self.control.exists() {
                seen = true;
            }
            if seen {
                if let Err(error) = self.tick() {
                    // Not listening yet, or busy: the changes are sent again next tick.
                    eprintln!("iol-bridge: link state not sent: {error}");
                }
            } else if on_absent(seen, started.elapsed()) == Absent::GiveUp {
                eprintln!(
                    "iol-bridge: no control socket at {} after {}s: the launcher runs without \
                     -keep, so IOL does not follow link state",
                    self.control.display(),
                    SOCKET_WAIT.as_secs()
                );
                return;
            }
            thread::sleep(TICK);
        }
    }

    /// Read the carriers and send the launcher what changed. A change is recorded only once the
    /// launcher has accepted it; anything else is sent again on the next tick.
    fn tick(&mut self) -> io::Result<()> {
        let carrier: Vec<Link> = self
            .carriers
            .iter()
            .map(|path| link_from_carrier(fs::read_to_string(path).ok().as_deref()))
            .collect();
        let pending = changes(&self.launcher, &carrier);
        if pending.is_empty() {
            return Ok(());
        }
        let stream = UnixStream::connect(&self.control)?;
        stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
        stream.set_write_timeout(Some(REPLY_TIMEOUT))?;
        let mut writer = &stream;
        let mut reader = BufReader::new(&stream);
        for change in pending {
            writer.write_all(change.line().as_bytes())?;
            let mut reply = String::new();
            reader.read_line(&mut reply)?;
            let reply = reply.trim_end();
            if accepted(reply) {
                eprintln!(
                    "iol-bridge: eth{} link {}",
                    change.index,
                    change.link.command()
                );
                self.launcher[usize::from(change.index)] = change.link;
            } else {
                eprintln!(
                    "iol-bridge: launcher refused '{}': {reply}",
                    change.line().trim_end()
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU32, Ordering};

    use Link::{Down, Up};

    static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

    /// A fresh directory for one test, with a fake sysfs holding `eth0` to `eth<ports - 1>`.
    fn test_dir(carriers: &[&str]) -> (PathBuf, Vec<Port>) {
        let dir = std::env::temp_dir().join(format!(
            "iol-bridge-link-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        let ports: Vec<Port> = (0_u16..)
            .zip(carriers)
            .map(|(index, carrier)| {
                let port = Port::new(index, &dir);
                let iface = dir.join("net").join(&port.interface);
                fs::create_dir_all(&iface).unwrap();
                fs::write(iface.join("carrier"), carrier).unwrap();
                port
            })
            .collect();
        (dir, ports)
    }

    /// A fake launcher: answers each line with the next of `replies` and returns what it got.
    fn launcher(path: &Path, replies: &'static [&'static str]) -> thread::JoinHandle<Vec<String>> {
        let listener = UnixListener::bind(path).unwrap();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut writer = &stream;
            let mut got = Vec::new();
            for reply in replies {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                got.push(line);
                writer.write_all(reply.as_bytes()).unwrap();
            }
            got
        })
    }

    #[test]
    fn the_monitor_sends_changes_and_resends_what_the_launcher_refused() {
        let (dir, ports) = test_dir(&["1\n", "0\n", "1\n", "0\n"]);
        let control = dir.join("control.sock");
        let mut monitor = Monitor::new(&ports, &dir.join("net"), &control);

        // No launcher yet: nothing is recorded.
        assert!(monitor.tick().is_err());
        assert_eq!(monitor.launcher, vec![Up; 4]);

        let first = launcher(
            &control,
            &["OK: disabling port 1\n", "ERROR: invalid index 3, max 2\n"],
        );
        monitor.tick().unwrap();
        assert_eq!(first.join().unwrap(), vec!["down 1\n", "down 3\n"]);
        assert_eq!(monitor.launcher, vec![Up, Down, Up, Up]);

        fs::remove_file(&control).unwrap();
        fs::write(dir.join("net/eth1/carrier"), "1\n").unwrap();
        let second = launcher(
            &control,
            &["OK: enabling port 1\n", "OK: disabling port 3\n"],
        );
        monitor.tick().unwrap();
        assert_eq!(second.join().unwrap(), vec!["up 1\n", "down 3\n"]);
        assert_eq!(monitor.launcher, vec![Up, Up, Up, Down]);

        // Nothing changed: the launcher is not contacted.
        fs::remove_file(&control).unwrap();
        monitor.tick().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_interface_counts_as_down() {
        let (dir, ports) = test_dir(&["1\n", "1\n"]);
        fs::remove_dir_all(dir.join("net/eth1")).unwrap();
        let control = dir.join("control.sock");
        let mut monitor = Monitor::new(&ports, &dir.join("net"), &control);
        let got = launcher(&control, &["OK: disabling port 1\n"]);
        monitor.tick().unwrap();
        assert_eq!(got.join().unwrap(), vec!["down 1\n"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_carrier_of_one_is_up() {
        assert_eq!(link_from_carrier(Some("1\n")), Up);
    }

    #[test]
    fn no_carrier_a_failed_read_or_garbage_is_down() {
        assert_eq!(link_from_carrier(Some("0\n")), Down);
        assert_eq!(link_from_carrier(None), Down);
        assert_eq!(link_from_carrier(Some("")), Down);
        assert_eq!(link_from_carrier(Some("11\n")), Down);
    }

    #[test]
    fn ports_with_carrier_at_start_are_left_alone() {
        assert_eq!(changes(&[Up, Up, Up], &[Up, Up, Up]), vec![]);
    }

    #[test]
    fn ports_without_carrier_at_start_are_taken_down() {
        assert_eq!(
            changes(&[Up, Up, Up, Up], &[Up, Down, Up, Down]),
            vec![
                Change {
                    index: 1,
                    link: Down
                },
                Change {
                    index: 3,
                    link: Down
                },
            ]
        );
    }

    #[test]
    fn a_carrier_coming_back_brings_the_port_up() {
        assert_eq!(
            changes(&[Down, Down], &[Down, Up]),
            vec![Change { index: 1, link: Up }]
        );
    }

    #[test]
    fn the_management_port_is_followed_like_the_others() {
        assert_eq!(
            changes(&[Up, Up], &[Down, Up]),
            vec![Change {
                index: 0,
                link: Down
            }]
        );
    }

    #[test]
    fn an_unchanged_down_port_is_not_sent_again() {
        assert_eq!(changes(&[Down, Up], &[Down, Up]), vec![]);
    }

    #[test]
    fn changes_are_launcher_lines() {
        assert_eq!(
            Change {
                index: 12,
                link: Down
            }
            .line(),
            "down 12\n"
        );
        assert_eq!(Change { index: 0, link: Up }.line(), "up 0\n");
    }

    #[test]
    fn only_an_ok_reply_is_accepted() {
        assert!(accepted("OK: disabling port 1"));
        assert!(accepted("OK: enabling port 1"));
        assert!(!accepted("ERROR: invalid index 99, max 15"));
        assert!(!accepted("ERROR: unknown command DOWN"));
        assert!(!accepted(""));
    }

    #[test]
    fn a_missing_socket_is_waited_for_then_given_up() {
        assert_eq!(on_absent(false, Duration::ZERO), Absent::Retry);
        assert_eq!(on_absent(false, Duration::from_secs(29)), Absent::Retry);
        assert_eq!(on_absent(false, Duration::from_secs(30)), Absent::GiveUp);
    }

    #[test]
    fn a_socket_once_seen_is_never_given_up() {
        assert_eq!(on_absent(true, Duration::from_secs(3600)), Absent::Retry);
    }
}
