#!/usr/bin/env bash
set -euo pipefail

IOL_ID=1
IOUYAP_ID=$((IOL_ID + 512))
WORK_DIR="/iol"

cd "$WORK_DIR"

# Discover container eth interfaces and find highest index
max_eth=0
eth_interfaces=()
for iface in /sys/class/net/eth*; do
    name=$(basename "$iface")
    idx=${name#eth}
    eth_interfaces+=("$idx")
    if [[ $idx -gt $max_eth ]]; then
        max_eth=$idx
    fi
done

# Calculate number of interface groups (4 interfaces per group)
# Group 0: e0/0-e0/3, Group 1: e1/0-e1/3, etc.
# We need enough groups to cover all eth interfaces
groups=$(( (max_eth / 4) + 1 ))

# Generate MAC addresses for each interface
generate_mac() {
    local group=$1
    local port=$2
    printf "aabb.cc00.%02x%02x" "$group" "$port"
}

# Generate NETMAP file
# Maps IOL ports to iouyap virtual node ports
: > NETMAP
for idx in "${eth_interfaces[@]}"; do
    group=$((idx / 4))
    port=$((idx % 4))
    echo "${IOL_ID}:${group}/${port} ${IOUYAP_ID}:${group}/${port}" >> NETMAP
done

# Generate iouyap.ini
# Maps iouyap bay:unit ports to container eth interfaces
cat > iouyap.ini <<EOF
[default]
base_port = 49000
netmap = NETMAP
EOF

for idx in "${eth_interfaces[@]}"; do
    group=$((idx / 4))
    port=$((idx % 4))
    bay_unit="${group}:${port}"

    cat >> iouyap.ini <<EOF

[${bay_unit}]
eth_dev = eth${idx}
EOF
done

# Template the default config — skip if user provided their own
if [[ "${TEMPLATE_CONFIG:-true}" == "true" ]]; then
    HOSTNAME=$(hostname)
    sed -i "s/<hostname>/${HOSTNAME}/g" config.txt

    eth0_mac=$(generate_mac 0 0)
    sed -i "s/<eth0_mac>/${eth0_mac}/g" config.txt

    # Generate data interface configurations
    interface_config=""
    for idx in "${eth_interfaces[@]}"; do
        # Skip eth0 — management interface handled separately in config template
        if [[ $idx -eq 0 ]]; then
            continue
        fi
        group=$((idx / 4))
        port=$((idx % 4))
        mac=$(generate_mac "$group" "$port")

        interface_config+="!
interface Ethernet${group}/${port}
 mac-address ${mac}
 no ip address
 no shutdown
"
    done

    if [[ -n "$interface_config" ]]; then
        escaped=$(printf '%s' "$interface_config" | sed ':a;N;$!ba;s/\n/\\n/g')
        sed -i "s|<interfaces>|${escaped}|g" config.txt
    else
        sed -i "s|<interfaces>||g" config.txt
    fi
fi

# Start iouyap in background (runs as virtual node IOUYAP_ID)
iouyap -q "$IOUYAP_ID" &

# Small delay for iouyap to initialize
sleep 1

# Launch IOL binary — exec so it becomes direct child of tini
exec ./iol.bin "$IOL_ID" -e "$groups" -s 0 -d 0 -c config.txt -- -n 1024 -q -m 1024
