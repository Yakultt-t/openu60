# 架构

这篇文档讲短信、彩信、通话、推送和远程访问的数据怎么走，以及各段代码在哪里。安装步骤见 [install.md](install.md)。

## 总览

```
中国 SIM ─▶ 基带（SDX75）─▶ 厂商守护进程（zte_topsw_wms 等）─▶ ubus / sms.db
                 │  QMI over QRTR（WMS、IMS、Voice、UIM）、AT 口 /dev/at_mdm0
                 ▼
            zte-agent（Rust，/data/zte-agent，由守护脚本拉起）
              ├─ HTTP API  :9090    局域网直连；走隧道时由隧道转发
              ├─ 语音桥    UDP :9091   通话音频
              ├─ 网页管理面板（嵌入在 :9090）
              ├─ 推送 ──HTTPS──▶ openu60-pushgw（自建）──▶ Apple ──▶ iPhone（短信、彩信、来电、流量提醒）
              │         （或用自己的 .p8 直连 APNs）
              ├─ 隧道端 ──UDP──▶ openu60-relay（自建）◀──UDP── 手机 App 里的隧道库
              └─ 屏幕守护 ──▶ openu60-screen watch（连按三下电源键切换触摸屏界面）
```

| 部分 | 代码 |
|---|---|
| 路由器 agent | `zte-agent/src/`：`sms.rs`、`sms_crypto.rs`、`push.rs`、`mms/`、`call_monitor.rs`、`telephony.rs`、`voice_bridge.rs`、`ims_guard.rs`、`dns_guard.rs`、`wifi_wan.rs`、`data_plan.rs`、`tunnel.rs` |
| 隧道 | `tunnel/src/`（`device.rs` 路由器端、`phone.rs` 手机端、`relay.rs` 中继、`voice.rs` 语音通道） |
| 中继 | `relay/src/main.rs` |
| 推送网关 | `pushgw/src/main.rs` |
| 触摸屏 | `screen/src/`（`main.rs` 切换与事件循环、`pages/` 各页面、`display.rs` DRM 输出、`input.rs` 触摸和电源键）；agent 端 `screen_watch.rs`、`screen_api.rs` |
| 路由器脚本 | `scripts/u60/device/`：`zte-agent-run.sh`（守护进程）、`wifi_uplink.sh`、`wifi_uplink_dhcp.sh` |
| 安装工具 | `desktop/U60Toolkit/`（SwiftUI 界面）+ `Resources/payload/u60.sh`（实际执行安装的脚本） |

手机 App（U60 助手）不开源，下文提到的 App 行为只用来说明 agent 的接口是怎么被用的。

agent 只启动一个 `ubus listen` 进程（`event_bus.rs`），把固件事件分发给各个订阅者，避免每个功能各开一个监听。

## 短信

```
基带 ─▶ zte_topsw_wms ─▶ /etc_rw/ztembb/ztesms/sms_db/sms.db
              │ ubus 事件 zwrt_wms_status_event（new_sms_received）
              ▼
        zte-agent：event_bus ─▶ push-sms 线程 / 短信转发
              │ zwrt_wms zte_libwms_get_sms_data（必要时解密）
              ├─▶ APNs ─▶ iPhone 通知（可直接回复）
              └─▶ Telegram / ntfy / Webhook / Discord / Slack / 短信（可选转发）

iPhone app ── POST /api/sms/list | /api/sms/read | /api/sms/send ──▶ zte-agent ──▶ zwrt_wms
```

接收：

1. 基带收到短信（VoLTE 注册成功时走 IMS，否则走普通网络），厂商守护进程 `zte_topsw_wms` 把它写进 `sms.db`，并发出 ubus 事件 `zwrt_wms_status_event`。
2. agent 的 `push-sms` 线程等 700 ms（让固件先存好），再用 `zwrt_wms zte_libwms_get_sms_data` 取出 id 比水位线大的短信。
3. 每条新短信发一条 APNs 提醒：`thread-id` 为发件人，`category` 为 `OPENU60_SMS`（带“回复”动作），自定义字段有 `kind: "sms"` 和 `sms_id`。同一条短信用 `apns-collapse-id` 去重，并按各设备的通知偏好过滤。
4. app 在前台时自己读列表，并用应用内横幅提示。

STD（海外版）固件会加密短信：`number`、`content` 字段是 `base64(iv[12] ‖ tag[16] ‖ 密文)`，算法为 AES-256-GCM。agent 自己生成 32 字节会话密钥，用 `zwrt_web web_crt_get` 拿到的公钥做 RSA 加密，再经 `web_http_enstr_set` 交给固件（`sms_crypto.rs`）。这个密钥是全局的，用浏览器登录一次路由器后台就会被换掉，所以 agent 解密失败时会自动重新协商；发送前也总会重新协商一次。

发送：`POST /api/sms/send` 会先清理号码（去掉空格、括号、方向标记，多个收件人用 `;` 分隔），加密后交给 `zte_libwms_send_sms`。走 IMS 还是普通网络由固件按 `zwrt_wms.config.wms_ims_flag` 决定，`ims_guard` 让这个标志跟 IMS 实际注册状态保持一致（见 [china-sim.md](china-sim.md)）。

角标与已读（App 端）：

- 角标就是未读短信/彩信的数量，每次已读状态变化都会重算。推送里不带 `badge` 字段，角标由 app 计算。
- 在通知里清除或回复某条短信，会把这条以及同一会话里更早的未读消息标为已读，并报给路由器；路由器暂时连不上时先记在本地，下次连上再补报。
- 打开会话时，会移除这个会话已经送达的通知。

## 彩信

厂商固件不支持彩信：收到彩信通知（WAP Push）后，它把正文换成占位文本 `multimedia_information`（发件人显示为 `99`）存进库里，下载地址就丢了。所以 agent 自己实现了整条 MM1 链路（`zte-agent/src/mms/`）。

```
MMSC ─WAP Push 短信（端口 2948）─▶ 基带 ─▶ 厂商守护进程（存库、换成占位）
                                     │
              第二个只读 QMI WMS 客户端（mms-wms 线程）：新消息指示 → Raw Read
                                     ▼
              WAP 重组 → m-notification.ind（下载地址）
                                     ▼
       按 SIM 归属运营商拉起彩信 PDN（profile 4，rmnet_data*）
                                     ▼
       curl --interface <彩信网卡> -x <WAP 代理> ─▶ GET m-retrieve.conf ─▶ /data/openu60/mms/
                                     ▼
       POST m-notifyresp.ind；删掉占位短信；APNs（kind: "mms"）
```

接收：

1. MMSC 发来一条 WAP Push 短信（8-bit 编码，UDH 目的端口 2948，常常分成两段级联）。
2. `mms/qmi.rs` 用 `AF_QIPCRTR` 套接字连基带的 WMS 服务（服务号 5，节点/端口每次开机都会变，用 `qrtr-lookup` 查），发 `Set Event Report`（0x0001）订阅新消息指示；收到“已存储”的指示就马上 `Raw Read`（0x0022），赶在厂商守护进程处理之前读回原始 PDU。短信路由和确认仍由厂商守护进程负责，agent 不改动。
3. `wap.rs` 解析 SMS-DELIVER、重组级联短信、拆 WSP Push；`pdu.rs` 解出 `m-notification.ind`（Content-Location、事务 ID、发件人、大小、有效期）。
4. `bearer.rs` 按 SIM 卡的归属运营商（`zwrt_zte_mdm.api get_sim_info` 返回的 `mdm_mcc`/`mdm_mnc`，而不是漫游所在的网络）选出彩信 APN、MMSC 和 WAP 代理，用 `zwrt_apn_object set_apn_at_cid` 和 `zwrt_data set_wwaniface` 在 profile 4 上拉起第二条 PDN。不用 profile 2，是因为很多运营商配置里它就是 IMS（VoLTE）的 profile。默认路由不受影响。
5. `http.rs` 调固件自带的 `curl`，绑定彩信网卡、经代理去 GET 下载地址，拿到 `m-retrieve.conf` 后把各部分存进 `/data/openu60/mms/`（`index.json` 加每个附件一个文件，最多 500 条），再 POST `m-notifyresp.ind`。
6. 删除厂商留下的占位短信，发 APNs 提醒（`kind: "mms"`、`mms_id`）。下载失败会重拨 PDN 再试一次；agent 重启约 20 秒后，会接着完成上次没下完的彩信。

发送：app 把图片（长边缩到 1280 px 的 JPEG，附件总量不超过 900 KB）或音频以 base64 放进 `POST /api/mms/send`；agent 编码成 `m-send.req`（上限约 1.2 MB），经同一条 PDN POST 给 MMSC，再解析返回的 `m-send.conf`。app 里彩信用负数 id 并进短信时间线。

接口：`GET /api/mms/list`、`GET /api/mms/status`（监听器状态、当前 APN/MMSC/代理和 PDN 状态）、`POST /api/mms/send`、`GET /api/mms/{id}`、`GET /api/mms/{id}/part/{n}`、`POST /api/mms/{id}/retry`、`POST /api/mms/mark-read`、`DELETE /api/mms/{id}`。

## 通话

### 信令

```
来电：基带振铃 ─▶ AT+CLCC 出现 incoming/waiting
   ├─ push-calls 线程：有设备注册了 VoIP token 时每秒查一次（有 app 在轮询时直接读 call_monitor 的缓存）
   │      └─▶ 给 4 秒内没有轮询过的每台设备发 VoIP 推送 {kind: "incoming_call", number, call_id}
   └─ 前台的 app：GET /api/call/status 长轮询，直接看到来电
iPhone：PushKit ─▶ CallKit 系统来电界面 ─▶ 接听 ─▶ POST /api/call/answer（ATA）
      ─▶ agent 打开语音桥会话 ─▶ POST /api/call/audio/start 拿到会话密钥 ─▶ UDP 收发音频
去电：POST /api/call/dial（ATD<号码>;）─▶ agent 打开语音桥会话；app 用 CallKit 同步显示这通电话
挂断：POST /api/call/hangup（AT+CHUP）；按键音：POST /api/call/dtmf
```

- `call_monitor.rs`：有人在看通话状态时，每 250 ms 查一次 `AT+CLCC` 并带版本号缓存；`GET /api/call/status?wait=<毫秒>&since=<版本>` 在版本变化时立即返回（单次最多等 25 秒）。最后一次请求之后还会继续轮询 20 秒。
- 这台基带的 `AT+CLCC` 永远会列出两条 mode=1 的数据承载，只有 mode=0 的才是语音通话，其余一律忽略。
- 漫游时有些 IMS 来电在 `AT+CLCC` 里没有号码，agent 会从 QMI Voice 服务（服务号 9，`Get All Call Info` 0x002F 的 TLV 0x11）补上。
- VoIP 推送的 `apns-push-type` 为 `voip`，优先级 10，25 秒后过期。正在轮询的设备不再推送，以免同一通来电报两次。
- 多台设备：每台都会响铃，谁接听音频就归谁（app 里的 `call_owned_at`）；其他设备显示“已在别处接听”并停止响铃，只有拥有这通电话的设备结束 CallKit 通话时才会挂断路由器上的电话。
- 防误挂：CallKit 在上报来电后 1.5 秒内自己结束的（模拟器、专注模式、上报失败等），不会挂断路由器上的来电；模拟器里根本不上报 CallKit 来电。
- Apple Watch 拨出的电话，音频由 iPhone 承载。

### 音频

```
上行：iPhone 麦克风 ─▶ 20 ms µ-law 帧 ─U60A─▶ 路由器 :9091 ─▶ 上行节拍器 ─RTP─▶ 192.168.253.1:48894 ─▶ ZTE ODU 工具 ─▶ DSP ─▶ 对方
下行：对方 ─▶ DSP ─▶ ZTE ODU 工具 ─RTP─▶ 192.168.253.100:48894（agent）─U60A─▶ iPhone 抖动缓冲 ─▶ 听筒/扬声器
```

路由器上的 `voice_bridge.rs` 在通话接通后启动固件自带的 ZTE ODU 语音工具，在它和手机之间转发 G.711 µ-law 音频。手机端的音频包格式是 `"U60A" | type | flags | seq | ts | key[16] | payload`，密钥由 `POST /api/call/audio/start` 下发，需要先登录。走隧道时，这些包被包进隧道的独立语音通道。抖动缓冲、节拍器、静音和已知问题见 [call-audio.md](call-audio.md)。

## 推送

`push.rs` 有两条路，公开版本**两条都默认关闭**：

1. **经推送网关**（推荐）：路由器不持有 Apple 的密钥。每个请求用路由器的隧道身份（Ed25519）签名，带上公钥、时间戳和签名，`POST` 给 `openu60-pushgw`；网关验签、检查时间偏差和每台路由器的限额后，用自己的 `.p8` 转发给 Apple，并回 `{"status": <APNs 状态>, "reason": …}`。网关地址来自环境变量 `ZTE_AGENT_PUSH_GATEWAY`（设为 `off` 关闭），或编译时的 `OPENU60_PUSH_GATEWAY`。自建方法见 [remote-access.md](remote-access.md#自建推送网关可选)。
2. **直连 APNs**：路由器自己持有 `.p8`。有本地密钥时优先直连；`ZTE_AGENT_PUSH_VIA=gateway` 可强制走网关。

| 项目 | 说明 |
|---|---|
| 直连配置 | `ZTE_AGENT_APNS_CONFIG` 或 `ZTE_AGENT_LIVE_ACTIVITY_CONFIG` 指向的 JSON（安装脚本 `u60.sh` 带 `APNS_P8` 时写到 `/data/zte-agent-apns.json`）：`team_id`、`key_id`、`bundle_id`、`key_file`、`environment`。配置文件和 `.p8` 都必须是属于 agent 用户的普通文件，权限 0600，不超过 16 KiB |
| 认证 | 用 `.p8`（ES256）签发 provider JWT，复用 50 分钟；HTTP/2 连 `api.push.apple.com`（production）或 `api.sandbox.push.apple.com`（sandbox） |
| topic | 普通提醒用 `bundle_id`，VoIP 用 `bundle_id.voip` |
| 注册 | App 登录后调用 `PUT /api/push/register`，带上 `device_id`、`name`、`alert_token`、`voip_token`、`prefs`（`sms`/`mms`/`calls`/`data`）；保存在 `/data/zte-agent-push.json`，agent 重启后仍有效。`DELETE /api/push/register?device=<id>` 注销单台设备 |
| 环境 | 每台设备先试配置里的环境，遇到 `BadDeviceToken` 之类的错误就换另一个，成功后记住（开发构建走 sandbox，App Store 构建走 production）。两个环境都不认的 token 会被删除 |
| 类别 | `sms`、`mms`、`incoming_call`（VoIP）、`data_alert`（流量提醒）、`test`；另有上游的实时活动信号更新 |
| 调试 | `GET /api/push/status` 查看注册情况和最近一次错误，`POST /api/push/test` 给所有设备发一条测试通知 |

推送要求路由器自己能上网（蜂窝数据或 Wi-Fi 上行都行）。直连时 `.p8` 必须属于 App 所在的开发者团队，`bundle_id` 也要和 App 一致。

## 远程访问

手机 App 按下面的顺序找路由器：

1. 配置的网关（默认 `192.168.0.1`，即连着 U60 热点或 USB 时）；
2. 上次在局域网连上的地址、配对记录里的家庭地址、Wi-Fi 上行模式下路由器在家庭网里的 IP；
3. Bonjour：路由器广播 `_openu60._tcp`，TXT 里带路由器 ID；
4. 隧道：经中继介绍，能打洞就直连，否则由中继转发（需要自建中继，公开版本没有默认中继）。

前三步的候选地址并行探测（每个超时 1.8 秒），按优先级取第一个应答的。界面上显示为“局域网”“远程 · 直连”或“远程 · 中继”。走隧道时每 30 秒检查一次是否回到了局域网，通话期间不换路径。

手机端的隧道库在本机开 `127.0.0.1:<端口>`（HTTP）和一个 UDP 语音端口，app 只是把服务器地址换成本机，其余代码不变。路由器端把已配对手机的流转给 `:9090`，未配对手机的流只能到 `127.0.0.1:9092`（只允许登录和配对），语音转给 `:9091`。设计细节、自建中继和安全性见 [remote-access.md](remote-access.md)。

## 路由器上的数据文件

| 路径 | 内容 |
|---|---|
| `/data/zte-agent` | agent 二进制 |
| `/data/local/tmp/start_zte_agent.sh` | 启动脚本，内含 agent 密码（0700） |
| `/data/local/tmp/zte-agent-run.sh` | 守护进程 |
| `/data/local/tmp/wifi_uplink.sh`、`wifi_uplink_dhcp.sh`、`wifi_mode.conf` | Wi-Fi 上行脚本和保存的模式（含 Wi-Fi 密码，0600） |
| `/data/local/tmp/mms.json` | 可选：覆盖彩信 APN/MMSC/代理 |
| `/data/apns.p8`、`/data/zte-agent-apns.json` | APNs 密钥和配置（0600） |
| `/data/zte-agent-push.json` | 各设备的推送 token 和通知偏好 |
| `/data/openu60/tunnel/` | 隧道身份（`identity.key`）、配置（`config.json`）、已配对设备（`peers.json`） |
| `/data/openu60/mms/` | 彩信索引和附件 |
| `/data/openu60/voice.json` | 通话音频的麦克风模式 |
| `/data/openu60/data_plan.json` | 流量套餐设置 |
| `/data/openu60/screen/` | 触摸屏程序 `openu60-screen`、它的设置和壁纸；放一个 `disabled` 文件可以停用它 |
| `/data/openu60/adblock/` | 广告拦截的设置和统计 |
| `/tmp/zte-agent.log`、`/tmp/zte-agent.prev.log` | agent 日志（在内存里，重启后清空） |
