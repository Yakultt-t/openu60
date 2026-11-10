#!/bin/bash
# Cross-compile open-u60-pro's zte-agent for the U60 Pro (aarch64 musl) with cargo-zigbuild.
# Output: target/aarch64-unknown-linux-musl/release/zte-agent and openu60-screen
# Then deploy with:  scripts/u60/04_deploy_agent.sh target/aarch64-unknown-linux-musl/release/zte-agent
set -u
HERE="$(cd "$(dirname "$(realpath "$0")")" && pwd)"   # scripts/u60 in the repo
REPO="$(cd "$HERE/../.." && pwd)"
# Private local state (agent password, logs, results, config.env) lives outside the repo.
U60_HOME="${U60_HOME:-$HOME/u60}"; mkdir -p "$U60_HOME"
[ -f "$U60_HOME/config.env" ] && . "$U60_HOME/config.env"
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
cd "$REPO" || exit 1
echo "== cargo zigbuild (release, aarch64-unknown-linux-musl)"
cargo zigbuild --release --target aarch64-unknown-linux-musl -p zte-agent 2>&1 | tee "$U60_HOME/agent-build.log" | grep -E "^(error|warning: unused|Compiling zte-agent|Finished)" 
BIN="target/aarch64-unknown-linux-musl/release/zte-agent"
[ -x "$BIN" ] || { echo "build failed, see $U60_HOME/agent-build.log"; exit 1; }
ls -l "$BIN"; file "$BIN"; shasum -a 256 "$BIN"
echo "== cargo zigbuild openu60-screen (router touchscreen UI)"
cargo zigbuild --release --target aarch64-unknown-linux-musl -p openu60-screen --bin openu60-screen 2>&1 | tee -a "$U60_HOME/agent-build.log" | grep -E "^(error|Finished)"
SCR="target/aarch64-unknown-linux-musl/release/openu60-screen"
[ -x "$SCR" ] || { echo "screen build failed, see $U60_HOME/agent-build.log"; exit 1; }
ls -l "$SCR"
echo "OK. Deploy: $HERE/04_deploy_agent.sh $REPO/$BIN"
