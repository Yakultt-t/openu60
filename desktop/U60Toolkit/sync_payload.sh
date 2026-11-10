#!/bin/bash
# Copies the current agent build and device scripts into the app's payload folder.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$HERE/../.."
P="$HERE/Resources/payload"
cp "$ROOT/target/aarch64-unknown-linux-musl/release/zte-agent" "$P/zte-agent"
cp "$ROOT/target/aarch64-unknown-linux-musl/release/openu60-screen" "$P/openu60-screen"
for f in zte-agent-run.sh wifi_uplink.sh wifi_uplink_dhcp.sh; do cp "$ROOT/scripts/u60/device/$f" "$P/$f"; done
chmod 755 "$P"/*
shasum -a 256 "$P/zte-agent" | cut -c1-16
