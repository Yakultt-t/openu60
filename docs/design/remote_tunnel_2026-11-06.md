# OpenU60 远程访问隧道设计（2026-11-06）

目标：手机在任何网络下都能连上家里/身上的 U60 Pro，中间人看不到内容，部署可复制（多节点、可卖）。

## 分层

```
app  ──USB/热点/家庭局域网(Bonjour)──▶ 直连 http://<router>:9090
app  ──────────── 隧道 ─────────────▶ http://127.0.0.1:9090（本机代理）─QUIC─▶ 路由器 zte-agent:9090
                                      udp://127.0.0.1:9091（语音）  ─QUIC datagram─▶ 路由器 :9091
```

- 传输：QUIC（quinn + rustls，TLS 1.3），手机 ⇄ 路由器端到端加密。
- 身份：两端各一把 Ed25519 密钥（路由器 `/data/openu60/tunnel/identity.key`，手机 Keychain/文件）。
  证书自签，验证时只比对公钥（pinning），不看 CA、不看有效期。
  路由器 ID = SHA-256(公钥) 前 8 字节（16 个 hex 字符）。
- 配对：手机在局域网里用密码登录成功后，`POST /api/tunnel/pair` 把自己的公钥交给路由器，
  路由器返回自己的 ID、公钥和中继列表。路由器只接受白名单里的手机（mTLS）。
  以后扫码配对 = 同一份信息编成二维码。
- 中继（`openu60-relay`，UDP 443）：只做介绍和转发，永远看不到明文。
  1. 两端都用一个 UDP socket 向中继 HELLO → CHALLENGE → REGISTER（Ed25519 签名）→ REGISTERED(token)。
  2. 每 15 s PING 保活（保持 NAT 映射）。
  3. 手机 CONNECT(device_id) → 中继给双方发 LINK(link_id, 对方公网地址)。
  4. 双方互发 PROBE 打洞 ~1.5 s；通了就 QUIC 直连，否则 QUIC 包装进 DATA 帧由中继转发。
  5. 中继帧首字节 0x2A（QUIC 首字节固定位 0x40 必为 1，所以 0x2A 永不冲突）。
- 多节点：配置里是一个中继列表；路由器向所有节点注册，手机挑延迟最低的。回国前加香港节点即可。

## 帧格式（UDP，均以 0x2A 开头）

| type | 名称 | 内容 |
|---|---|---|
| 0x01 | HELLO | ver u8 |
| 0x02 | CHALLENGE | nonce[16] addr |
| 0x03 | REGISTER | role u8 pubkey[32] nonce[16] sig[64] |
| 0x04 | REGISTERED | token[16] addr |
| 0x05 | PING | token[16] |
| 0x06 | PONG | token[16] addr |
| 0x07 | CONNECT | token[16] device_id[8] |
| 0x08 | CONNECT_FAIL | token[16] reason u8 |
| 0x09 | LINK | token[16] link_id u32 peer_addr peer_role u8 |
| 0x0A | PROBE | link_id u32 reply u8（端到端直发） |
| 0x0B | DATA | token[16] link_id u32 payload（经中继转发的 QUIC 包） |
| 0x0C | CLOSE | token[16] link_id u32 |

addr = family u8(4/6) + ip + port(BE)。签名消息 = "OU60-relay-register/v1" ‖ role ‖ nonce。

## 代码

- `tunnel/` 共享库：identity、proto、mux（UDP 复用 + 中继会话 + quinn 抽象 socket）、tls、forward、device（路由器角色）、phone（手机角色）、mdns（可选）。
- `relay/` 中继服务二进制，systemd 部署到 relay.example.com。
- 手机端通过一个 C ABI 静态库调用 `tunnel/`（手机 App 不在本仓库）。
- `zte-agent` 新增 `tunnel` 模块与 `/api/tunnel/*`。

## 运维

- 中继：`cargo zigbuild --release --target x86_64-unknown-linux-musl -p openu60-relay`，
  二进制放到 `/usr/local/bin/openu60-relay`，systemd 服务 `openu60-relay`（用户 openu60，UDP 443），
  日志 `journalctl -u openu60-relay -f`。加节点 = 同一二进制部署到新机器，再把地址加进路由器的中继列表
  （`PUT /api/tunnel/config {"relays": ["relay.example.com:443", "hk.example:443"]}`）。
- 路由器：`scripts/u60/06_build_agent.sh` 然后 `scripts/u60/04_deploy_agent.sh`；数据在 `/data/openu60/tunnel/`。
  `GET /api/tunnel/status` 看注册状态、已配对手机和当前连接。
- 命令行调试客户端：`cargo run -p openu60-tunnel --example phone -- <device_id> <device_key>`
  （先用 `--key` 打印公钥并 `POST /api/tunnel/pair`）。

## 已验证（2026-11-06）

- 进程内端到端测试（直连、强制中继、陌生手机被拒）：`cargo test -p openu60-tunnel --features mdns`。
- Mac → relay.example.com → 路由器（运营商 CGNAT）：中继路径，RTT 约 93 ms，HTTP 请求 46 ms 返回。
- iPhone Duo 模拟器：局域网登录后自动配对，“现在测试中继”走中继 109 ms，仪表盘持续刷新。
- 家里的局域网出口就是 U60 本身（手机和路由器公网 IP 相同），所以在家打洞失败属正常，外出用移动数据时才有机会直连。

## 语音通道（2026-11-06 下午修订）

第一版把通话音频放在 QUIC datagram 里，实测走中继时通话卡顿：datagram 和 HTTP 流共用一条连接的
拥塞窗口、pacing 和 ACK 时钟，路由器 5G 链路一丢包就把音频堵在后面，发送缓冲还会把过期的音频
排队送出。现在音频走 QUIC 之外的独立包（首字节 0x2C，`tunnel/src/voice.rs`）：

- 密钥由 QUIC 会话导出（TLS exporter，两个方向各一把 AES-256-GCM），仍然端到端加密，中继看不到内容。
- 同一个 UDP socket，直连或经中继 DATA 帧转发都行；没有拥塞控制、没有重传，晚到即丢。
- 每个包带当前帧和前一帧，接收端按序号去重并按顺序交付，单包丢失不掉音。
- `probe_voice` / 命令行 `--probe` 可以测这条路径的 RTT、抖动、丢包。
  Mac → 中继 → 路由器实测：RTT 平均 37 ms，抖动 5 ms，丢包 0%。
- App 走隧道时仪表盘轮询放慢到 ≥ 5 s，减少通话期间和音频抢 5G 上行。

## 通话体验（2026-11-06 晚修订）

- 语音包改为带前两帧冗余（连丢两包不掉音），app 走隧道时“随时随地访问”卡里有“测试通话音频链路”，
  5 秒内给出往返、抖动、丢包（走的就是通话音频的那条路）。
- 通话状态：代理新增 `call_monitor`，有人看状态时每 250 ms 查一次 `AT+CLCC` 并缓存带版本号，
  `GET /api/call/status?wait=8000&since=<version>` 长轮询到变化即返回；app 在通话建立/进行中改用长轮询。
  tiny_http 工作线程从 4 加到 8，长轮询不会饿死其他请求。
- 静音：app 先本地静音（发给路由器的音频直接变静音帧）并立即更新按钮，`AT+CMUT` 在后台补发。

## 上行节拍器（2026-11-06 夜修订）

症状：走中继通话时对方听我这边断断续续，而且越聊越明显；我听对方正常。原因：手机 → 路由器方向
没有抖动缓冲。ZTE 的 DSP 工具收到 RTP 就直接播进通话，而手机采集回调本身就是成批出帧，
再加上网络抖动，帧到得不均匀就直接变成对方听到的卡顿；下行方向 app 里有自适应抖动缓冲所以没事。

改法（`zte-agent/src/voice_bridge.rs`）：手机来的帧先进缓冲，`voice-uplink` 线程按 20 ms 的稳定时钟
取一帧打成 RTP 发给 DSP。起播前先攒 60 ms；缺帧时送静音帧并把目标加深一帧（上限 200 ms），
30 s 没缺帧就减一帧（下限 40 ms）；超过目标 +60 ms 的积压直接丢，防止延迟越聊越大。
`GET /api/call/audio/status` 新增 `uplink_depth_ms / uplink_target_ms / uplink_underruns /
uplink_dropped / uplink_silence`，通话后可以看。

## App 改版（2026-11-06 晚）

- 登录：启动即先找路由器（配置网关 → 上次家庭地址 → Bonjour → 中继），登录页显示查找状态；
  配对信息（路由器 ID/公钥/中继）和密码一起放进 iCloud 钥匙串，新设备在外面也能走中继登录。
- 中继端“引导通道”：没配对的手机也能完成 QUIC 握手，但流只能到路由器的 127.0.0.1:9092
  （只放行 /api/auth/login、/api/tunnel/info、/api/tunnel/pair，密码错 10 次/15 分钟封锁），
  登录后自动配对再重连拿到完整权限。未配对连接每分钟最多 20 个。
- 首页改成小组件网格（`Features/Home/Widgets/`）：小/中/大三种尺寸，长按改尺寸，编辑模式
  抖动 + 减号移除 + 拖拽排序 + 添加组件；布局存在 UserDefaults `home_widgets_v1`。
  四合一快捷按钮和“信号详情页”取消，信号、小区、历史曲线、流量、电池、Wi-Fi、设备等直接当组件。
  左上角状态显示 局域网 / 远程·直连 / 远程·中继，不再显示秒数。
- 设置改成单页分组（`Features/More/SettingsRootView.swift`）：状态卡 → 网络 / 信号与基站 / SIM /
  路由器 / 设备 / 自动化与工具 / 高级（Mihomo、ADB）/ App。原 Network/Router/Tools/App 四个分页删除。
- 新增 Mihomo 代理页（状态、启停、开机自启、透明代理、代理组切换、日志）和 USB 模式切换；
  删除蓝牙/9eSIM B1 功能（U60 没有蓝牙）。

## 通话音频第三轮（2026-11-06 深夜）

- 手机接收缓冲：首次攒够后不再因单次缺帧整段重填（原来每次缺帧空 120–400 ms，即“割裂感”）；
  积压高于目标 80 ms 持续约 1 s 就削 20 ms（时钟漂移）；缺的帧用上一帧衰减填补（U60A 序号跟踪）；
  走隧道时起始 160 ms / 下限 100 ms，局域网 80 / 60。通话界面诊断行显示 lost/drop/buf/线路/RTT。
- 路由器上行节拍器缺帧只补一帧静音继续走，连续 200 ms 没数据才重新攒。
- 语音冗余前三帧（4 帧/包）。
- 路由器音频线程与隧道 tokio 线程设 SCHED_FIFO（失败则 nice），负载 2+ 的机器上节拍不再被挤。
- 静音：AT+CMUT 该调制解调器不支持（ERROR）。ODU 工具 `-d 2,2147483652` 打开的是扬声器 + 路由器
  自带麦克风，疑似对端能听到路由器周围声音。新增 `PUT /api/call/audio/settings {"mic_mode":
  default|mute_tx|no_mic}`（mute_tx 加 `-u 0`，no_mic 只开 `-d 2`），存 /data/openu60/voice.json，
  app 设置 → 设备 → 通话音频 里选；需要打测试电话验证哪种模式既能听到 app 音频又能彻底静音。
- 启动不再有登录等待页：有存储密码直接进 app，后台连接；密码被拒才回登录页。
  局域网探测改为并行（1.8 s 超时），Bonjour 同时进行。

## 通话断联与上行延迟（2026-11-06 夜，根据路由器/中继日志）

- 日志显示通话期间 `[voice] phone endpoint` 在两个本地端口间来回跳：iPad 也在轮询通话状态，看到
  “通话中”就自己开了音频流，和 iPhone 抢同一个语音桥。现在：只有拨出/接听的那台设备开音频
  （`call_owned_at`，4 小时内重启可恢复），路由器语音桥锁定第一个端点，另一个端点要等原端点
  静默 2.5 s 才能接管，被拒包计入 `foreign`。
- 同一时段路由器“relay session lost”，中继日志里设备会话过期：上一版给音频线程和隧道线程设了
  SCHED_FIFO，DSP 工具空转时（每秒上千个静音包）实时线程把隧道线程饿死，PING 发不出去。
  已改回 nice（-8 / -5）。中继和两端现在都把转发的数据当作存活证明，PING 改 10 s、超时 45 s，
  中继会话 TTL 90 s，QUIC 空闲超时 60 s、keep-alive 5 s。
- 通话中：不再因发现局域网或网络切换而换路径；每 2 s 检查隧道，断了立即重连；状态轮询连续
  失败 20 s 才结束通话界面（路由器恢复后轮询会重新看到通话）。
- 上行延迟：采集改为 20 ms 一片（原来 16 kHz 下 1024 帧 = 64 ms 一批）；路由器上行缓冲起始 40 ms、
  上限 120 ms、8 s 无缺帧就缩、积压超过目标 40 ms 即丢。

## 后台来电（2026-11-06）

- 根因：路由器上从未安装 APNs 推送密钥（`/api/push/status` enabled=false），app 被杀后没有任何东西能叫醒它。
  另外 04_deploy_agent.sh 每次重写启动脚本时会丢掉 `ZTE_AGENT_LIVE_ACTIVITY_CONFIG`，现已保留。
- 推送 token 改为按设备存储（`device_id` + 名称），短信/彩信推给所有设备，来电 VoIP 推给所有
  “当前没在轮询”的设备（每台设备轮询时带 `device=`，不再因为别的设备开着就全体不推）。
- APNs 环境按设备自动识别：先试配置的环境，BadDeviceToken 时换另一个，成功后记住（Xcode 构建走
  sandbox，TestFlight 走 production），两边都不认才删除 token。
- 多设备来电：谁接听谁拥有音频；其他设备看到“已在别处接听”后停止响铃（CallKit answeredElsewhere），
  CallKit reset 只有拥有通话的设备才会挂断路由器上的电话（原来会直接把来电挂掉）。
- 手表拨出的电话由 iPhone 承载音频。

## 中国移动卡漫游 Telstra 时 VoLTE 不注册（2026-11-06）

- 同一张卡在 S26 Ultra 上 VoLTE 正常，路由器上不行。AT 命令看不到 IMS，用 QRTR 直接问 QMI：
  IMSA（服务 33，先 Bind 0x0033）注册状态“未注册、无错误码”；IMS settings（服务 18，先 Bind 0x0098）
  的 Get IMS Services Enabled（0x0090）里 TLV 0x19“IMS 注册服务启用”= 0。
- 用 Set（0x008f，TLV 0x18 = 1）打开后 15 秒内注册成功（WWAN），短信和语音服务都变为可用。
- zte-agent 新增 `ims_guard`：每 60 秒检查一次，关了就重新打开。工具 `src/bin/qmi_probe.rs`。
- 调制解调器数据 profile 2 就是 `ims` APN；彩信承载原来写 profile 2，会覆盖 IMS APN，已改用 profile 4。
- 短信发送改回走 IMS（zte_wms_ims_switch=1）。
