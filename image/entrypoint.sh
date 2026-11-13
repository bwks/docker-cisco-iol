#!/bin/sh
# Start IOL through Cisco's launcher, with the bridge relaying the container's interfaces.
#
# Environment, set by Sherpa:
#   IOL_ID         IOL application ID, 1 to 999, unique among the IOL nodes on one network.
#                  IOL derives its MAC addresses from it. 1000 is the launcher's own ID.
#   IOL_MEMORY_MB  IOL's memory in MiB (its -m): the whole IOL process, not IOS's free memory.
#   IOL_ETH_PORTS  Ethernet ports, management included, 1 to 64. IOL has slots of 4 ports.
#
# Mounts: /config holds ios_config.txt, the startup config, if there is one. /tmp/run holds
# IOL's NVRAM; mount it to keep a saved config across restarts of the container.
set -eu

IOL_ID="${IOL_ID:-1}"
IOL_MEMORY_MB="${IOL_MEMORY_MB:-768}"
IOL_ETH_PORTS="${IOL_ETH_PORTS:-16}"
LAUNCHER_CONFIG=/tmp/iol-config.json
REMOTE_APP_ID=1000

fail() {
    echo "sherpa-iol-entrypoint: $*" >&2
    exit 1
}

# Whether $1 is a whole number from $2 to $3.
in_range() {
    case "$1" in
        '' | *[!0-9]*) return 1 ;;
    esac
    [ "$1" -ge "$2" ] && [ "$1" -le "$3" ]
}

in_range "$IOL_ID" 1 999 || fail "IOL_ID must be 1 to 999, not '$IOL_ID'"
in_range "$IOL_MEMORY_MB" 256 65535 || fail "IOL_MEMORY_MB must be 256 to 65535, not '$IOL_MEMORY_MB'"
in_range "$IOL_ETH_PORTS" 1 64 || fail "IOL_ETH_PORTS must be 1 to 64, not '$IOL_ETH_PORTS'"

# The launcher exits without /tmp/run. Sockets left by an earlier run of this container
# would stop the launcher binding its own.
mkdir -p /tmp/run
rm -rf /tmp/s[0-9][0-9].sock /tmp/c[0-9][0-9].sock /tmp/console.sock /tmp/control.sock \
    /tmp/netio[0-9]*

printf '{"binary":"/binary.iol","memory":%s,"num-eth":%s,"num-serial":0,"local-app":%s,"remote-app":%s,"user-id":0,"group-id":0}\n' \
    "$IOL_MEMORY_MB" "$IOL_ETH_PORTS" "$IOL_ID" "$REMOTE_APP_ID" >"$LAUNCHER_CONFIG"

# IOL owns the management address. Left on eth0, the kernel would answer for it too.
if [ -e /sys/class/net/eth0 ]; then
    ip addr flush dev eth0
fi

/usr/local/bin/iol-bridge "$IOL_ETH_PORTS" 2>>/tmp/iol-bridge.log &

exec /iol-runner -config "$LAUNCHER_CONFIG" -stdio
