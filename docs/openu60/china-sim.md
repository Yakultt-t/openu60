# 中国卡漫游指南

这篇讲把中国手机卡放进路由器、卡在国外漫游时会遇到的问题：VoLTE 注册、彩信 APN、DNS、国际短信拦截和 iMessage。实测最多的是中国移动的卡；联通、电信的参数来自公开资料，没有实测。

## 前提

- 卡要开通国际漫游，有些套餐默认不开漫游语音和短信。
- 当地网络和你的运营商之间要有 **VoLTE 漫游**。很多国家已经关闭 2G/3G，没有 VoLTE 漫游就既打不了电话，也可能发不了短信。同一张卡插在手机上能正常打电话，基本说明漫游关系没问题。
- 只靠 Wi-Fi 上行上网时，可以关掉漫游数据省钱。短信和 VoLTE 走单独的 IMS 承载，但关掉“移动数据”会不会影响它们取决于运营商配置，建议自己实测一次。彩信需要漫游数据（见下文）。
- 漫游时的来电、短信、彩信流量都可能收费，请先查清套餐资费。
- 拨号时建议带国家码（例如 `+86 138 0013 8000`、`+61 412 345 678`），最不容易出错。

## VoLTE / IMS 不注册

**现象**：同一张卡在手机上 VoLTE 正常，插进路由器后打不进来、打不出去，发短信也失败，收短信却可能正常。AT 命令看不到 IMS 状态。

**原因**：基带加载这张卡的运营商配置后，IMS settings 服务里的“IMS 注册服务启用”是 0，IMS 根本不会去注册。用 QMI（经 QRTR，不走 AT 口）可以看到并改掉它：

| 服务 | 操作 | 说明 |
|---|---|---|
| IMS settings（服务 18） | Bind `0x0098`（TLV `0x01` = 0） | 同一个客户端里要先 Bind |
| | Get IMS Services Enabled `0x0090` | 响应里 TLV `0x19` = IMS 注册服务是否启用 |
| | Set `0x008F`（TLV `0x18` = 1） | 打开 IMS 服务，十几秒内就能注册上 |
| IMSA（服务 33） | Bind `0x0033`（TLV `0x10` = 0），Get Registration Status `0x0020` | TLV `0x12` 为 2 表示已注册 |

**自动修复**：agent 的 `ims_guard`（`zte-agent/src/ims_guard.rs`）在 agent 启动 45 秒后开始工作（等基带加载完运营商配置），之后每 60 秒检查一次，发现关着就重新打开，并在日志里留下 `[ims] IMS service was off, switched on`。

它还会同步厂商短信守护进程的标志 `zwrt_wms.config.wms_ims_flag`：IMSA 报告已注册就写 `1`，否则写 `0`（经 `ubus call zwrt_wms zwrt_wms_uci_set`）。固件靠这个标志决定短信走 IMS 还是普通网络，而 IMS 晚一点才注册上时，固件不会自己刷新这个标志。漫游的 LTE 网络上少了这个提示，发短信就会失败。

**手动检查**：`06_build_agent.sh` 会顺带编译只读探测工具 `qmi_probe`（`zte-agent/src/bin/qmi_probe.rs`，除非你发的是 Set，否则不改任何东西）。QRTR 的节点和端口每次开机都会变，每次都要先查：

```bash
adb push target/aarch64-unknown-linux-musl/release/qmi_probe /data/local/tmp/
adb shell
qrtr-lookup | awk '$1==18 || $1==33'          # 第 4、5 列是 node、port
/data/local/tmp/qmi_probe <node> <port18> 0098 01:00000000 -- 0090      # 看 TLV 0x19
/data/local/tmp/qmi_probe <node> <port33> 0033 10:00000000 -- 0020      # 看 TLV 0x12（2 = 已注册）
# 手动打开（ims_guard 也会做这件事）：
/data/local/tmp/qmi_probe <node> <port18> 0098 01:00000000 -- 008f 18:01
```

**相关细节**

- 在实测的中国移动配置里，基带数据 profile 2 就是 `ims` APN。彩信因此改用 profile 4：往 profile 2 写彩信 APN 会把 IMS 的 APN 覆盖掉，电话就没了。
- 漫游时有些 IMS 来电在 `AT+CLCC` 里没有号码，agent 会从 QMI Voice 服务（服务 9）补上，所以来电界面能显示号码和通讯录里的名字。
- STD 固件的 `get_sim_info` 返回的 IMSI 和本机号码是加密的，agent 改用 `AT+CIMI` 和 `AT+CNUM` 来取。

## 彩信 APN（实验性）

彩信永远由 **SIM 卡归属运营商**的 MMSC 处理，漫游时也一样，走的是回到国内的彩信 APN。所以 agent 按卡的 MCC/MNC（`zwrt_zte_mdm.api get_sim_info` 的 `mdm_mcc`/`mdm_mnc`）选参数，而不是按当前漫游的网络。内置的参数取自 AOSP 的 `apns-full-conf.xml`（`zte-agent/src/mms/bearer.rs`）：

| 运营商 | MCC/MNC | APN | MMSC | WAP 代理 |
|---|---|---|---|---|
| 中国移动 | 46000、46002、46004、46007、46008、46013（代码中 46015、46060 也用这组） | `cmwap` | `http://mmsc.monternet.com` | `10.0.0.172:80` |
| 中国联通 | 46001、46006、46009 | `3gwap` | `http://mmsc.myuni.com.cn` | `10.0.0.172:80` |
| 中国电信 | 46003、46005、46011 | `ctwap` | `http://mmsc.vnet.mobi` | `10.0.0.200:80` |

PDN 拉在 profile 4（`cid` 4）上，默认路由不受影响。当前选中的参数和 PDN 状态可以这样看：

```bash
api GET /api/mms/status | jq .        # api 函数见 install.md 第 5 节
```

参数不对或运营商不在表里时，用 `/data/local/tmp/mms.json` 覆盖，任何字段都可以只写一部分。表里没有的运营商要写全 `apn`、`mmsc` 和 `proxy`；`cid` 默认就是 4，只有 4 号 profile 被占用时才需要改（不要用 IMS 所在的 profile）：

```json
{"apn": "cmwap", "mmsc": "http://mmsc.monternet.com", "proxy": "10.0.0.172:80", "cid": 4}
```

注意：

- 收发链路已在一家海外运营商的网络上实测打通；**中国三家运营商在漫游状态下还没有端到端验证**，如果收不到，先看 agent 日志里的 `[mms]` 行。
- 彩信流量走漫游数据，可能产生费用；漫游数据被关掉时，彩信 PDN 可能拉不起来。
- 中国的企业类彩信正在退场（运营商下线梦网短彩、银行停发彩信账单），个人点对点彩信目前仍可用；5G 消息（RCS）在路由器上实现不了。

## DNS

中国卡漫游时用的是回国内的 APN，运营商下发的 DNS 有时经这条回国的数据通道根本不回应：路由器自己测不了速、查不到 APNs 的地址，热点下的设备也上不了网。

agent 的 `dns_guard`（`zte-agent/src/dns_guard.rs`）在 agent 启动 20 秒后开始，每 60 秒向 `/etc/resolv.conf` 里的每个运营商 DNS 查询一次 `www.apple.com`。全都不回应、而公共 DNS 能用时，就在运营商 DNS 后面加上 `223.5.5.5` 和 `1.1.1.1`（带 `# openu60 fallback` 标记）；运营商 DNS 恢复后再自动删掉。日志关键字是 `[dns]`。

## 收不到国际短信

**现象**：国内的 106 短信正常，境外号码发来的短信（包括当地号码、境外服务发的验证码）却收不到，路由器的短信库和日志里也什么都没有。

**原因**：为了防范境外诈骗短信，部分省份的运营商把“国际及港澳台短信接收”改成了默认关闭、按需开通。据媒体报道：浙江电信、移动、联通从 2021 年 8 月 10 日起实施；贵州（2021 年 11 月 27 日）、辽宁（2022 年 1 月 25 日）、江西（2022 年 3 月 28 日）、河南（2022 年 5 月 7 日）等地也陆续跟进。其他省份的政策可能不同，以当地运营商为准。

**办法**：打 10086（联通 10010、电信 10000），或在中国移动 App、营业厅申请开通“国际及港澳台短信接收”，办完再测一次。具体的短信指令以运营商的答复为准。

**国际来电也可能被拦**：据报道，浙江移动从 2022 年 5 月起默认关闭了国际及港澳台来电的接听，需要发送 `1219` 到 10086 登记确认。人在国外时，当地号码打你的中国号码就是国际来电，可能受这个限制影响。

参考：[IT之家的报道](https://www.ithome.com/0/589/062.htm)、[凤凰网的报道](https://news.ifeng.com/c/8G2r58q4v9R)。

## iMessage 的坑

对方用 iPhone 给你的中国号码发信息时，如果这个号码在某台 Apple 设备上注册过 iMessage（比如这张卡以前插在 iPhone 里），对方的“信息”app 会显示蓝色气泡。这条消息走的是 Apple 的服务器，**根本不会经过运营商**，路由器当然收不到。FaceTime 也一样。

办法：

- 请对方长按这条消息，选“作为短信发送”（变成绿色气泡）。对方 iPhone 的“设置 → App → 信息 → 作为短信发送”只在 iMessage 发送失败时才会回落成短信，靠不住。
- 这个号码不再用于 iMessage 的话，到 Apple 的[注销 iMessage](https://selfsolve.apple.com/deregister-imessage/) 页面注销它（参见 Apple 支持文章 [HT204270](https://support.apple.com/en-us/HT204270)）。验证码会以短信发到这张卡上，在 OpenU60 里就能看到。
- 用这个号码发信息时，用 OpenU60 发，而不是系统“信息”app（后者用的是 iPhone 自己的卡或 iMessage）。
