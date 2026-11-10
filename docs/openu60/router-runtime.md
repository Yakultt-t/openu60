# 路由器运行时

这篇讲 agent 在路由器上怎么启动、怎么保活、有哪些后台守护，重启后哪些东西会自己恢复，以及 Wi-Fi 上行和 Wi-Fi 7 的细节。以后改部署或开机行为时，请保持这里描述的约定不变。

## 文件系统

- 根文件系统 `/` 是只读的 ext4；`/etc` 是可写的 overlay（落在 `zteoverlay` 分区），改动重启后保留；`/data` 可写；`/tmp` 在内存里，重启就清空。
- agent 和脚本放在 `/data`，开机钩子写在 `/etc/rc.local`，完整的文件清单见 [architecture.md](architecture.md#路由器上的数据文件)。

## 启动链

```
/etc/rc.local
 ├─ (sleep 15; ubus call zwrt_bsp.usb set '{"mode":"debug"}') &    开机后重新打开 ADB
 ├─ sh /data/local/tmp/start_zte_agent.sh                           导出密码和推送配置
 │    └─ start-stop-daemon -S -b -x /data/local/tmp/zte-agent-run.sh 守护进程（新会话）
 │         └─ /data/zte-agent >> /tmp/zte-agent.log
 └─ (sleep 45; sh /data/local/tmp/wifi_uplink.sh restore) &          恢复 Wi-Fi 上行
```

为什么要用 `start-stop-daemon -b`：adbd 在 adb 会话结束时会杀掉整个进程组，而这台机器的 busybox 没有 `setsid`。`start-stop-daemon` 会在可执行文件“已经在运行”时拒绝启动，所以要用专门的包装脚本，不能直接写 `/bin/sh`。

## 守护进程（`zte-agent-run.sh`）

源码在 `scripts/u60/device/zte-agent-run.sh`。

- agent 不管因为什么退出（崩溃、panic、被 OOM 杀掉），都会在 **3 秒**后重启。启动后不到一分钟就退出、而且连续 5 次时，改成每 60 秒重试一次，免得坏掉的二进制把 CPU 跑满。
- 看门狗：启动 2 分钟后开始，每 60 秒请求一次 agent 的 HTTP 端口，**连续 3 次**（约 3 分钟）完全没有响应就判定卡死，杀掉重启。
- 日志：`/tmp/zte-agent.log` 超过 2 MB 时裁剪到最后 512 KB；每次重启前把上一轮的末尾存到 `/tmp/zte-agent.prev.log`。
- 启动前清理父进程已经是 1 的 `ubus listen`：已退出的 agent 留下的这些子进程会继续占着 :9090，导致新 agent 绑定失败、悄无声息地退出。只清理孤儿进程，不碰厂商自己的监听进程。
- 存在 `/tmp/zte-agent.stop` 时守护进程会退出。

**正确的停止和重启方式**：光执行 `killall zte-agent` 没用，守护进程会用旧的环境变量把它重新拉起来。要先让守护进程退出：

```bash
# 停止
adb shell 'touch /tmp/zte-agent.stop
for d in /proc/[0-9]*; do
  [ "$(tr "\0" "\n" < $d/cmdline 2>/dev/null | sed -n 2p)" = /data/local/tmp/zte-agent-run.sh ] && kill ${d#/proc/}
done
killall zte-agent'
# 启动（会重新读取启动脚本里的环境变量）
adb shell 'rm -f /tmp/zte-agent.stop; sh /data/local/tmp/start_zte_agent.sh'
```

`04_deploy_agent.sh` 和安装脚本 `u60.sh`（U60 刷机助手）就是这么做的，还会等 :9090 释放后再启动。

**环境变量**（写在 `start_zte_agent.sh` 里）：

| 变量 | 默认值 | 作用 |
|---|---|---|
| `ZTE_AGENT_PASSWORD` | 无 | agent 密码（U60 助手密码）；不设则不鉴权，**不要这样用** |
| `ZTE_AGENT_LIVE_ACTIVITY_CONFIG` / `ZTE_AGENT_APNS_CONFIG` | 无 | 直连 APNs 的配置 JSON 路径 |
| `ZTE_AGENT_PUSH_GATEWAY` | 编译时的 `OPENU60_PUSH_GATEWAY`，公开版本为空 | 推送网关地址；`off` 关闭。见 [architecture.md](architecture.md#推送) |
| `ZTE_AGENT_PUSH_VIA` | 无 | 设为 `gateway` 时即使有本地 `.p8` 也走网关 |
| `ZTE_AGENT_BIND` | `0.0.0.0:9090` | HTTP 监听地址 |
| `ZTE_AGENT_THREADS` | 8 | HTTP 工作线程数（长轮询会占住线程） |
| `ZTE_AGENT_BOOTSTRAP_BIND` | `127.0.0.1:9092` | 只给未配对手机用的登录/配对端口 |
| `ZTE_TUNNEL_DIR` | `/data/openu60/tunnel` | 隧道身份和配置目录 |
| `ZTE_VOICE_PORT` / `ZTE_VOICE_VSID` / `ZTE_VOICE_DEVICES` | `9091` / `11C05000` / `2,2147483652` | 通话音频桥，见 [call-audio.md](call-audio.md) |

## 后台守护

| 守护 | 默认 | 节奏 | 做什么 |
|---|---|---|---|
| 守护进程看门狗 | 开 | 60 秒，连续 3 次 | HTTP 无响应就重启 agent（见上文） |
| `ims_guard` | 开 | agent 启动 45 秒后，每 60 秒 | IMS 注册服务被关掉时重新打开，同步 `wms_ims_flag`，见 [china-sim.md](china-sim.md) |
| `dns_guard` | 开 | agent 启动 20 秒后，每 60 秒 | 运营商 DNS 都不回应时临时加入公共 DNS，恢复后删掉 |
| `wifi-uplink-guard` | 开 | agent 启动 150 秒后，每 30 秒 | 处于客户端/中继模式、上行却断了，连续两次（约 1 分钟）就在单独的线程里执行 `wifi_uplink.sh restore`。如果某次连接/恢复（`restore`、`connect`、`mode`）连续 10 次检查（约 5 分钟）都还在跑，就当它卡死了，连同正在跑的 `iw … scan` 一起杀掉，下一轮再重新恢复 |
| 屏幕守护（`screen_watch.rs`） | 装了屏幕程序才生效 | agent 启动 30 秒后；没装时每 60 秒看一次 | 保持 `openu60-screen watch` 在后台运行，退出后自动重启，见 [esim-and-screen.md](esim-and-screen.md#触摸屏程序) |
| Wi-Fi 脚本清理 | 开 | agent 启动 150 秒后每 5 分钟；每次从休眠唤醒后 150 秒再查一次 | 杀掉卡死的 `/sbin/zte_post_wifi.sh`（固件缺陷，上游功能） |
| `conn_watchdog` | **关** | — | “连着但上不了网”时逐级处理：重连 PDN → LPM 再上线 → 重置基带 →（可选）重启。上游功能，还没有逐级验证 |
| `oos_policy` | **关** | — | 没有服务时把基带停在 LPM，按 20/40/60 分钟的间隔探测。**拿路由器当电话用时务必保持关闭**：停在 LPM 期间收不到电话和短信 |

`conn_watchdog` 和 `oos_policy` 的配置文件分别是 `/data/local/tmp/conn_watchdog.json` 和 `/data/local/tmp/oos_policy.json`（接口为 `/api/modem/conn-watchdog`、`/api/modem/oos-policy`）。没有配置文件就是关闭状态。两者在通话、USSD、测速期间都会推迟动作（`busy_check` 会用 `AT+CLCC` 再确认一次是否在通话）。

## 关闭的厂商服务

`03_disable_cloud.sh`（或助手的安装步骤）会停用 `zte_dm`、`zte_topsw_fota_result`（固件升级）、`zte_smart_manage`、`zte_topsw_get_brand`、`scp_agent`（中兴云的长连接和远程管理），关闭 TR-069 周期上报，并停用 `zte_cutoff_protect`。`04_deploy_agent.sh` 和助手的安装步骤还会关闭中兴固件自带的**每周定时重启**。

- **每周定时重启**（`zte_topsw_mc`，配置 `zwrt_zte_mc.reboot_schedule`）：出厂默认开启，每周二 02:00 后的随机 2 小时内重启一次。路由器的时区是 UTC，所以在澳洲、亚洲会落在白天，正好打断正在用的上行和远程连接。安装时会执行 `uci set zwrt_zte_mc.reboot_schedule.reboot_schedule_enable=0; uci commit zwrt_zte_mc`，`zte_topsw_mc` 下次检查时就会看到它已关闭。想恢复就把它改回 `1`。agent 的守护进程和各类守护已经覆盖了它的用途。
- `zte_cutoff_protect` 会 ping 公网，重新注册网络后检查失败就**重启路由器**。通话时数据短暂中断、或者只走 Wi-Fi 上行、关掉蜂窝数据时都会触发。它不在开机同步列表里，可以放心停用。
- 开机同步列表：主守护进程 `zte_topsw_daemon` 读取 `/etc/config/zte_topsw_daemon.conf`，要等列表里每个进程都注册上，才会放行开机流程。列表里的进程不能用 init.d 停用，否则会卡在 logo、不拨号、触摸无响应，必须在这个文件里把对应行注释掉（`04_deploy_agent.sh` 会注释掉 `zte_dm` 和 `zte_topsw_fota_result`）。检查方法：

```bash
adb shell "ubus call zwrt_topsw_daemon.sync get_sync_info '{}'"     # 应返回 "sync success"
adb shell "ls -la /zteoverlay/etc-upper_a/rc.d/ | grep '^c'"         # init.d disable 留下的 whiteout
```

## 重启后的恢复

实测重启后，下面这些都会自己恢复：ADB、agent 及其守护进程、IMS（由 `ims_guard` 打开）、Wi-Fi 上行（`rc.local` 在 45 秒时恢复，之后由 `wifi-uplink-guard` 兜底，卡住超过约 5 分钟会被杀掉重来）、中继注册、触摸屏程序的后台监听。

- Wi-Fi 子系统在开机后可能要两分钟左右才完全就绪。
- QRTR 服务的端口每次开机都会变（同一台机器上 WMS 就出现过 92 和 87），代码和手工命令都必须先用 `qrtr-lookup` 查，不能写死。
- phy 编号也不固定（干净开机是 `phy0`，驱动重载后可能是 `phy1`），Wi-Fi 脚本每次都从正在运行的 AP 接口去解析。

## Wi-Fi 上行

STD 固件里 `zwrt_wlan` 的 `sta_*` 方法还在，但它们要调用的 `/sbin/wifi_sta_opt.sh` 已经被删掉，扫描结果文件也从来不会生成，固件自带的中继功能是坏的。所以 OpenU60 自己搭了一套 station：`scripts/u60/device/wifi_uplink.sh` 加上 `wifi_uplink_dhcp.sh`，由 agent 的 `wifi_wan.rs` 调用（`/api/wifi/wan/*`）。可以在触摸屏的“上网方式”页面、网页管理面板的 Wi-Fi 页面或手机 App 里操作。

| 模式 | 行为 |
|---|---|
| 热点（默认） | 走 SIM 卡的数据，2.4G 和 5G 热点都开着 |
| 客户端 | 路由器接入家里的 Wi-Fi，另一个频段的热点 20 秒后关闭（留时间给 app 读到路由器在家庭网里的地址），手机通过家庭网访问路由器 |
| 中继 | 一个频段接上游，另一个频段继续开热点 |

实现要点：

- 驱动只允许 station 和 AP 处在**不同频段**（同频段会报 `sap-sta conc will fail`）。所以脚本先关掉上行所在频段的热点，再在 `wlan4` 上跨频段连接；用 `hostapd_cli enable/disable` 按频段开关热点，不做整体重载。
- 固件的 `ubus call zwrt_wlan reload` 会把 station 接口拆掉；而且它只重启配置有变化的频段，被留在 DISABLED 状态的 BSS 不会自己恢复。热点起不来时，脚本会强制把两个频段先关再开（十几秒），然后重新连接上行。
- 连接用 `wpa_supplicant`（nl80211，WPA2-PSK/WPA3-SAE，PMF 设为可选），DHCP 用 `udhcpc`：上行默认路由的 metric 是 10，优先于蜂窝；上行的 DNS 放在 `resolv.conf.auto` 最前面，关掉蜂窝数据后热点下的设备也能解析域名。
- 防火墙：在 `wlan4` 上做 MASQUERADE、放行转发，入站只放行 DHCP、9090/tcp（agent，家庭网里的设备凭密码访问）和 9091/udp（通话音频）；另外 agent 自己会放行 mDNS（udp 5353），Bonjour 才能在家庭网里找到路由器。这台内核的 iptables 没有 `state` 匹配，要用 `-m conntrack --ctstate`。
- 保存的模式在 `/data/local/tmp/wifi_mode.conf`（0600），**其中 Wi-Fi 密码是明文**，只有 root 能读；app 端每个 SSID 的密码存在 iCloud 钥匙串。
- **扫描超时**：每次 `iw scan` 最多等 `SCAN_TIMEOUT` 秒（默认 20），超时就杀掉它、按“没扫到”处理。开机后 5 GHz 热点要做约一分钟的 DFS 雷达检测，这期间扫描可能失败，下一次扫描甚至可能永远不返回。2026-11-10 就出过一次：中兴的定时重启之后，开机恢复卡在扫描上 73 分钟，路由器一直没网。这台 busybox 没有 `timeout` 和 `pkill`，所以脚本用“完成标记文件 + 轮询”实现超时，用 `ps` + `awk`（`kill_matching`）杀进程。
- **开机恢复会重试**：`restore` 最多扫描 4 次、每次间隔 15 秒来找家里的网络；还是找不到，就用 `wifi_mode.conf` 里记住的频段（`band=` 行，每次连接成功时更新）。决定先关哪个频段的热点要靠这个频段，猜错了连接会失败。
- 开机恢复时有一条“永不锁死”规则：即使原来是客户端模式，也只恢复上行，另一个频段的热点保持开着。这样上行或 app 出了问题，手机仍然能连上路由器；app 连上后可以再切回客户端模式。
- 日志：`/tmp/wifi_uplink.log`。

连接上游建议在触摸屏、网页面板或 App 里操作：密码经 API 交给脚本，不会出现在任何命令行里。排查时也可以手动调用（连接时密码通过环境变量 `WIFI_PSK` 传入，注意别留在 shell 历史里）：

```bash
adb shell 'sh /data/local/tmp/wifi_uplink.sh scan'
adb shell 'sh /data/local/tmp/wifi_uplink.sh status'
adb shell 'sh /data/local/tmp/wifi_uplink.sh mode hotspot'
# WIFI_PSK=… sh /data/local/tmp/wifi_uplink.sh mode client|repeater <SSID> [BSSID]
```

## Wi-Fi 7

- 这台固件上的 Wi-Fi 7 就是两个射频的 PHY 模式：`wireless.wifi0.hwmode=11beg`、`wireless.wifi1.hwmode=11bea`，配合 EHT 频宽。厂商的 `wifi6_switch` 标志，这版固件的 Wi-Fi 脚本根本不读。
- agent 在 `GET /api/wifi/status` 里报告 `wifi7_enabled`（两个射频都是 11be 才算），`PUT /api/wifi/settings` 接受 `"wifi7": true|false`：同时切换两个射频的 802.11be/802.11ax，并把频宽换到对应的写法（如 EHT160 ↔ HE160，否则 hostapd 不认）。
- 开关 Wi-Fi 7 会重启全部 Wi-Fi，较旧的手机和电脑可能连不上（app 里有提示）。处在客户端/中继模式时，`wifi-uplink-guard` 会在重载后把上行重新接上。
- MLO（多链路）需要两个频段的热点都开着，所以客户端模式下用不了（经验结论，未在代码里处理）。

## 日志

- agent：`/tmp/zte-agent.log`（上一轮在 `/tmp/zte-agent.prev.log`），每行带模块前缀，常用的有 `[call]`、`[voice]`、`[push]`、`[mms]`、`[ims]`、`[dns]`、`[wifi-wan]`、`[sms]`、`[screen]`、`[adblock]`、`[bootstrap]`、`[supervisor]`。
- 都在内存里，重启后清空。日志里可能有电话号码，分享前请先打码。

## 不要做的事

- 不要用 init.d 停用开机同步列表里的进程。
- 不要给音频或隧道线程设 SCHED_FIFO。
- 拿路由器当电话用时，不要打开 `oos_policy`。
- 不要写死 QRTR 的节点或端口。
- 不要只执行 `killall zte-agent` 就以为 agent 已经停了。
- 不要重新打开固件升级：一次 FOTA 升级就可能清掉 agent 和 ADB。
- 不要用 `init.d disable` 停用原厂屏幕程序 `zte_topsw_devui`：它在开机同步列表里，只能运行时临时 stop/start。
