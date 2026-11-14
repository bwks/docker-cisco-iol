//! `iol-bridge`: relays Ethernet frames between a container's `eth<N>` interfaces and the
//! per-port sockets of Cisco's IOL launcher, one port per interface.
//!
//! Usage: `iol-bridge <ports>`. Port 0 is `eth0`, IOL's `Ethernet0/0`; port `N` is `eth<N>`,
//! `Ethernet<N / 4>/<N % 4>`. Interfaces can be added after IOL starts and made again while it
//! runs. A link monitor makes IOL follow each interface's carrier through the launcher's
//! control socket.

#![forbid(unsafe_code)]

mod link;
mod packet;
mod port;

use std::env;
use std::path::Path;
use std::process::ExitCode;
use std::thread;

use crate::link::{CONTROL_SOCKET, Monitor, SYSFS_NET};
use crate::port::{Port, SOCKET_DIR, parse_port_count, run};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let ports = match parse_port_count(&args) {
        Ok(ports) => ports,
        Err(message) => {
            eprintln!("iol-bridge: {message}");
            return ExitCode::FAILURE;
        }
    };

    let ports: Vec<Port> = (0..ports)
        .map(|index| Port::new(index, Path::new(SOCKET_DIR)))
        .collect();

    // The monitor ends on its own when the launcher has no control socket; the relays do not
    // depend on it.
    let mut monitor = Monitor::new(&ports, Path::new(SYSFS_NET), Path::new(CONTROL_SOCKET));
    if let Err(error) = thread::Builder::new()
        .name("link".to_owned())
        .spawn(move || monitor.run())
    {
        eprintln!("iol-bridge: cannot start the link monitor: {error}");
    }

    let mut workers = Vec::new();
    for port in ports {
        let index = port.index;
        let spawned = thread::Builder::new()
            .name(port.interface.clone())
            .spawn(move || run(&port).map_err(|error| (port.interface, error)));
        match spawned {
            Ok(worker) => workers.push(worker),
            Err(error) => {
                eprintln!("iol-bridge: cannot start port {index}: {error}");
                return ExitCode::FAILURE;
            }
        }
    }

    // A port returns only when it cannot bind its socket; the others keep running.
    let mut failed = false;
    for worker in workers {
        match worker.join() {
            Ok(Ok(())) => {}
            Ok(Err((interface, error))) => {
                eprintln!("iol-bridge: {interface} stopped: {error}");
                failed = true;
            }
            Err(_) => {
                eprintln!("iol-bridge: a port thread panicked");
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
