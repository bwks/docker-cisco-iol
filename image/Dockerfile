# check=skip=InvalidDefaultArgInFrom
# Sherpa's Cisco IOL image, built on top of the user's own CML IOL image.
#
# CML_IMAGE is the CML image as loaded with `docker load`, for example
# iol-xe/iol-xe:17-18-02. Nothing in it is changed or copied out: this adds the bridge, a
# static busybox for the entrypoint, and the entrypoint. Build it with build-iol-image.sh.
ARG CML_IMAGE

FROM rust:1-alpine AS bridge
# libpcap-dev has the static libpcap.a the bridge links.
RUN apk add --no-cache libpcap-dev
WORKDIR /src
COPY bridge/Cargo.toml bridge/Cargo.lock ./
COPY bridge/src ./src
# Rust links musl and libpcap statically on Alpine: the binary needs nothing from the CML image.
RUN cargo build --release --locked

FROM busybox:1.37-musl AS busybox

FROM ${CML_IMAGE}
COPY --from=busybox /bin/busybox /bin/busybox
RUN ["/bin/busybox", "--install", "-s", "/bin"]
# Docker exec as user root, as Sherpa runs it, needs root in /etc/passwd; the CML image has none.
RUN [ -e /etc/passwd ] || { echo 'root:x:0:0:root:/:/bin/sh' >/etc/passwd; echo 'root:x:0:' >/etc/group; }
COPY --from=bridge /src/target/release/iol-bridge /usr/local/bin/iol-bridge
COPY image/entrypoint.sh /usr/local/bin/sherpa-iol-entrypoint
ENTRYPOINT ["/bin/sh", "/usr/local/bin/sherpa-iol-entrypoint"]
