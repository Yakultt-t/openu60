# 常见问题

文中的 `api …` 是 [install.md](install.md) 第 5 节里定义的小函数（经 ADB 调用 agent 的 API）。

## 通话

**来电只响两秒左右，就变成忙音或被挂断。**
多半是另一台设备把电话拒接了：每台连着路由器的 App 或网页面板都能发出 `/api/call/hangup`（`AT+CHUP`，在响铃时就是拒接）。测试来电时，先关掉其他装着旧版本 App 的设备和开着电话页面的浏览器。排查方法：看 agent 日志里是谁发的挂断：

```bash
adb shell 'grep -a "\[call\]" /tmp/zte-agent.log | tail -20'     # [call] hangup from <地址> (<User-Agent>)
```

**打不进来、打不出去、短信也发不出去。**
VoLTE（IMS）没有注册。先确认这张卡在手机上能用 VoLTE 漫游，再看 agent 日志里的 `[ims]` 行。`ims_guard` 会自动打开被运营商配置关掉的 IMS 注册，手动检查方法见 [china-sim.md](china-sim.md)。

**App 被关掉后来电不响。**
需要推送：路由器要么配置了推送网关（`ZTE_AGENT_PUSH_GATEWAY`，公开版本没有默认值），要么装了属于 App 开发者团队的 `.p8` 密钥（见 [install.md](install.md#推送可选)），并且 App 至少打开过一次、上报了 VoIP token。用 `api GET /api/push/status | jq .` 检查：看 `config_error`、`voip_registered`、`devices` 和 `last_error`。

**远程通话断断续续，或越聊延迟越大。**
看通话界面计时器下面的诊断行（lost、drop、buf、线路、RTT），再在“随时随地访问”里点“测试通话音频链路”。走中继时，延迟取决于中继离你多远；家里的上游网络如果就是这台 U60 本身，打洞必然失败，只能走中继。可以自建一个近一点的中继（[remote-access.md](remote-access.md)）。调优细节见 [call-audio.md](call-audio.md)。

**对方能听到路由器旁边的声音，或者静音不彻底。**
基带不支持 `AT+CMUT`，app 的静音是改发静音帧；而通话音频通路里包含路由器自己的麦克风。到“设置 → 设备 → 通话音频 → 路由器麦克风”里试试“在调制解调器端静音”或“不打开”（实验性，下一通电话生效）。

**两台设备能同时听同一通电话吗？**
不能。谁拨出或接听，音频就归谁，其他设备显示“已在别处接听”。

**通话界面自己弹出来，或者显示两个“通话中”。**
这台基带的 `AT+CLCC` 永远会列出两条 mode=1 的数据承载，它们不是电话。当前版本已经忽略了这两条；如果还出现，说明 app 或 agent 版本太旧。

## 短信

**国内短信正常，境外发来的短信（当地号码、海外验证码）收不到。**
很可能是你的卡所在省份默认关闭了“国际及港澳台短信接收”（例如浙江从 2021 年 8 月起），打 10086 或在中国移动 App 里开通；国际来电也可能需要登记。详见 [china-sim.md](china-sim.md#收不到国际短信)。

**对方说用 iPhone 发了信息，我这边什么都没有。**
对方那边如果显示蓝色气泡，走的是 iMessage，根本不经过运营商。请对方改用“作为短信发送”，或者到 Apple 的注销页面把这个号码从 iMessage 注销。见 [china-sim.md](china-sim.md#imessage-的坑)。

**角标是怎么算的？怎样才会消失？**
角标就是未读短信/彩信的数量，每次已读状态变化都会重算。在 app 里读完会话、在通知里清除某条短信、或者直接在通知里回复，都会把这个会话里到这条为止的消息标为已读（路由器暂时连不上时先记在本地，连上后再补报），角标随之减少；读过的会话，已送达的通知也会被移除。推送本身不带角标数字。

**用浏览器登录路由器后台之后，app 里的短信读不出来（或者反过来）。**
STD 固件的短信字段用一把全局会话密钥加密，浏览器登录会换掉这把密钥。agent 解密失败时会自动重新协商，下一次刷新就好了；同理，agent 协商之后，浏览器后台里的短信可能也要刷新才能看到。

**彩信收不到或发不出去。**
彩信是实验性功能。先看 `api GET /api/mms/status | jq .`（APN、MMSC、代理是否正确，PDN 是否连上）和日志里的 `[mms]` 行。彩信要走漫游数据；运营商不在内置表里时，用 `/data/local/tmp/mms.json` 覆盖参数，并写上 `cid`。见 [china-sim.md](china-sim.md#彩信-apn实验性)。

## 推送

**怎么确认推送通不通？**

```bash
api GET /api/push/status | jq .
api POST /api/push/test
```

推送要求路由器自己能上网（蜂窝数据或 Wi-Fi 上行都行）。经网关时，网关的地址要能从路由器访问；直连 APNs 时，`.p8` 必须属于 App 所在的开发者团队，`bundle_id` 也要一致，配置文件和 `.p8` 的权限必须是 0600。

**改了启动脚本之后 agent 起不来。**
重新跑一次 `04_deploy_agent.sh`（或在助手里重新安装），它会清理占着 :9090 的残留 `ubus listen` 进程，再重启 agent。

## 连接

**app 找不到路由器，一直显示“未连接”。**
确认 iPhone 连着 U60 的 Wi-Fi（或者和路由器在同一个家庭网里），并且在“设置 → 隐私与安全性 → 本地网络”里给 U60 助手 App 打开了权限，否则 Bonjour 和局域网探测都会失败。要在外面通过隧道连接，路由器得先配置好你自建的中继（公开版本没有默认中继，见 [remote-access.md](remote-access.md)），并且手机至少在局域网里登录过一次（这一次会完成配对）。

**在家里也显示“远程 · 中继”。**
说明局域网探测没成功（权限问题，或者路由器不在当前网络里），只能退回隧道；家里的上游如果就是这台 U60 本身，打洞也会失败，所以走了中继。打开本地网络权限、连上路由器的 Wi-Fi 后，app 30 秒内会切回局域网。

**换了 agent 密码，旧手机还能连？**
能。配对后每台设备都拿到一个长期 token，改密码不会让它失效，要解绑才行（`POST /api/tunnel/unpair`，见 [remote-access.md](remote-access.md#安全与隐私)）。手机丢了就去解绑它。

## 路由器

**`killall zte-agent` 之后它又自己起来了。**
这是守护进程在工作。正确的停止方式见 [router-runtime.md](router-runtime.md)（先 `touch /tmp/zte-agent.stop`）。

**通话之后、或者只用 Wi-Fi 上网时，路由器自己重启了。**
多半是中兴的 `zte_cutoff_protect`：它 ping 不通公网就会重启路由器。`03_disable_cloud.sh` 或助手的安装步骤会停用它。

**开机一直停在 logo。**
开机同步列表里有进程被 init.d 停用了。只要 ADB 还能连上，就能修复，见 [install.md](install.md#6-卸载与还原) 和 [router-runtime.md](router-runtime.md#关闭的厂商服务)。

**重启之后两个频段的热点都开着，不是我设的客户端模式。**
这是有意为之：开机恢复时只重新接上 Wi-Fi 上行，另一个频段的热点保持开着，免得上行出问题时手机再也连不上路由器。在触摸屏、网页面板或 App 里重新选一次客户端模式即可。

**路由器每周自己重启一次，重启后好久连不上家里的 Wi-Fi。**
中兴固件自带每周定时重启，而且按 UTC 时间算，在澳洲、亚洲会落在白天。新版的安装脚本（04 和助手）会把它关掉；旧版本重新安装一次即可。重启后 Wi-Fi 扫描卡住的问题也修好了：单次扫描最多等 20 秒，找不到家里的网络会重试几次，再不行就用上次记住的频段；agent 的守护发现重连跑了 5 分钟还没结束，会杀掉重来。见 [router-runtime.md](router-runtime.md#wi-fi-上行)。

**连按三下电源键，屏幕没有切换到 OpenU60 界面。**
依次检查：开机是否满 1 分钟；三下是否够快（每两下之间不超过 0.6 秒，每下都是短按）；屏幕程序是否装上了（`adb shell ls -l /data/openu60/screen/openu60-screen`，命令行路线要手动推上去，见 [install.md](install.md#04-部署-agent)）；是否存在 `/data/openu60/screen/disabled`；agent 日志里的 `[screen]` 行。屏幕程序 2 分钟内崩溃两次后会暂时留在原厂界面。

**路由器提示有固件更新。**
不要升级。一次 FOTA 升级就可能清掉 agent 和 ADB，而且新固件可能封堵开启 ADB 的方法。OpenU60 安装时已经关闭了自动升级。

**日志在哪里？能直接发出来吗？**
agent 日志在 `/tmp/zte-agent.log`（上一轮在 `/tmp/zte-agent.prev.log`），Wi-Fi 上行日志在 `/tmp/wifi_uplink.log`，屏幕程序的日志在 agent 日志的 `[screen]` 行。日志里可能有电话号码、IP 地址等信息，公开分享前请先打码；备份文件里还有 IMEI、序列号和你的配置，不要公开。

## 其他

**支持 Android 吗？**
目前没有 Android App。安卓手机可以直接用浏览器打开网页管理面板（`http://192.168.0.1:9090`）收发短信、拨打电话。上游项目 [jesther-ai/open-u60-pro](https://github.com/jesther-ai/open-u60-pro) 有一个 Android app，但不包含本项目新增的功能。

**手机 App 在哪里下载？为什么不开源？**
U60 助手 App 不开源，即将在 App Store 上架，上架后会在 README 里给出链接。路由器端的代码（agent、触摸屏程序、安装工具、中继、推送网关）全部在本仓库里。

**海外版能用 eSIM 吗？**
没有内置 eSIM（槽位 2 是空的），只能用可插拔的 eUICC 卡，见 [esim-and-screen.md](esim-and-screen.md)。
