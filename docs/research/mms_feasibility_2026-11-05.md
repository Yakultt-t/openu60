# U60 Pro 彩信（MMS）可行性研究

日期：2026-11-05。本文基于当天通过 adb 对路由器的只读探测、代码库现状和公开资料；没有改动路由器任何配置，也没有发送过任何彩信。

## 一句话结论

**技术上可行，但要在路由器 agent 里自己实现整条 MMS 链路；ZTE 固件完全没有彩信功能，只会把彩信通知丢成一条占位短信。** 三个硬骨头按风险排序：① 拿到彩信通知里的下载地址（固件把它扔了）；② 为彩信单独拉起第二条 APN 数据通道（固件有这个槽位，没验证过）；③ 中国运营商彩信生态正在退场（个人点对点彩信还在，但银行等机构已因"运营商逐步下线彩信"停发彩信）。建议先做两个几乎零成本的实验再决定投入（见文末）。

## 路由器现状（2026-11-05 探测）

| 项目 | 结果 |
|---|---|
| 系统 | OpenWrt 23.05.4，Linux 5.15.185-perf，aarch64，高通基带（IPA + rmnet） |
| 数据口 | `rmnet_data0`（当前 10.146.x.x，CGNAT），另有 `rmnet_data1`–`rmnet_data5` 空闲网卡 |
| PDN 槽位 | ubus `zwrt_data get_wwaniface` 有 cid 1/2/3；cid 1 已连接，cid 2/3 `connect_mode=2`、`disconnected`；`network.interface.zte_wan_2/3/4` 已定义但无设备 |
| APN 管理 | ubus `zwrt_apn_object`：`set_apn_at_cid`/`get_apn_at_cid`/`getCidApnList` 等，支持按 cid 配不同 APN |
| APN 数据库 | `/etc/auto_apn.db`（SQLite）只有 APN/认证/PDP 字段，**没有 MMSC、代理、端口字段**；中国三家只收录了 cmnet/3gnet/ctnet 上网 APN |
| 短信守护进程 | `zte_topsw_wms`（高通 WMS 库 + ZTE 封装），通过 QMI WMS "transfer route" 收短信，写入 `/etc_rw/ztembb/ztesms/sms_db/sms.db` |
| 彩信通知处理 | 二进制里有日志串 `THis is a MMS`，紧跟 UCS-2 十六进制 `multimedia_information`：**识别到 WAP Push 彩信通知后，正文被替换成占位文本 "multimedia_information" 存库**，通知里的下载 URL（Content-Location）、事务 ID、大小、过期时间全部丢失。目前库里 13 条短信没有一条是这种占位（还没收过彩信） |
| QMI 服务 | `qrtr-lookup` 可见节点 3（基带）上的 WMS(5)、WDS(1)、NAS(3)、UIM(11)、Voice(9) 等 |
| 工具 | 无 qmicli/uqmi/mmcli；有 `libqmi-glib.so.5`(1.32.4)、`libqrtr-glib`、`qrtr-lookup`、`curl`、`sqlite3`；无 python |
| Web UI | luci 版 zte_web，无任何 mms 字样 |
| 日志 | `logread` 里没有 wms 输出，守护进程只有 `sms_redirect_debug_num` 一个调试开关，拿不到原始 PDU |

## 彩信是怎么工作的（MM1 接口）

彩信在手机侧只有一个接口 MM1，全部是"短信 + HTTP"：

1. **接收**：彩信中心（MMSC）先发一条 **WAP Push 短信**（8-bit 数据短信，UDH 里目的端口 2948），内容是二进制编码的 `m-notification.ind`，里面有 `X-Mms-Content-Location`（一个 http URL）、事务 ID、发件人、大小、过期时间。手机再对这个 URL 发 **HTTP GET**（通常必须经过运营商 WAP 网关代理，如 10.0.0.172:80），拿回 `m-retrieve.conf`（二进制头 + multipart 正文：SMIL、图片、音频），然后 POST 一个 `m-notifyresp.ind` 告诉 MMSC 已收到。
2. **发送**：把图片/文字打包成 `m-send.req`（WSP 二进制头 + multipart），**HTTP POST** 到 MMSC 地址（如 `http://mmsc.monternet.com`），MMSC 回 `m-send.conf` 给出消息 ID。
3. **数据通道**：以上 HTTP 必须走运营商的 **彩信 APN**（中国三家都是独立 APN：cmwap / 3gwap / ctwap），走普通上网 APN 一般到不了 MMSC/代理。所以手机都是同时挂两条 PDN。

规范：3GPP TS 23.140 + OMA MMS Encapsulation（OMA-WAP-MMS-ENC）。Linux 手机上的开源实现 mmsd-tng（Phosh/Chatty 用）就是这套流程，可作协议参考（GPL，不能抄代码，只看流程）。

### 中国运营商参数（来自 AOSP apns-full-conf.xml，2026-11-05 版）

| 运营商 | 上网 APN | 彩信 APN | MMSC | 代理 | 协议 |
|---|---|---|---|---|---|
| 中国移动 (460 00/02/04/07/08/13) | cmnet | cmwap | http://mmsc.monternet.com | 10.0.0.172:80 | IPv4v6 |
| 中国联通 (460 01/09) | 3gnet | 3gwap | http://mmsc.myuni.com.cn | 10.0.0.172:80 | IPv4v6 |
| 中国电信 (460 03/11) | ctnet | ctwap | http://mmsc.vnet.mobi | 10.0.0.200:80 | IPv4v6 |
| 中国广电 (460 15/60) | cbnet / cmnet | cmwap | http://mmsc.monternet.com | 10.0.0.172:80 | IPv4v6 |

彩信 APN 都是单独一条 `type="mms"` 记录，和上网 APN 分开，说明在 Android 上也是双 PDN。

## 三个难点与方案

### 难点 1：拿到彩信通知（下载地址）

固件丢掉了通知正文，只留下 "multimedia_information" 占位。三条路线：

- **A. 第二个 QMI WMS 客户端（推荐先试）**。agent 直接开 `AF_QIPCRTR` socket 连基带的 WMS 服务（qrtr-lookup 已看到服务 5 在节点 3 端口 92），分配客户端 ID 后发 `WMS Set Event Report`（上报 MT 短信）。高通基带会把新到短信的原始 PDU 以 indication 推给每一个打开了上报的客户端；agent 只读不 ack（ack 仍由 ZTE 守护进程做），从 PDU 里解析 UDH 端口 2948 的 WAP Push，得到 `m-notification.ind`。代码库已有 QRTR 经验（lpac 可行性报告），纯 Rust 手写 QMI 编码不依赖 libqmi（crates.io 上只有 `libqmi-glib-sys`/`qrtr-sys` FFI 绑定，没有纯 Rust QMI 实现）。**风险**：是否所有客户端都能收到 MT indication、以及会不会干扰 ZTE 守护进程的路由设置，必须实测；可以随时 kill 原型回滚，不改任何持久配置。
- **B. 把占位短信当"有彩信到达"的信号**。零风险、立刻可用：收到正文为 "multimedia_information" 的短信就推送 "收到一条来自 X 的彩信，当前无法下载"。只能告知，不能收内容。可作为第一版兜底。
- **C. AT 口 `AT+CNMI`/`+CMT`**。`/dev/at_mdm2` 是 agent 现有 AT 通道，但这条口的短信路由是否独立于 QMI、是否会把短信从 ZTE 守护进程那边抢走，不确定，排在 A 之后。

### 难点 2：第二条 PDN（彩信 APN）

固件已经预留了 cid 2/3 和 `rmnet_data1+` 网卡，看起来是为"多 APN"设计的（ZTE 其它市场固件用它做 IMS/企业 APN）。做法：

1. `ubus call zwrt_apn_object set_apn_at_cid '{"cid":2,"wanapn":"cmwap","pdpType":3,...}'`
2. `ubus call zwrt_data set_wwaniface '{"cid":2,"enable":1,"connect_mode":...}'`
3. 看 `get_wwaniface cid=2` 是否变成 `ipv4_connected` 并给出 `ipv4_dev_name`（预期 `rmnet_data1`）和地址。

成功后不要碰默认路由：agent 的 HTTP 客户端用 `SO_BINDTODEVICE` 绑到 `rmnet_data1`，再通过代理 10.0.0.172:80 访问 MMSC；或者只加一条 `/32` 到代理的静态路由。路由器的 `pbr`/`mwan3` 都在，但没必要用。未验证点：`set_wwaniface` 的 `connect_mode` 取值含义、第二 PDN 在 5G NSA 下能否起来、漫游时运营商是否允许 cmwap。**这是整个方案成败的关键实验，但要等中国 SIM 插上才有意义**（澳洲 OnePass 卡也可以先验证"固件能否拉起第二 PDN"这一点）。

### 难点 3：编解码与发送

- 没有现成 Rust 库，需要在 agent 里手写 OMA MMS 编码器/解码器子集：`m-notification.ind` 解码、`m-retrieve.conf` 解码（WSP 头 + multipart/related + SMIL）、`m-send.req` 编码、`m-notifyresp.ind`/`m-acknowledge.ind` 编码。参考 Android 的 PduParser/PduComposer（Apache 2.0，可以参考实现）。估计 600–900 行。
- 发送：`curl`-级别的 HTTP POST，`Content-Type: application/vnd.wap.mms-message`，经代理。
- 存储：彩信附件存在 `/etc_rw` 下，agent 提供 `/api/mms/list`、`/api/mms/{id}/part/{n}`、`/api/mms/send`（multipart 上传）；推送走现有 APNs 通道。
- iOS 端：会话里渲染图片/音频气泡，发送侧已经有 "+" 菜单和附件条，只差把附件交给 `/api/mms/send`。

### 工作量估算

| 部分 | 估计 |
|---|---|
| QMI WMS 监听原型（难点 1-A） | 300–400 行 Rust，1 个实验日 |
| 第二 PDN 拉起 + 路由（难点 2） | 主要是实验；代码 100–200 行 |
| MMS PDU 编解码 | 600–900 行 Rust |
| 下载/发送/存储/API/推送 | 400–600 行 Rust |
| iOS 收发与渲染 | 500–800 行 Swift |

## 运营商现状与风险（2026-11）

- 中国移动 2026-03 公告：梦网全网短彩等 5 款业务 2026-04-30 下线；2025-08-31 起部分 SP 梦网短彩停止。这是企业/SP 彩信，不是个人点对点彩信。
- 建设银行 2026-10 公告：因"各运营商已逐步下线彩信服务"，2026-11-30 起停发信用卡彩信账单。
- 没有找到三家运营商停止个人彩信的公告，但趋势明确：**彩信在中国正在退场，被 5G 消息（RCS）取代**，而 RCS 在路由器上实现不了。换句话说，这个功能的"保质期"可能只有一两年，而且验证码、账单等机构类消息已经不再走彩信。
- 澳洲（现在插的 OnePass 卡）彩信仍是主流业务，可以用来做协议验证，但 OnePass 的 MMSC/APN 参数需另查。

## 建议的下一步（按成本从低到高）

1. **零成本实验：用 iPhone 给路由器号码发一张图片彩信（澳洲卡现在就能做）**，然后看 `sms.db` 是否多出 "multimedia_information" 占位行、发件人号码是否保留。确认后先把路线 B（"收到彩信"提醒）做进 app，半天。
2. **QMI WMS 监听原型**（难点 1-A）：agent 加一个只读的 WMS 客户端，再发一条彩信，看能否拿到原始 WAP Push PDU。拿到了，接收路线就通了；拿不到，再考虑 AT 口。
3. **第二 PDN 实验**（难点 2）：先用澳洲卡试 `set_apn_at_cid` + `set_wwaniface cid=2` 能否起来 `rmnet_data1`；中国 SIM 插上后用 cmwap/3gwap/ctwap 复测，并用 `curl --interface rmnet_data1 -x 10.0.0.172:80` 访问 MMSC。
4. 以上三步都通过，再投入编解码和 app 端开发（约 1–2 周）。
5. 如果第 2 或第 3 步失败，退回路线 B，并把"彩信"明确标为不支持，附件发送改走其它方式（例如 iMessage/邮件自己转发）。

## 资料来源

- mmsd-tng 项目与 wiki：https://gitlab.com/kop316/mmsd ，https://gitlab.com/kop316/mmsd/-/wikis/home
- OMA MMS 封装规范：http://openmobilealliance.org/release/MMS/V1_1-20021104-C/OMA-WAP-MMS-ENC-V1_1-20021030-C.pdf ；3GPP TS 23.140
- NowSMS MM1 协议讲解：https://www.nowsms.com/presentations/mm1-phone-to-mmsc-protocol/MM1%20Phone%20To%20MMSC%20Protocol.pdf ；WAP Push 端口 2948：https://nowsms.com/delivering-mms-notifications-with-wap-push-over-ip
- oFono WAP Push 插件（端口 2948 处理）：https://elixir.bootlin.com/ofono/1.10/source/plugins/push-notification.c
- AOSP 运营商 APN 表：https://android.googlesource.com/device/sample/+/master/etc/apns-full-conf.xml
- Rust FFI 绑定：https://docs.rs/crate/libqmi-glib-sys ；qrtr-sys（crates.io）
- 中国移动梦网短彩下线：https://www.ithome.com/0/930/412.htm ，https://www.ithome.com/0/946/012.htm
- 建行停发彩信账单（引用运营商逐步下线彩信）：https://www.yicai.com/news/103401827.html
- 本仓库既有报告：docs/research/U60_Pro_QRTR_lpac_Feasibility.md（QRTR/QMI 接入经验）

## 实验结果（2026-11-05 晚，澳洲 OnePass mobile 卡，Optus 网络 50502）

1. **接收占位确认**：iPhone 发一张图片彩信到路由器号码后，`sms.db` 新增一行：正文正是占位 "multimedia_information"（UCS-2 十六进制），发件人字段是 "99"（WAP Push 短信的 TP-OA，不是真正的发件人），没有保留任何原始 PDU。路线 B（"收到一条彩信"提醒）可以立刻做；真正的发件人和下载地址必须走 QMI WMS 监听（路线 A）。
2. **第二条 PDN 成功**（固件原生能力，无需改固件）：
   ```
   ubus call zwrt_apn_object set_apn_at_cid '{"cid":2,"profilename":"Optus MMS","wanapn":"mms","username":"","password":"","pdpType":1,"pppAuthMode":0,"profileId":"manu_mms2","isEnable":true,"isValid":1,"extraInt1":0,"roamingPdpType":1}'
   ubus call zwrt_data set_wwaniface '{"source_module":"zte-agent","cid":2,"enable":1,"connect_mode":1}'
   ```
   约 10 秒后 `get_wwaniface cid=2` 变为 `ipv4_connected`，网卡 `rmnet_data1`，地址 10.1.40.8/28，默认路由不受影响，主连接（cid 1）照常。关闭用 `set_wwaniface '{"cid":2,"enable":0}'`。
3. **MMSC 可达性**：主 APN 上 MMSC/代理全部超时；`rmnet_data1` 上直连 MMSC 超时，但经代理 61.88.190.10:8070 可达（GET 返回 HTTP 400，属正常）。结论：必须双 PDN + 代理。
4. **发送成功**：用 240 字节手工编码的 `m-send.req`（文字 + 48×48 PNG，`application/vnd.wap.multipart.mixed`），`curl --interface rmnet_data1 -x http://61.88.190.10:8070 -X POST -H 'Content-Type: application/vnd.wap.mms-message'` 到 `http://mmsc.optus.com.au:8002/`，0.44 秒返回 HTTP 200，`m-send.conf`：Response-Status 0x80（OK），Response-Text "1000:OK"，带 Message-ID。编码器在工作区 `mms/build_send_req.py`（待并入 agent）。

**结论更新**：难点 2（第二 PDN）和难点 3 的发送半边已经打通，剩下的硬骨头只有接收通知（难点 1-A 的 QMI WMS 监听原型）。
5. **端到端确认**：第一次误发到了路由器自己的号码（路由器随即收到一条占位通知，印证投递正常）；第二次发到 iPhone 号码，Response-Text 1000:OK，**iPhone 收到了图片和文字**。发送链路（双 PDN + 代理 + 手工 m-send.req）端到端打通。

## 接收实验（2026-11-05 23:14，同一张澳洲卡）

原型 `zte-agent/src/bin/wms_listen.rs`（只读 QMI WMS 客户端，AF_QIPCRTR 直连基带 WMS 服务，节点 3 端口 92）：

1. `WMS Set Event Report`（新消息上报）对第二个客户端返回 ok；`Get Routes` 显示厂商守护进程的路由是全部消息类 → 存储 NV（storage 1）、receipt_action 1（存储并通知），不是 transfer-only。
2. 路由器给自己发一条彩信后 3.5 秒，监听器收到两条 `MT message stored: storage=1 index=0` 指示（WAP Push 分成两段级联短信），**每条都在厂商守护进程删除之前用 `Raw Read` 读回了完整 PDU**（日志：docs/research/mms_wms_listen_2026-11-05.log）。
3. 解码（scripts/mms/decode_push.py）：SMS-DELIVER 来自 "99"，DCS 0x04（8-bit），UDH 端口 2948←9200 + 级联（ref 0x7cba，2 段）；WSP Push → `m-notification.ind`：Transaction-ID、From 61412345678/TYPE=PLMN、Message-Size 1179、Expiry 48 小时、**Content-Location http://vmmsc1.optus.com.au:8002/?<message-id>**。
4. 用 `curl --interface rmnet_data1 -x 61.88.190.10:8070` GET 该地址，0.57 秒拿到 325 字节 `m-retrieve.conf`：From/To/Message-ID/Date/Subject 完整，multipart.mixed 两个部分：text/plain(utf-8) 46 字节 + image/png 114 字节，**PNG 校验完好（48×48）**。
5. `m-notifyresp.ind`（Status Retrieved）POST 到 mmsc.optus.com.au 和 vmmsc1 都返回 HTTP 400，待查（不影响收发；可能需要 1.3 版本号或 From 字段）。

**结论**：发送与接收两条链路都已实测打通，可以进入正式实现：agent 内嵌 WMS 监听 + 级联重组 + WAP Push/MMS 编解码 + 按 PLMN 选择彩信 APN 拉起 cid 2 + 经代理收发 + 存储与 API + APNs 推送；app 端渲染图片/音频并走 `/api/mms/send`。

## 实现（2026-11-06 凌晨）

agent（`zte-agent/src/mms/`）：
- `qmi.rs`：常驻只读 QMI WMS 客户端（AF_QIPCRTR），新消息指示 → `Raw Read` 读回 PDU，带 60 秒心跳和自动重连。
- `wap.rs`：SMS-DELIVER 解析、级联重组、WSP Push 拆包；`pdu.rs`：OMA MMS 编解码子集（m-notification.ind / m-retrieve.conf / m-send.req / m-send.conf / m-notifyresp.ind，含单元测试）。
- `bearer.rs`：按 PLMN 选彩信 APN/MMSC/代理（中国移动/联通/电信、Optus/Telstra/Vodafone AU），`/data/local/tmp/mms.json` 可覆盖；用 `set_apn_at_cid` + `set_wwaniface` 拉起 cid 2 并等待 `rmnet_data1`。
- `http.rs`：通过固件自带 curl，`--interface` 绑定彩信网卡、经代理收发。
- `store.rs`：`/data/openu60/mms/index.json` + 每个附件一个文件（最多 500 条）。
- `mod.rs`：收到通知 → 下载 → 存储 → `m-notifyresp.ind`（已修正消息类型 0x83，MMSC 返回 200）→ 删除厂商占位短信 → APNs 推送（`kind: mms`）；发送 → 编码 → POST → 记录状态。时间戳扣除 NITZ 偏移（固件把本地时间当 UTC 写进系统时钟）。
- API：`GET /api/mms/list`、`GET /api/mms/status`、`POST /api/mms/send`（JSON，附件 base64）、`GET /api/mms/{id}/part/{n}`、`POST /api/mms/{id}/retry`、`POST /api/mms/mark-read`、`DELETE /api/mms/{id}`。

iOS：MMS 以负 id 合并进短信时间线（`MMSModels.swift`），会话里图片缩略图/全屏查看/共享、语音条播放、失败重试；“+”菜单的照片/相机/音频附件经 `/api/mms/send` 发送（图片缩到 1280px、JPEG、总量 ≤ 900 KB）。

实测：agent 自发自收（通知 3.5 秒内截获、下载、占位清理）、模拟器里从相册选图发送均成功。
