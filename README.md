# docker-cisco-iol

Builds the Cisco IOL container images for [Sherpa](https://github.com/bwks/sherpa)'s
`cisco_iol` and `cisco_ioll2` node models, on top of the Cisco Modeling Labs (CML) IOL images
you already have.

**No Cisco files are included in this repository**, and none must ever be added: no images,
binaries, licence files or anything taken from them. You bring your own CML IOL images.

## What you need

- Docker.
- Your own CML IOL image tarballs, from a CML release's reference platform files:
  - `iol-xe-<version>.tar.gz`, IOL XE (router), for `cisco_iol`;
  - `ioll2-xe-<version>.tar.gz`, IOL XE L2 (switch), for `cisco_ioll2`.

The images are licensed by Cisco for use within CML. Check that your licence allows using
them with Sherpa before you build.

## Build

Give the build script a CML tarball, or the tag of a CML image already loaded with
`docker load`:

    ./build-iol-image.sh ~/images/iol/iol-xe-17-18-02/iol-xe-17-18-02.tar.gz
    ./build-iol-image.sh ~/images/iol/ioll2-xe-17-18-02/ioll2-xe-17-18-02.tar.gz

    # or, already loaded
    ./build-iol-image.sh iol-xe/iol-xe:17-18-02

The variant and version come from the CML image's tag:

| CML image | Sherpa image |
|---|---|
| `iol-xe/iol-xe:17-18-02` | `localrepo/cisco_iol:17.18.02` |
| `iol-xe/ioll2-xe:17-18-02` | `localrepo/cisco_ioll2:17.18.02` |

The script prints the version of Cisco's IOL launcher in the CML image, and warns when it is
not the version this build was tested with (0.2.2).

The `iol-xe-serial-4eth` CML node definition uses the same `iol-xe` image; there is no
separate image to build.

## Import into Sherpa

Built on the Sherpa server, record the images with:

    sherpa server image scan

Built elsewhere, save the image and import it:

    docker save localrepo/cisco_iol:17.18.02 -o cisco_iol-17.18.02.tar
    sherpa server image import --src cisco_iol-17.18.02.tar --model cisco_iol --version 17.18.02

## How the image works

The image is the CML image with three additions. Nothing of Cisco's is changed or copied out.

- `iol-bridge`, a small static Rust program built in a builder stage (`bridge/`). Cisco's
  launcher, `/iol-runner`, exposes each IOL port as a pair of Unix datagram sockets carrying
  Ethernet frames: it binds `/tmp/sNN.sock` and sends to `/tmp/cNN.sock`, where `NN` is
  `slot * 4 + port`. The bridge binds `cNN.sock` and relays its frames to and from the
  container interface `ethN` through libpcap, linked statically, with the `pcap` crate; the
  bridge has no `unsafe` code of its own. libpcap puts back the 802.1Q tag that the kernel
  takes off a received frame, so IOL sees a trunk's frames tagged. The bridge reads only the
  frames arriving on `ethN`, so the frames IOL sends never come back to it. It attaches when `ethN` exists and is up, and re-attaches when it is
  made again, without restarting IOL, so interfaces can be added after IOL has started. While
  a port has no interface, the bridge drops what IOL sends on it, as an unplugged cable would.
  A second part of the bridge, the link monitor, makes IOL follow link state; see
  [Link state](#link-state).
- A static busybox, for the entrypoint's shell and `ip`.
- The entrypoint (`image/entrypoint.sh`). It writes the launcher's configuration from the
  environment, removes the addresses Docker gave `eth0` (IOL owns the management address on
  `Ethernet0/0`), starts the bridge and runs the launcher with the console on stdio and with
  `-keep`, when the launcher has it.

| Environment | Default | Meaning |
|---|---|---|
| `IOL_ID` | `1` | IOL application ID, 1 to 999. IOL derives its MAC addresses from it, so it must be unique among the IOL nodes on one network. |
| `IOL_MEMORY_MB` | `768` | IOL's memory (`-m`), the size of the whole IOL process. |
| `IOL_ETH_PORTS` | `16` | Ethernet ports, management included, in slots of 4. |

| Mount | Meaning |
|---|---|
| `/config` | `ios_config.txt`, the startup config, if there is one. Without one IOL boots to its factory state. |
| `/tmp/run` | IOL's NVRAM. Mounted from the host, a config saved with `write memory` survives a restart of the container and wins over `ios_config.txt`. IOL runs as the owner of `/tmp/run`, so what it writes there belongs to the host user who owns the folder. |

Interface `ethN` is IOL's `Ethernet(N / 4)/(N % 4)`: `eth0` is `Ethernet0/0`, `eth1` is
`Ethernet0/1`, `eth4` is `Ethernet1/0`.

The container does not need to be privileged: Docker's default capabilities plus `NET_ADMIN`
are enough. The console is on the container's stdio, so `docker attach` reaches it; it needs a
TTY (`docker run -it`).

### Link state

IOL learns of a link going down through layer 1 keepalives, not from the container interface.
Run with `-keep`, Cisco's launcher answers the keepalives IOL sends on each port once a second,
and listens on `/tmp/control.sock` for `up <N>` and `down <N>`, where `N` is the port and
`ethN` its interface. After `down N` the launcher stops answering port N; IOL declares the
interface down after 10 unanswered keepalives, plus IOS's 2 second carrier delay. After `up N`
the launcher wakes the port and IOL brings it back.

The bridge's link monitor reads the carrier of every `ethN` from
`/sys/class/net/ethN/carrier` every 500 ms, `eth0` included, and sends the launcher `down N` or
`up N` whenever a port's carrier differs from what the launcher last accepted. An interface
that is missing or admin-down counts as having no carrier. The launcher's ports start enabled,
so at start the monitor sends `down` only for the ports without carrier: ports with a link
never flap at boot. A command that fails, because the launcher is not listening yet or refuses
it, is sent again on the next poll. The monitor and the relays do not share state: a relay
keeps running through a carrier loss, and attaches again to an interface made again, as before.

The entrypoint looks for `-keep` in `/iol-runner -h`. A launcher without it runs as before,
with a warning on the console that link state is not followed; the link monitor then finds no
control socket, logs it once and stops. The bridge logs to `/tmp/iol-bridge.log` in the
container.

Sherpa sets all of this up itself. To run an image by hand:

    docker run -dit --cap-add NET_ADMIN \
        -e IOL_ID=1 -v "$PWD/config:/config:ro" -v "$PWD/run:/tmp/run" \
        localrepo/cisco_iol:17.18.02

## Known limits

- IOL's interface MTU is fixed at 1500. Linux peers on a link to IOL must use MTU 1500 on that
  interface, or their TCP stalls.
- A host transmitting to IOL through a veth with transmit checksum offload hands IOL TCP
  packets without a checksum. Sherpa turns the offload off on the host side of IOL's veths;
  by hand, `ethtool -K <host veth> tx off`.
- IOL follows link state through keepalives, not at once. Measured on the 17.18.02 images, an
  interface goes down about 12 seconds after its container interface loses carrier
  (`%LINK-3-UPDOWN`, then `%LINEPROTO-5-UPDOWN` a second later), and comes back about
  2 seconds after carrier returns. Use BFD where faster detection matters.
- Interfaces with no link show down, `notconnect` on the L2 image, rather than up.
- A carrier loss the monitor sees, however short, still takes the IOL interface down and up,
  about 11 to 14 seconds later: the launcher does not cancel a `down` with an `up`.

## The bridge

    cd bridge
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test
