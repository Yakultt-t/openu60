# eSIM 与屏幕

## 海外版没有内置 eSIM

在一台海外版机器（硬件 `MU5250_HW1.0`，固件 `STD_PL_MU5250V1.0.0B02`）上，通过 QMI UIM 服务查到的情况：

- 有两个物理槽位。槽位 1 是卡托，插的是普通 UICC（不是 eUICC，没有 EID）。
- 槽位 2 处于激活状态，但没有卡，`Get Card Status` 报告的是错误状态 “no ATR received”：基带开机时会给槽位 2 上电、复位，可是没有任何东西应答。也就是说，这条线上**没有焊芯片**。这不是软件锁，改软件或改固件都没用；切到槽位 2 的唯一后果，是把实体卡也断掉。
- 这台机器上没有国行双卡机型才有的 `support_dual_sim` 配置项。厂商 ubus 接口 `zwrt_zte_mdm.api` 里的 `sim_switch_slot`、`sim_get_slot`、`zte_get_current_slot_info` 都还在，但调用后什么也不返回。

**自己检查的方法**：用只读的 `qmi_probe`（见 [china-sim.md](china-sim.md) 的“手动检查”）向 UIM 服务（服务 11）发 `Get Slot Status`（`0x0047`）和 `Get Card Status`（`0x002F`）：

```bash
adb shell
qrtr-lookup | awk '$1==11'                                  # UIM 的 node、port，每次开机都会变
HEXMAX=4096 /data/local/tmp/qmi_probe <node> <port> 0047     # 槽位状态
HEXMAX=4096 /data/local/tmp/qmi_probe <node> <port> 002f     # 卡状态
```

输出里有 ICCID、EID 之类的标识，**不要把原始输出贴到公开的 issue 里**，只描述结构即可。

**国行机型**（社区报告，未验证）：部分国行机型（例如“全球版”）在槽位 2 焊有一颗内置卡，但它是只能在大陆使用的贴片 SIM，不是能写入自定义 profile 的 eUICC；普通版没有这颗芯片。国行机型的拆机图里能看到预留的 eSIM 焊盘，也有人把内置卡换焊成了真正的 eUICC。目前还没见到哪台海外版机器装着这颗芯片、或者槽位 2 能用。国行和海外版之间互刷固件需要中兴签名的 Firehose loader，据公开资料还没有人成功过。

## 能走通的路：可插拔 eUICC 卡 + lpac

想在这台路由器上用 eSIM，现实的办法是买一张**可插拔的 eUICC 卡**（市面上有 9eSIM、5ber、eSTK.me 等），插进卡托，再用 LPA 软件管理里面的 profile。海外版固件同样适用。限制是：同一时间只能启用一个 profile，切换会短暂断网，并让基带重新读卡。

**现状**

- 本仓库**没有实现** eSIM 管理。上游 [jesther-ai/open-u60-pro](https://github.com/jesther-ai/open-u60-pro) 已经合并了基于 lpac 的 eSIM profile 管理和 iOS 界面（[PR #28](https://github.com/jesther-ai/open-u60-pro/pull/28)），本分支还没有合并它。
- `lpac-qrtr-build/` 是 [lpac](https://github.com/estkme-group/lpac) 的交叉编译配方：只带 QMI-over-QRTR 的 APDU 后端，做成 aarch64 musl 的独立包。背景见 [docs/research/U60_Pro_QRTR_lpac_Feasibility.md](../research/U60_Pro_QRTR_lpac_Feasibility.md)。这是研究用的，还没有用它在路由器上真正管理过 profile。
- [acloudpeng/U60Pro-OpenUI](https://github.com/acloudpeng/U60Pro-OpenUI)（针对国行 B31 固件）已经在路由器上实现了 profile 管理：静态编译的 lpac 加上 agent，经 QMI UIM 转发 APDU（Open Logical Channel `0x42`、Send APDU `0x3B`、关闭逻辑通道 `0x3F`）；切换 profile 的流程是：启用 profile → 射频进入低功耗 → SIM 断电再上电 → 射频上线。

**许可证**：lpac 主要以 AGPL-3.0 发布（少数文件用其他许可证，以 lpac 仓库为准），`lpac-qrtr-build/patches/` 也按 AGPL-3.0-only 发布。把 lpac 随设备分发、或做成网络服务提供给别人时，必须遵守 AGPL，包括提供对应的源码。

## 触摸屏程序

路由器的屏幕原本由原厂的 `zte_topsw_devui` 驱动。它是一个闭源的 LVGL 程序，图片资源在 `/usr/ui`，根本不知道“客户端/中继模式”这回事。我们**不给这个二进制打补丁**，而是另写了一个独立的屏幕程序 `openu60-screen`（`screen/`，Rust，DRM 原子模式设置，320×480），需要时接管屏幕，退出时把原厂界面还回去。

**两种运行方式**

- `openu60-screen watch`：由 agent 的 `screen_watch.rs` 拉起，常驻后台，原厂界面照常显示。它只监听电源键：快速连按三下（短按，每两下之间不超过 0.6 秒）时，或者 App 调用 `POST /api/screen/open` 时，启动下面的 `run`。
- `openu60-screen run`：通过 init 脚本停掉原厂界面，自己接管屏幕和触摸（Sitronix 触摸屏 `event3`、电源键 `event0`）；退出时总会再把原厂界面启动起来。

**界面**

- 锁屏：日期、大号时钟、三个圆环、实时网速、提醒，滑动进入。
- 网络：信号、实时速率、用量、已连接设备、电池、远程连接状态。
- 上网方式：SIM 热点 / Wi-Fi 中继 / Wi-Fi 客户端，以及附近可加入的网络（调用 agent 的 Wi-Fi 上行接口）。
- 充电：电池状态、充电上限（直供电）、反向充电。
- 广告拦截：24 小时计数和图表、开关、暂停、拦截最多的域名。
- 连接与配对：把热点做成 Wi-Fi 二维码，手机扫码就能连上，App 的引导也会扫它。
- 设置：外观（壁纸等）、显示（亮度等）、锁屏、高级选项，以及“回到原厂界面”。
- 来电时弹出全屏来电卡片；接听在手机上进行（音频在手机那边），这里只能拒接。

**按键**：单按电源键熄屏/点亮；长按约 2 秒弹出“重启 / 关机 / 取消”菜单；快速连按三下回到原厂界面。

**设置和接口**：屏幕设置、壁纸都在 `/data/openu60/screen/`，agent 提供 `GET/PUT /api/screen/settings`、`PUT/DELETE /api/screen/wallpaper`、`POST /api/screen/open`。

**安全措施**

- `zte_topsw_devui` 在开机同步列表里，**只能在运行时用 init 脚本临时停掉，绝不能用 `init.d disable` 停用**，否则开机会卡在 logo（见 [router-runtime.md](router-runtime.md#关闭的厂商服务)）。屏幕程序只调用 `/etc/init.d/zte_topsw_devui stop/start`，从不 disable。
- 开机不满 60 秒、原厂界面还没起来时，连按三下不会切换。
- `run` 在 30 秒内异常退出、且 2 分钟内发生两次时，`watch` 会暂时留在原厂界面，不再尝试。
- 想彻底停用：`adb shell 'touch /data/openu60/screen/disabled'`，删掉这个文件即恢复。卸载（助手或 `u60.sh uninstall`）会删掉程序并把原厂界面启动起来。
- 屏幕卡住时，手动恢复原厂界面：`sh /usr/bin/mtdev2tuio.sh`，然后 `kill -9 $(pidof zte_topsw_devui); /usr/bin/zte_topsw_devui &`。

**调试**：`cargo run -p openu60-screen --bin screen-preview -- <正文字体.ttf> <数字字体.ttf> <输出目录>` 可以在 Mac 上用模拟数据把各页面渲染成 PNG；`screen/screen_test.sh` 是早期点亮测试用的脚本，停掉原厂界面、跑测试程序，结束时（即使出错）一定把原厂界面恢复回来。

**参考**：思路来自 [acloudpeng/U60Pro-OpenUI](https://github.com/acloudpeng/U60Pro-OpenUI) 的 DevUI 方案（其屏幕程序为 GPL-3.0-or-later，渲染部分来自 MIT 许可的 [33333s/zwrt-devui](https://github.com/33333s/zwrt-devui)）。如果你要复用 OpenUI 的代码，请遵守 GPL。
