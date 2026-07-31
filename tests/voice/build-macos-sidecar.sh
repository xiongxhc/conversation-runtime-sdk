#!/bin/sh
set -eu

swift build -c release --package-path platform/macos/voice-sidecar
SIDE_CAR_BIN="$(
    swift build -c release \
        --package-path platform/macos/voice-sidecar \
        --show-bin-path
)/conversation-voice-sidecar"
test -x "$SIDE_CAR_BIN"
printf '%s\n' "$SIDE_CAR_BIN"
