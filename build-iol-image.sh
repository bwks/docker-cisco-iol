#!/usr/bin/env bash
# Build Sherpa's Cisco IOL image from a CML IOL image.
set -euo pipefail

# The launcher version this build was tested with.
TESTED_LAUNCHER_VERSION="0.2.2"

usage() {
    cat <<EOF
Usage: $(basename "$0") <cml-image>

Build a Sherpa Cisco IOL image on top of a Cisco CML IOL image.

Arguments:
    cml-image    The CML image tarball (for example iol-xe-17-18-02.tar.gz), or the tag of a
                 CML image already loaded into Docker (for example iol-xe/iol-xe:17-18-02).

The variant and version come from the CML image's tag:
    iol-xe/iol-xe:17-18-02     ->  localrepo/cisco_iol:17.18.02
    iol-xe/ioll2-xe:17-18-02   ->  localrepo/cisco_ioll2:17.18.02

No Cisco files are included in this repository; bring your own CML images.
EOF
    exit 1
}

fail() {
    echo "Error: $*" >&2
    exit 1
}

if [[ $# -ne 1 ]] || [[ "$1" == "-h" ]] || [[ "$1" == "--help" ]]; then
    usage
fi

SOURCE="$1"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

if [[ -f "$SOURCE" ]]; then
    echo "Loading ${SOURCE}"
    LOADED="$(docker load --input "$SOURCE")"
    echo "$LOADED"
    CML_IMAGE="$(sed -n 's/^Loaded image: //p' <<<"$LOADED" | head -n 1)"
    [[ -n "$CML_IMAGE" ]] || fail "docker load did not report a tagged image"
else
    CML_IMAGE="$SOURCE"
fi

docker image inspect "$CML_IMAGE" >/dev/null 2>&1 || fail "no such image: ${CML_IMAGE}"

REPOSITORY="${CML_IMAGE%:*}"
TAG="${CML_IMAGE##*:}"
[[ "$REPOSITORY" != "$CML_IMAGE" ]] || fail "the image has no tag: ${CML_IMAGE}"

case "${REPOSITORY##*/}" in
    iol-xe) MODEL="cisco_iol" ;;
    ioll2-xe) MODEL="cisco_ioll2" ;;
    *) fail "not a CML IOL image: ${REPOSITORY} (expected .../iol-xe or .../ioll2-xe)" ;;
esac

[[ "$TAG" =~ ^[0-9]+(-[0-9]+)+$ ]] || fail "unexpected CML tag '${TAG}', expected one like 17-18-02"
VERSION="${TAG//-/.}"

LAUNCHER_VERSION="$(docker run --rm --entrypoint /iol-runner "$CML_IMAGE" -h 2>&1 |
    sed -n 's/^[[:space:]]*Version:[[:space:]]*//p' | head -n 1)"
[[ -n "$LAUNCHER_VERSION" ]] || fail "${CML_IMAGE} has no Cisco IOL launcher at /iol-runner"
echo "Cisco IOL launcher: ${LAUNCHER_VERSION}"
if [[ "$LAUNCHER_VERSION" != "$TESTED_LAUNCHER_VERSION" ]]; then
    echo "Warning: this build was tested with launcher ${TESTED_LAUNCHER_VERSION}; the launcher's" \
        "configuration or sockets may have changed." >&2
fi

IMAGE_TAG="localrepo/${MODEL}:${VERSION}"
echo "Building ${IMAGE_TAG} from ${CML_IMAGE}"
docker build \
    --build-arg "CML_IMAGE=${CML_IMAGE}" \
    --file "${SCRIPT_DIR}/image/Dockerfile" \
    --tag "$IMAGE_TAG" \
    "$SCRIPT_DIR"

echo ""
echo "Built ${IMAGE_TAG}"
echo "On the Sherpa server, record it with: sherpa server image scan"
