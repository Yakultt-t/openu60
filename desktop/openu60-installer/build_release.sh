#!/bin/bash
# Builds the downloadable installer: the router agent from this checkout (with NO private
# relay / push defaults), embedded into one installer file per desktop platform.
# Output: desktop/openu60-installer/dist/ (+ SHA256SUMS). Needs rustup, zig and cargo-zigbuild.
#   desktop/openu60-installer/build_release.sh
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"

# A public download must not carry anyone's own server or build paths.
unset OPENU60_DEFAULT_RELAYS OPENU60_PUSH_GATEWAY OPENU60_PAYLOAD
EMPTY_HOME="$(mktemp -d)"
export RUSTFLAGS="--remap-path-prefix=$HOME=/build --remap-path-prefix=$REPO=/src"

echo "== router agent and touchscreen program (aarch64 musl)"
U60_HOME="$EMPTY_HOME" "$REPO/scripts/u60/06_build_agent.sh"
rm -rf "$EMPTY_HOME"

cd "$HERE"
DIST="$HERE/dist"
rm -rf "$DIST"; mkdir -p "$DIST"
build() { # target, output name
    rustup target add "$1" >/dev/null 2>&1 || true
    if [ "$1" = aarch64-apple-darwin ]; then cargo build --release --target "$1"; else cargo zigbuild --release --target "$1"; fi
    local ext=""; case "$1" in *windows*) ext=".exe";; esac
    cp "target/$1/release/openu60-installer$ext" "$DIST/$2$ext"
}
echo "== installers"
build x86_64-pc-windows-gnu OpenU60-Installer-Windows-x64
build aarch64-apple-darwin OpenU60-Installer-macOS-arm64
build x86_64-unknown-linux-musl OpenU60-Installer-Linux-x64

echo "== checking for build-machine paths"
bad=0
for f in "$DIST"/* "$REPO/target/aarch64-unknown-linux-musl/release/zte-agent"; do
    if strings -n 6 "$f" | grep -q -E "/Users/|/home/[a-z]"; then echo "  build path left in $f"; bad=1; fi
done
[ "$bad" = 0 ] || { echo "FAILED: remove the paths above before publishing"; exit 1; }
(cd "$DIST" && shasum -a 256 * > SHA256SUMS)
ls -l "$DIST"
echo "OK. Also scan dist/ for your own server names before uploading."
