FROM debian:bookworm-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    build-essential \
    bison \
    flex \
    libpcap-dev \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN git clone https://github.com/GNS3/iouyap.git /tmp/iouyap \
    && cd /tmp/iouyap \
    && CFLAGS="-fcommon" make \
    && cp iouyap /usr/local/bin/iouyap

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    tini \
    iproute2 \
    iputils-ping \
    net-tools \
    libpcap0.8 \
    python3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/iouyap /usr/local/bin/iouyap

WORKDIR /iol

COPY iol.bin /iol/iol.bin
RUN chmod +x /iol/iol.bin

COPY config.txt /iol/config.txt
COPY keygen.py /iol/keygen.py
COPY entrypoint.sh /iol/entrypoint.sh
COPY start-iol.sh /iol/start-iol.sh
RUN chmod +x /iol/entrypoint.sh /iol/start-iol.sh

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["/iol/entrypoint.sh"]
