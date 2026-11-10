#!/usr/bin/env bash
# OpenU60 relay: one-command install on a fresh Linux server (Ubuntu, Debian, CentOS/Rocky/
# Alma, Fedora, Amazon Linux...; x86_64 or arm64; systemd required).
#
#   curl -fsSL https://raw.githubusercontent.com/Yakultt-t/openu60/main/scripts/relay/install.sh | sudo bash
#
# Options (append after `bash -s --` when piping, e.g. `| sudo bash -s -- --port 8443`):
#   --port N        UDP port to listen on (default 443)
#   --bin PATH      install this openu60-relay binary instead of downloading one
#   --version TAG   download this release instead of the latest
#   --uninstall     stop and remove the relay
#
# What it does: downloads the static openu60-relay binary from the GitHub release (checks its
# SHA-256), creates the `openu60` system user, installs a systemd service that starts at boot
# and restarts on failure, opens the UDP port in ufw / firewalld / iptables, checks that the
# relay answers, and prints the address to add in the app.
#
# The relay never sees the content it forwards (phone and router talk end-to-end encrypted
# QUIC through it), keeps nothing on disk and needs no domain name or certificate.
set -euo pipefail

REPO="${OPENU60_REPO:-Yakultt-t/openu60}"
SERVICE="${OPENU60_RELAY_SERVICE:-openu60-relay}"
BIN="${OPENU60_RELAY_PATH:-/usr/local/bin/openu60-relay}"
RUN_USER="openu60"
PORT=443
LOCAL_BIN=""
TAG=""
ACTION=install

say()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!! \033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31mxx \033[0m %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --port) PORT="${2:-}"; shift 2 ;;
        --bin) LOCAL_BIN="${2:-}"; shift 2 ;;
        --version) TAG="${2:-}"; shift 2 ;;
        --uninstall) ACTION=uninstall; shift ;;
        -h|--help) sed -n '2,20p' "$0" 2>/dev/null || true; exit 0 ;;
        *) die "unknown option: $1 (see --help)" ;;
    esac
done

case "$PORT" in ''|*[!0-9]*) die "--port needs a number" ;; esac
[ "$PORT" -ge 1 ] && [ "$PORT" -le 65535 ] || die "--port must be 1-65535"
[ "$(id -u)" = 0 ] || die "run as root (sudo bash)  /  请用 root 运行（sudo bash）"
[ "$(uname -s)" = Linux ] || die "Linux only"
command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ] || die "systemd is required"

UNIT="/etc/systemd/system/${SERVICE}.service"

# --- firewall -------------------------------------------------------------------------
fw_open() {
    local p="$1" done=0
    if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q "^Status: active"; then
        ufw allow "${p}/udp" comment "OpenU60 relay" >/dev/null && say "ufw: allowed ${p}/udp"
        done=1
    fi
    if command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
        firewall-cmd --quiet --permanent --add-port="${p}/udp" && firewall-cmd --quiet --reload && say "firewalld: allowed ${p}/udp"
        done=1
    fi
    if [ $done = 0 ] && command -v iptables >/dev/null 2>&1; then
        # Images such as Oracle Cloud's ship iptables rules that reject everything not listed.
        if iptables -S INPUT 2>/dev/null | grep -qE -- "-j (REJECT|DROP)|-P INPUT DROP"; then
            iptables -C INPUT -p udp --dport "$p" -j ACCEPT 2>/dev/null || iptables -I INPUT -p udp --dport "$p" -j ACCEPT
            say "iptables: allowed ${p}/udp"
            if command -v netfilter-persistent >/dev/null 2>&1; then
                netfilter-persistent save >/dev/null 2>&1 || true
            elif [ -d /etc/iptables ]; then
                iptables-save > /etc/iptables/rules.v4 2>/dev/null || true
            else
                warn "iptables rule added but not saved; it may be gone after a reboot"
            fi
        fi
    fi
}

fw_close() {
    local p="$1"
    if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q "^Status: active"; then
        ufw delete allow "${p}/udp" >/dev/null 2>&1 || true
    fi
    if command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
        firewall-cmd --quiet --permanent --remove-port="${p}/udp" 2>/dev/null && firewall-cmd --quiet --reload || true
    fi
    if command -v iptables >/dev/null 2>&1; then
        iptables -D INPUT -p udp --dport "$p" -j ACCEPT 2>/dev/null || true
    fi
}

current_port() {
    [ -f "$UNIT" ] || return 0
    sed -n 's/.*--listen [^ ]*:\([0-9]*\).*/\1/p' "$UNIT" | head -1
}

# --- uninstall ------------------------------------------------------------------------
if [ "$ACTION" = uninstall ]; then
    old_port="$(current_port)"
    systemctl disable --now "$SERVICE" >/dev/null 2>&1 || true
    rm -f "$UNIT" "$BIN"
    systemctl daemon-reload
    [ -n "$old_port" ] && fw_close "$old_port"
    # Keep the user while any other service (another relay, the push gateway) still runs as it.
    if id "$RUN_USER" >/dev/null 2>&1 \
        && ! grep -lqs "^User=${RUN_USER}\$" /etc/systemd/system/*.service \
        && ! pgrep -u "$RUN_USER" >/dev/null 2>&1; then
        userdel "$RUN_USER" 2>/dev/null || true
    fi
    say "OpenU60 relay removed  /  已卸载 OpenU60 转发服务"
    exit 0
fi

# --- binary ---------------------------------------------------------------------------
case "$(uname -m)" in
    x86_64|amd64) ARCH=x86_64 ;;
    aarch64|arm64) ARCH=aarch64 ;;
    *) die "unsupported CPU $(uname -m) (x86_64 and arm64 are supported)" ;;
esac

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if [ -n "$LOCAL_BIN" ]; then
    [ -f "$LOCAL_BIN" ] || die "--bin: $LOCAL_BIN not found"
    cp "$LOCAL_BIN" "$TMP/openu60-relay"
else
    command -v curl >/dev/null 2>&1 || die "curl is required (apt install curl / dnf install curl)"
    ASSET="openu60-relay-${ARCH}-linux-musl"
    if [ -n "$TAG" ]; then BASE="https://github.com/${REPO}/releases/download/${TAG}"
    else BASE="https://github.com/${REPO}/releases/latest/download"; fi
    say "downloading ${ASSET}"
    curl -fsSL --retry 3 -o "$TMP/openu60-relay" "${BASE}/${ASSET}" \
        || die "download failed: ${BASE}/${ASSET}"
    if curl -fsSL --retry 3 -o "$TMP/sha256" "${BASE}/${ASSET}.sha256" 2>/dev/null; then
        want="$(awk '{print $1}' "$TMP/sha256")"
        got="$(sha256sum "$TMP/openu60-relay" | awk '{print $1}')"
        [ "$want" = "$got" ] || die "checksum mismatch for ${ASSET}"
        say "checksum ok"
    else
        warn "no checksum file published for ${ASSET}; skipped the check"
    fi
fi
chmod 755 "$TMP/openu60-relay"
"$TMP/openu60-relay" --version >/dev/null 2>&1 || die "the binary does not run on this machine"

# --- port -----------------------------------------------------------------------------
if command -v ss >/dev/null 2>&1; then
    holder="$(ss -Hlunp "sport = :$PORT" 2>/dev/null | grep -v openu60-relay | head -1 || true)"
    [ -z "$holder" ] || die "UDP port $PORT is already used by another program: $holder  (use --port)"
fi

# --- install --------------------------------------------------------------------------
if ! id "$RUN_USER" >/dev/null 2>&1; then
    useradd --system --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin "$RUN_USER" 2>/dev/null \
        || useradd -r -M -s /sbin/nologin "$RUN_USER"
fi
old_port="$(current_port)"
install -m 755 "$TMP/openu60-relay" "$BIN"

cat > "$UNIT" <<EOF
[Unit]
Description=OpenU60 relay (rendezvous and encrypted packet forwarding)
After=network-online.target
Wants=network-online.target

[Service]
User=${RUN_USER}
ExecStart=${BIN} --listen 0.0.0.0:${PORT}
Restart=always
RestartSec=3
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectControlGroups=yes
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable "$SERVICE" >/dev/null 2>&1
systemctl restart "$SERVICE"
[ -n "$old_port" ] && [ "$old_port" != "$PORT" ] && fw_close "$old_port"
fw_open "$PORT"

# --- check ----------------------------------------------------------------------------
sleep 1
systemctl is-active --quiet "$SERVICE" || { journalctl -u "$SERVICE" -n 20 --no-pager >&2 || true; die "the service did not start"; }
"$BIN" --probe "127.0.0.1:${PORT}" >/dev/null || die "the relay does not answer on 127.0.0.1:${PORT}"
say "$("$BIN" --version) is running (systemctl status ${SERVICE})"

# Public address: the one on the default route, or ask a public "what is my IP" service when
# the server sits behind the provider's NAT (AWS, GCP, Oracle...).
ip_addr="$(ip -4 route get 1.1.1.1 2>/dev/null | sed -n 's/.* src \([0-9.]*\).*/\1/p' | head -1)"
case "$ip_addr" in
    ''|10.*|192.168.*|172.1[6-9].*|172.2[0-9].*|172.3[01].*|100.6[4-9].*|100.[7-9][0-9].*|100.1[01][0-9].*|100.12[0-7].*)
        pub="$(curl -4 -fsS --max-time 5 https://api.ipify.org 2>/dev/null || true)"
        [ -n "$pub" ] && ip_addr="$pub" ;;
esac
addr="${ip_addr:-<server-public-IP>}"
[ "$PORT" = 443 ] || addr="${addr}:${PORT}"

cat <<EOF

  OpenU60 relay is ready / 转发服务器已就绪

  Address to add in the app / 在 App 里添加的地址:

      ${addr}

  App: Settings → Anywhere access → Relay servers → Add server
  App：设置 → 随处访问 → 转发服务器 → 添加服务器

  If your cloud provider has its own firewall (AWS/GCP/Azure/Oracle "security group",
  Alibaba/Tencent "安全组"), allow UDP ${PORT} there too.
  如果云服务商有自己的防火墙（安全组），还要在控制台里放行 UDP ${PORT}。

  Logs / 日志:      journalctl -u ${SERVICE} -f
  Update / 更新:    run this script again / 再运行一次本脚本
  Remove / 卸载:    curl -fsSL https://raw.githubusercontent.com/${REPO}/main/scripts/relay/install.sh | sudo bash -s -- --uninstall

EOF
