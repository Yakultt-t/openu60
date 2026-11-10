# 远程访问与中继

人不在路由器旁边时，手机 App 通过一条端到端加密的隧道访问路由器。路由器和手机通常都在运营商 NAT 后面，没有公网 IP，所以需要一台公网上的中继/会合服务器：它负责介绍双方，打洞失败时再帮忙转发，但始终看不到内容。完整的协议设计见 [docs/design/remote_tunnel_2026-11-06.md](../design/remote_tunnel_2026-11-06.md)，下面是摘要和部署方法。

## app 怎么找路由器

手机 App 按这个顺序尝试：配置的网关（默认 `192.168.0.1`）→ 上次在局域网连上的地址、家庭 Wi-Fi 里的地址 → Bonjour（`_openu60._tcp`）→ 隧道。本地候选地址并行探测，都不通才走隧道。走隧道时界面显示“远程 · 直连”或“远程 · 中继”，并且每 30 秒看一眼是否回到了局域网（通话中不切换）。详见 [architecture.md](architecture.md#远程访问)。

## 隧道设计摘要

- **传输**：QUIC（quinn + rustls，TLS 1.3），手机和路由器之间端到端加密。
- **身份**：两端各有一把 Ed25519 密钥（路由器上是 `/data/openu60/tunnel/identity.key`，手机上存在钥匙串/文件里）。证书是自签的，校验时只比对对方的公钥（pinning），不看 CA 和有效期。路由器 ID 是公钥 SHA-256 的前 8 字节（16 个十六进制字符）。
- **配对**：手机第一次在局域网用密码登录后，自动调用 `POST /api/tunnel/pair` 交出自己的公钥；路由器返回自己的 ID、公钥、中继列表和一个长期有效的设备 token。路由器只接受白名单里的公钥（双向 TLS）。配对信息存在 iCloud 钥匙串里，同一 Apple 账户下的其他设备可以在外面先经中继连上，再用密码给自己配对。
- **引导通道**：没配对的手机也能完成 QUIC 握手，但它的流只会被转到路由器的 `127.0.0.1:9092`，那里只提供 `/api/auth/login`、`/api/tunnel/info` 和 `/api/tunnel/pair`。密码错 10 次后锁 15 分钟，未配对连接每分钟最多 20 个。
- **中继协议**（UDP，所有帧以 `0x2A` 开头，QUIC 包的首字节不可能是它）：两端用同一个 UDP socket 向中继 HELLO → CHALLENGE → REGISTER（Ed25519 签名，签名内容为 `"OU60-relay-register/v1" ‖ role ‖ nonce`）→ REGISTERED。之后每 10 秒 PING 一次保持 NAT 映射，45 秒收不到回应就重新注册；中继侧的会话 90 秒过期。手机发 CONNECT(路由器 ID)，中继给双方各发一个 LINK（带对方的公网地址）。
- **打洞**：双方互发 PROBE 约 1.5 秒，收到对方公网地址的回应就走直连；否则把 QUIC 包装进 DATA 帧，由中继转发。QUIC 空闲超时 60 秒，keep-alive 5 秒。
- **语音**：通话音频不走 QUIC，而是在同一个 socket 上用独立的 AEAD 包发送（首字节 `0x2C`）。两个方向的 AES-256-GCM 密钥由 QUIC 会话导出，没有重传，每包带前 3 帧冗余。见 [call-audio.md](call-audio.md)。
- **手机端**：App 里的隧道库（`tunnel/src/phone.rs`）在本机开 `127.0.0.1:<端口>`（HTTP）和一个 UDP 语音端口，App 把服务器地址换成本机即可。`tunnel/examples/phone.rs` 是同一套代码的命令行版本，可以在 Mac 上模拟一部手机（见下文）。

## 没有默认中继

公开版本**不带任何默认中继**：`tunnel/src/lib.rs` 的 `default_relays()` 只在编译时设置了 `OPENU60_DEFAULT_RELAYS` 才有值。所以刚装好的路由器只能在局域网里访问，想在外面用，就按下面的步骤自建一个中继，再告诉路由器。中继看不到内容，但能看到双方的公网 IP、路由器 ID、连接时间和流量大小，所以最好用你自己的。

## 自建中继

中继是一个单文件、无状态的 UDP 服务（`relay/`，二进制名 `openu60-relay`），不需要数据库，也不需要预先登记设备，最小规格的 VPS 就够用。尽量选离路由器和手机都近的地方：在运营商级 NAT（CGNAT）后面经常打不通洞，这时所有流量都要经过中继，延迟就取决于中继的位置。

**1. 编译**

```bash
# 在 Mac 上交叉编译 x86_64 Linux（ARM 服务器换成 aarch64-unknown-linux-musl）
rustup target add x86_64-unknown-linux-musl
cargo zigbuild --release --target x86_64-unknown-linux-musl -p openu60-relay
# 产物：target/x86_64-unknown-linux-musl/release/openu60-relay
# 也可以直接在服务器上：cargo build --release -p openu60-relay
```

**2. 安装为 systemd 服务**

```bash
sudo install -m 755 openu60-relay /usr/local/bin/openu60-relay
sudo useradd --system --no-create-home --shell /usr/sbin/nologin openu60-relay
sudo tee /etc/systemd/system/openu60-relay.service >/dev/null <<'EOF'
[Unit]
Description=OpenU60 relay
After=network-online.target
Wants=network-online.target

[Service]
User=openu60-relay
ExecStart=/usr/local/bin/openu60-relay --listen 0.0.0.0:443 --log info
Restart=always
RestartSec=2
# 非 root 用户绑定 443 端口
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF
sudo systemctl daemon-reload
sudo systemctl enable --now openu60-relay
journalctl -u openu60-relay -f
```

端口可以随意选。UDP 443 在很多受限网络里也能通（QUIC 用的就是它）。

**3. 网络**

- 在云服务商的安全组和本机防火墙里放行这个 **UDP** 端口（例如 `sudo ufw allow 443/udp`），不需要开 TCP。
- 给服务器配一个域名（A 记录）。如果 DNS 托管在 Cloudflare，必须设成“仅 DNS”（灰色云朵），它的代理不会转发任意 UDP。

**4. TLS 证书：不需要**

中继自己不做 TLS，也不需要证书。加密在手机和路由器之间端到端完成（自签证书加公钥 pinning），中继只转发它解不开的 QUIC 包和语音包，以及签过名的注册/介绍帧。

## 让路由器和 app 用你的中继

app 不单独配置中继：它在配对时从路由器的 `/api/tunnel/info` 拿到中继列表，之后每次启动、登录后都会刷新一次，列表变了就自动重新配对。所以只要改路由器上的配置：

```bash
# api 函数见 install.md 第 5 节
api PUT /api/tunnel/config '{"relays": ["relay.example.com:443"]}'
api GET /api/tunnel/status | jq '.data.relays_status'     # registered 应为 true
```

- 路由器会向列表里的**每一个**中继注册；手机按列表顺序逐个尝试，第一个连得通的胜出，所以把离你最近的放在第一位。可以同时写多个，例如 `["hk.example.com:443", "relay.example.com:443"]`。
- 配置保存在 `/data/openu60/tunnel/config.json`，`PUT` 后立即生效并重新注册。也可以用 `{"enabled": false}` 关闭中继访问。
- 想让新装的路由器默认就用你的中继，就在 `${U60_HOME:-~/u60}/config.env` 里写 `export OPENU60_DEFAULT_RELAYS=relay.example.com:443`（多个用逗号分隔），再用 `06_build_agent.sh` 重新编译 agent。手机端的中继列表来自路由器，app 不用改。配对过手机的路由器已经把当时的中继列表写进了 `config.json`，仍需要用上面的 `PUT` 修改。

**用命令行测试**（在 Mac 上模拟一部手机）：

```bash
cargo run -p openu60-tunnel --example phone -- --key        # 打印本机公钥（身份保存在 ~/.openu60/phone.key）
api POST /api/tunnel/pair '{"public_key": "<上一步的公钥>", "name": "Mac"}'
api GET /api/tunnel/info | jq .                             # 取 device_id 和 public_key
adb forward --remove tcp:19090                              # 测试客户端也要用本机的 19090 端口
cargo run -p openu60-tunnel --example phone -- <device_id> <public_key> \
  --relay relay.example.com:443 --force-relay --probe
```

连上后，agent 在本机的 `http://127.0.0.1:19090` 可用（语音在 UDP 19091）；`--probe` 会测语音通道的往返、抖动和丢包。测完记得用 `POST /api/tunnel/unpair` 删掉这个测试身份（见下文）。

## 自建推送网关（可选）

手机 App 被关掉时，短信提醒和来电要靠 Apple 的推送（APNs）。推送网关 `openu60-pushgw`（`pushgw/`）持有 APNs 密钥，路由器不持有：路由器用自己的隧道身份（Ed25519）给每个请求签名，`POST /v1/push` 给网关，网关检查签名、时间偏差和每台路由器的限额（突发 60 次、每秒补 1 次，每天最多 30 个不同的设备 token）后转发给 Apple。

- 编译：`cargo zigbuild --release --target x86_64-unknown-linux-musl -p openu60-pushgw`（或在服务器上 `cargo build --release -p openu60-pushgw`）。
- 配置（环境变量）：`PUSHGW_LISTEN`（默认 `0.0.0.0:443`）、`PUSHGW_CERT` / `PUSHGW_KEY`（TLS 证书链和私钥，PEM，例如 Let's Encrypt 的）、`PUSHGW_APNS_KEY`（`.p8` 文件）、`PUSHGW_KEY_ID`、`PUSHGW_TEAM_ID`、`PUSHGW_BUNDLE_ID`。`.p8` 必须属于 App 所在的开发者团队，`PUSHGW_BUNDLE_ID` 要和 App 一致，所以只有自己构建 App 的人才需要它。
- 和中继可以放在同一台机器上：中继用 UDP，网关用 TCP（HTTPS），端口不冲突。健康检查：`curl https://push.example.com/healthz`。
- 让路由器用它：在启动脚本 `/data/local/tmp/start_zte_agent.sh` 里 `export ZTE_AGENT_PUSH_GATEWAY=https://push.example.com/v1/push` 后重启 agent（方法见 [router-runtime.md](router-runtime.md#守护进程zte-agent-runsh)），或者编译时在 `config.env` 里写 `export OPENU60_PUSH_GATEWAY=…`。设成 `off` 则关闭。

## 安全与隐私

- 中继看不到内容，但能看到元数据：双方公网 IP、路由器 ID、连接时间、流量大小。
- 任何知道路由器 ID 和公钥的人，都能经中继摸到引导端口（只能登录和配对，有限速）。agent 密码要够长、够随机，`04_deploy_agent.sh` 默认生成 24 位随机密码。
- 配对后的设备 token 长期有效，**改 agent 密码并不会让它失效**，只有解绑才行。手机丢了，就在另一台设备上，或用 API 解绑它：

```bash
api GET /api/tunnel/status | jq '.data.peers'              # 已配对设备：key、name、added
api POST /api/tunnel/unpair '{"public_key": "<要删除的 key>"}'
```

  在 app 里“退出登录”时，也会解绑当前这台设备。
- 推送 token 另外存放，解绑后可以用 `DELETE /api/push/register?device=<id>` 清掉（`GET /api/push/status` 能看到设备列表）。

## 其他远程方式

- **家庭局域网**：路由器处于 Wi-Fi 客户端/中继模式时，家里的手机可以直接通过它在家庭网里的地址访问（app 会自动尝试）。
- **Tailscale**：上游的 Tailscale 功能仍然保留（`/api/tailscale/*`，app 的“随时随地访问”页面里也有入口），适合已经在用 tailnet 的用户。
- 不要把 agent 的 9090 端口直接暴露到公网。
