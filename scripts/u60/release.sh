#!/bin/bash
# Official release: router software for online updates + the desktop installers.
#
#   BUILD=2 VERSION=0.2.0 NOTES=notes.json SIGNING_KEY=key.pem [RELEASE_RELAYS=host:port] \
#   [PUBLISH=1] scripts/u60/release.sh
#
# - Builds the agent twice: channel "public" (no default servers) and, with RELEASE_RELAYS,
#   channel "relay" (that relay as the default). Both carry OPENU60_BUILD=$BUILD, the number
#   online updates compare. The touchscreen program and device scripts are shared.
# - Writes dist-release/router/: the files, manifest.json and manifest.json.sig (Ed25519 over
#   the exact manifest bytes; the agent embeds the public key, see zte-agent/src/update.rs).
# - Builds the installers around the same agents in dist-release/installer/.
# - PUBLISH=1 uploads: router files to the fixed release "router-update" (the URL every agent
#   checks; old files there are replaced) and the installers to "installer-v$VERSION".
#   Neither is marked latest (the relay one-line installer uses releases/latest).
# NOTES: {"zh-Hans": "...", "en-US": "..."} shown in the app before updating.
set -eu
HERE="$(cd "$(dirname "$(realpath "$0")")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
: "${BUILD:?BUILD (a number larger than the last release)}" "${VERSION:?VERSION}" "${NOTES:?NOTES json}" "${SIGNING_KEY:?SIGNING_KEY (Ed25519 PEM)}"
GH_REPO="${GH_REPO:-Yakultt-t/openu60}"
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
unset OPENU60_DEFAULT_RELAYS OPENU60_PUSH_GATEWAY OPENU60_PAYLOAD
# OPENU60_UPDATE_URL and UPDATE_BASE are for testing against a local server only.
# Encoded form (0x1f-separated) because the checkout path may contain spaces.
unset RUSTFLAGS
export CARGO_ENCODED_RUSTFLAGS="--remap-path-prefix=$HOME=/build"$'\x1f'"--remap-path-prefix=$REPO=/src"
export OPENU60_BUILD="$BUILD" OPENU60_VERSION="$VERSION"
OUT="$REPO/dist-release"; rm -rf "$OUT"; mkdir -p "$OUT/router" "$OUT/installer"
cd "$REPO"

echo "== web panel (embedded in the agent)"
( cd web-admin && npx -y pnpm@10 install --frozen-lockfile >/dev/null && npx -y pnpm@10 build 2>&1 | grep -E "error|built in" )
[ "$(wc -c < zte-agent/web-dist/index.html)" -gt 100000 ] || { echo "web panel build failed"; exit 1; }

agent() { # channel target-dir [relays]
    echo "== agent ($1)"
    ( export OPENU60_CHANNEL="$1" CARGO_TARGET_DIR="$2"
      [ -n "${3:-}" ] && export OPENU60_DEFAULT_RELAYS="$3"
      cargo zigbuild --release --target aarch64-unknown-linux-musl -p zte-agent 2>&1 | grep -E "^error|Finished" )
    [ -x "$2/aarch64-unknown-linux-musl/release/zte-agent" ] || { echo "agent build failed"; exit 1; }
}
agent public "$REPO/target-release-public"
CHANNELS="public"
if [ -n "${RELEASE_RELAYS:-}" ]; then agent relay "$REPO/target-release-relay" "$RELEASE_RELAYS"; CHANNELS="public relay"; fi
echo "== touchscreen program"
CARGO_TARGET_DIR="$REPO/target-release-public" cargo zigbuild --release --target aarch64-unknown-linux-musl -p openu60-screen --bin openu60-screen 2>&1 | grep -E "^error|Finished"

R="$OUT/router"
cp "$REPO/target-release-public/aarch64-unknown-linux-musl/release/openu60-screen" "$R/openu60-screen-b$BUILD"
for f in zte-agent-run.sh wifi_uplink.sh wifi_uplink_dhcp.sh; do cp "$REPO/scripts/u60/device/$f" "$R/${f%.sh}-b$BUILD.sh"; done
for c in $CHANNELS; do cp "$REPO/target-release-$c/aarch64-unknown-linux-musl/release/zte-agent" "$R/zte-agent-$c-b$BUILD"; done
# Installs from the router itself (UFI-TOOLS plugin, ssh/adb); fixed name so its URL stays put.
cp "$REPO/scripts/u60/device/openu60-netinstall.sh" "$R/openu60-netinstall.sh"

echo "== checking for build-machine paths"
for f in "$R"/*; do
    if strings -n 6 "$f" | grep -q -E "/Users/|/home/[a-z]"; then echo "build path left in $f"; exit 1; fi
done

echo "== manifest"
python3 - "$R" "$BUILD" "$VERSION" "$NOTES" "$GH_REPO" "$CHANNELS" <<'EOF'
import hashlib, json, os, sys, datetime
r, build, version, notes, repo, channels = sys.argv[1:7]
base = os.environ.get("UPDATE_BASE") or f"https://github.com/{repo}/releases/download/router-update"
def entry(name, file):
    p = os.path.join(r, file)
    return {"name": name, "url": f"{base}/{file}", "sha256": hashlib.sha256(open(p, "rb").read()).hexdigest(), "size": os.path.getsize(p)}
shared = [entry("openu60-screen", f"openu60-screen-b{build}")] + [
    entry(f, f"{f[:-3]}-b{build}.sh") for f in ("zte-agent-run.sh", "wifi_uplink.sh", "wifi_uplink_dhcp.sh")]
m = {"build": int(build), "version": version, "date": datetime.date.today().isoformat(),
     "notes": json.load(open(notes)),
     "channels": {c: [entry("zte-agent", f"zte-agent-{c}-b{build}")] + shared for c in channels.split()}}
open(os.path.join(r, "manifest.json"), "w").write(json.dumps(m, ensure_ascii=False, indent=1))
EOF
openssl pkeyutl -sign -inkey "$SIGNING_KEY" -rawin -in "$R/manifest.json" -out "$R/manifest.sig.bin"
base64 < "$R/manifest.sig.bin" | tr -d '\n' > "$R/manifest.json.sig"; rm "$R/manifest.sig.bin"
openssl pkey -in "$SIGNING_KEY" -pubout -out "$OUT/pub.pem"
openssl pkeyutl -verify -pubin -inkey "$OUT/pub.pem" -rawin -in "$R/manifest.json" -sigfile <(base64 -d < "$R/manifest.json.sig") >/dev/null || { echo "signature check failed"; exit 1; }
rm "$OUT/pub.pem"

[ "${SKIP_INSTALLERS:-0}" = 1 ] && { echo "OK (router files only)"; exit 0; }
echo "== installers"
INST="$REPO/desktop/openu60-installer"
payload() { # channel dir
    mkdir -p "$2"
    cp "$R/zte-agent-$1-b$BUILD" "$2/zte-agent"; cp "$R/openu60-screen-b$BUILD" "$2/openu60-screen"
    for f in zte-agent-run.sh wifi_uplink.sh wifi_uplink_dhcp.sh; do cp "$R/${f%.sh}-b$BUILD.sh" "$2/$f"; done
}
installer() { # payload-dir target name
    ( cd "$INST" && export OPENU60_PAYLOAD="$1" CARGO_TARGET_DIR="$INST/target-$(basename "$1")"
      if [ "$2" = aarch64-apple-darwin ]; then cargo build --release --target "$2" 2>&1 | grep -E "^error|Finished"
      else rustup target add "$2" >/dev/null 2>&1 || true; cargo zigbuild --release --target "$2" 2>&1 | grep -E "^error|Finished"; fi )
    local ext=""; case "$2" in *windows*) ext=".exe";; esac
    cp "$INST/target-$(basename "$1")/$2/release/openu60-installer$ext" "$OUT/installer/$3$ext"
}
payload public "$OUT/payload-public"
installer "$OUT/payload-public" x86_64-pc-windows-gnu OpenU60-Installer-Windows-x64
installer "$OUT/payload-public" aarch64-apple-darwin OpenU60-Installer-macOS-arm64
installer "$OUT/payload-public" x86_64-unknown-linux-musl OpenU60-Installer-Linux-x64
if [[ " $CHANNELS " == *" relay "* ]]; then
    payload relay "$OUT/payload-relay"
    installer "$OUT/payload-relay" x86_64-pc-windows-gnu OpenU60-Installer-Windows-x64-relay
fi
for f in "$OUT/installer"/*; do
    if strings -n 6 "$f" | grep -q -E "/Users/|/home/[a-z]"; then echo "build path left in $f"; exit 1; fi
done
(cd "$OUT/installer" && shasum -a 256 * > SHA256SUMS)
ls -l "$R" "$OUT/installer"

[ "${PUBLISH:-0}" = 1 ] || { echo "OK (not published; PUBLISH=1 uploads)"; exit 0; }
echo "== publish router-update"
gh release view router-update -R "$GH_REPO" >/dev/null 2>&1 || \
    gh release create router-update -R "$GH_REPO" --latest=false --title "路由器在线更新 / Router updates" \
      --notes "SIM 随行 App 在线更新路由器时读取这里的文件，请不要手动下载。Files for online updates from the app; no need to download them." >/dev/null
gh release upload router-update -R "$GH_REPO" --clobber "$R"/*
for a in $(gh release view router-update -R "$GH_REPO" --json assets -q '.assets[].name'); do
    [ -f "$R/$a" ] || gh release delete-asset router-update "$a" -R "$GH_REPO" -y >/dev/null
done
echo "== publish installer-v$VERSION"
gh release create "installer-v$VERSION" -R "$GH_REPO" --latest=false --target main \
    --title "安装程序 v$VERSION / Installer v$VERSION" --notes-file "${INSTALLER_NOTES:-/dev/null}" "$OUT/installer"/*
echo "OK. Published build $BUILD."
