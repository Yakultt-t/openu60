# 安装

两条路线，结果一样：用 macOS 上的“U60 刷机助手”点几下，或者按顺序跑 `scripts/u60/` 里的脚本。两条路线都要先在路由器上开启 ADB，都会关闭中兴的固件升级和每周定时重启，再装上 agent 和开机钩子。根目录的 [README](../../README.md) 有一份照着做就行的精简版，这里是每一步的细节。

> **先读这里**
> - 目前只在海外版固件 `STD_PL_MU5250V1.0.0B02` 上验证过。别的固件（国行 B27/B31 等）上，厂商守护进程列表、短信加密和 Wi-Fi 实现都可能不同，请先自行确认（见下文 03 的警告）。
> - 改动可能让路由器停在开机 logo、失去保修或丢数据。先做备份，备份里有设备标识和你的配置，不要公开。
> - 涉及两个密码，别弄混：
>   - **路由器后台管理员密码**：网页 `http://192.168.0.1` 的登录密码，只在开启 ADB 时用一次，脚本和助手都不保存；
>   - **U60 助手密码**（agent 密码）：安装 agent 时生成，手机 App、网页管理面板（`http://192.168.0.1:9090`）和 HTTP API 都用它登录。

## 1. 准备

**硬件**：U60 Pro、能传数据的 USB-C 线、一台 Mac。开启 ADB 时，Mac 要连着 U60 的 Wi-Fi（要访问 `http://192.168.0.1`），同时用 USB 线连着路由器（ADB 走 USB）。

**软件**（Homebrew）：

```bash
brew install android-platform-tools jq xcodegen zig pnpm node   # pnpm 用来构建网页面板，node 只有 07 用到
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # Rust（rustup）
source "$HOME/.cargo/env"
rustup target add aarch64-unknown-linux-musl
cargo install cargo-zigbuild
```

构建 U60 刷机助手需要 Xcode（助手要求 macOS 15 及以上）。`06_build_agent.sh` 默认使用 `~/.rustup` 里 Apple Silicon 版的 stable 工具链。

**私有状态目录**：脚本把不能进仓库的东西放在 `${U60_HOME:-~/u60}`（仓库外）：agent 密码 `agent-credentials.txt`、构建日志、通话音频测试结果，以及可选的 `config.env`。

公开版本不带任何默认中继或推送网关。想让自己编译的 agent 默认就用你自建的服务，可以写一个 `config.env`（编译时生效；不写也完全能用，只是远程访问要装好后再用 API 配置）：

```bash
mkdir -p ~/u60 && chmod 700 ~/u60
cat > ~/u60/config.env <<'EOF'
# 只放在本机，不要提交。脚本启动时会读取这个文件；变量要 export 才能传给编译器。
export OPENU60_DEFAULT_RELAYS=relay.example.com:443             # 自建中继，多个用逗号分隔
export OPENU60_PUSH_GATEWAY=https://push.example.com/v1/push    # 自建推送网关（可选）
EOF
chmod 600 ~/u60/config.env
```

想放到别的地方就 `export U60_HOME=/path/to/dir`。

## 2. 路线 A：U60 刷机助手（Mac app）

助手位于 `desktop/U60Toolkit/`，界面是中文，安装逻辑在 `Resources/payload/u60.sh` 里。它会把编译好的 agent 和触摸屏程序打包进去，所以先编译，再构建助手：

```bash
(cd web-admin && pnpm install && pnpm build)   # 网页管理面板（可跳过，跳过则面板是占位页）
scripts/u60/06_build_agent.sh            # 编译 agent 和触摸屏程序 openu60-screen
desktop/U60Toolkit/sync_payload.sh       # 把它们和设备脚本拷进助手
cd desktop/U60Toolkit
xcodegen generate
xcodebuild -project U60Toolkit.xcodeproj -scheme U60Toolkit -configuration Release \
  -derivedDataPath build build
open "build/Build/Products/Release/U60 刷机助手.app"
```

助手用的是 ad-hoc 签名，不需要开发者账号。拷到别的 Mac 上时，第一次要在 Finder 里右键选“打开”，或者到“系统设置 → 隐私与安全性”里点“仍要打开”。

使用步骤：

开始前：路由器开机、装好 SIM 卡，用 USB 线接到 Mac，Mac 连上路由器的 Wi-Fi。

1. **① 开启 USB 调试**：填中兴后台管理员密码（默认印在路由器标签上），点“开启”。助手会登录网页后台，把 USB 切到调试模式，约 10 秒后 ADB 出现。密码不保存。
2. **② 备份**：选“配置备份”（几 MB，约 1 分钟：设备信息、分区表、全部 UCI 配置、`/etc` overlay、agent 数据），或“完整备份”（整块 eMMC 约 7.6 GB，15 分钟左右，带 md5 校验）。
3. **③ 安装 U60 Pro 软件**：
   - 会关闭固件升级、中兴云服务和中兴自带的每周定时重启，把 agent、触摸屏程序、守护进程、Wi-Fi 上行脚本和开机钩子装到路由器上；
   - 会生成一个 U60 助手密码（8–64 位，只能用字母、数字和 `-_.`），**只显示这一次**，记下来；更新时勾选“保留现在的 U60 助手密码”，已配对的手机不用重新登录。
4. **④ 用手机配对**：在 iPhone 上安装 U60 助手 App（不开源，即将在 App Store 上架），按引导连上路由器的 Wi-Fi（可以扫路由器屏幕上的 Wi-Fi 二维码），输入 U60 助手密码。没有 App 也可以直接用路由器的触摸屏和网页管理面板，见第 4 节。
5. **卸载（可选）**：停止并删除 agent 和触摸屏程序，去掉开机钩子，把 Wi-Fi 切回热点模式；`/data/openu60` 里的数据保留，固件升级仍保持关闭。

同一个脚本也可以直接在终端里用：

```bash
desktop/U60Toolkit/sync_payload.sh                 # install 要用 payload 里的 agent，先拷进去
P=desktop/U60Toolkit/Resources/payload
$P/u60.sh status
$P/u60.sh backup-config ~/u60/backup-config-$(date +%Y%m%d)
$P/u60.sh backup-full   ~/u60/backup-full-$(date +%Y%m%d)
read -rs AGENT_PASSWORD; export AGENT_PASSWORD     # 首次安装时设置 U60 助手密码（不回显）
$P/u60.sh install                                  # 可选（自己有 APNs 密钥时）：APNS_P8=… APNS_ENV=production TEAM_ID=… BUNDLE_ID=…
$P/u60.sh uninstall
```

更新时不设 `AGENT_PASSWORD`，就会沿用路由器上已有的密码。

## 3. 路线 B：命令行脚本

所有脚本都在仓库根目录下运行（`scripts/u60/…`），路由器端用的脚本在 `scripts/u60/device/`。

### 01 开启 ADB

```bash
scripts/u60/01_enable_adb.sh          # 路由器不在 192.168.0.1 时：GATEWAY=192.168.x.1 scripts/u60/01_enable_adb.sh
```

流程：先匿名读取固件信息和登录盐值，**交互输入**后台管理员密码（不回显，按网页的方式做两次 SHA-256 后登录，不写盘），再调用 `zwrt_bsp.usb set {"mode":"debug"}` 给 USB 加上 ADB，等 `adb devices` 看到设备，最后在 root shell 里打印固件版本、USB 模式和分区。方法来自 [amenekowo/mu5250_tweaking](https://github.com/amenekowo/mu5250_tweaking)。

### 02 备份

```bash
scripts/u60/02_backup.sh "${U60_HOME:-$HOME/u60}/backup-$(date +%Y%m%d)"
```

不给参数时，默认输出到当前目录下的 `./backup-时间戳`。脚本会保存设备信息、分区表、挂载信息、关键 UCI 配置和 `uci export` 的全部配置，再用 `dd` 经 `adb exec-out` 导出 `mmcblk0`（约 7.6 GB）和两个 4 MB 的 boot 区，并比对设备端与本地的 md5。系统运行时可写分区一直在变，所以整盘 `mmcblk0` 的 md5 对不上属正常，两个 boot 区应当一致。

目前没有公开、可靠的整机写回流程，完整备份主要用于留档和找回配置。

### 03 关闭固件升级和中兴云

```bash
scripts/u60/03_disable_cloud.sh
```

停用并禁止开机启动：`zte_dm`（每天检查固件升级）、`zte_topsw_fota_result`、`zte_smart_manage`、`zte_topsw_get_brand`、`scp_agent`（与中兴云的长连接和远程管理），关闭 TR-069 的周期上报，再停用 `zte_cutoff_protect`。最后这个服务会 ping 公网，失败后会**重启路由器**：通话时数据短暂中断、或者关掉蜂窝数据只走 Wi-Fi 上行时都会触发。所有改动都可以用 `/etc/init.d/<服务> enable` 撤销。

> **警告：03 之后要马上执行 04，中间不要重启。** `zte_dm` 和 `zte_topsw_fota_result` 在厂商主守护进程的开机同步列表 `/etc/config/zte_topsw_daemon.conf` 里。用 init.d 停用了列表里的进程，开机会一直卡在 logo（屏幕不出界面、不拨号）。04 的第一步就是把这两行注释掉。其他固件上同步列表可能还有别的进程（上游文档记录过 `zte_smart_manage` 也在列表里），先检查：
>
> ```bash
> adb shell 'grep -nE "^#?sync" /etc/config/zte_topsw_daemon.conf'
> ```
>
> 凡是 03 要停用、又出现在同步列表里的进程，都要先在这个文件里注释掉。开机后可以用 `ubus call zwrt_topsw_daemon.sync get_sync_info '{}'` 检查，正常应返回 `"sync success"`。

### 06 编译 agent

```bash
scripts/u60/06_build_agent.sh
```

用 `cargo zigbuild` 交叉编译 `aarch64-unknown-linux-musl` 的 release 版，产物在 `target/aarch64-unknown-linux-musl/release/`：agent `zte-agent`（同目录下还有调试工具 `qmi_probe` 和 `wms_listen`）和触摸屏程序 `openu60-screen`。日志写到 `$U60_HOME/agent-build.log`。想要带网页管理面板的版本，先在 `web-admin/` 里执行 `pnpm install && pnpm build`，否则嵌入的是一个占位页。`config.env` 里 export 的 `OPENU60_DEFAULT_RELAYS`、`OPENU60_PUSH_GATEWAY` 会在这一步编进 agent。

### 04 部署 agent

```bash
scripts/u60/04_deploy_agent.sh target/aarch64-unknown-linux-musl/release/zte-agent
```

1. 把 `zte_dm`、`zte_topsw_fota_result` 两行从开机同步列表注释掉（原文件备份为 `/data/zte_topsw_daemon.conf.orig`）。
2. 先让守护进程退出（`/tmp/zte-agent.stop`），停掉旧 agent，清理占着 :9090 的 `ubus listen` 子进程，再推送二进制到 `/data/zte-agent`。
3. **第一次运行时生成 24 位随机的 agent 密码**（即 U60 助手密码），写进 `$U60_HOME/agent-credentials.txt`（权限 600），从不打印；以后再部署会沿用这个密码。密码另外只存在路由器上的启动脚本 `/data/local/tmp/start_zte_agent.sh` 里（0700）。已经装好的推送配置会保留。
4. 推送守护进程 `zte-agent-run.sh` 和 Wi-Fi 上行脚本。
5. 在 `/etc/rc.local` 里加三个开机钩子（重复运行不会重复添加）：开机 15 秒后重新开启 ADB；启动 agent；开机 45 秒后恢复 Wi-Fi 上行。并关闭中兴固件自带的每周定时重启（`zwrt_zte_mc.reboot_schedule`，见 [router-runtime.md](router-runtime.md#关闭的厂商服务)）。
6. 启动 agent，通过 `adb forward tcp:19090 tcp:9090` 用生成的密码登录验证，打印 `/api/device`。

04 只部署 agent，不装触摸屏程序。想要触摸屏界面，再手动推上去（agent 每分钟检查一次，看到文件就会拉起，不用重启）：

```bash
adb shell 'mkdir -p /data/openu60/screen'
adb push target/aarch64-unknown-linux-musl/release/openu60-screen /data/openu60/screen/openu60-screen
adb shell 'chmod 755 /data/openu60/screen/openu60-screen'
```

更新 agent：重新跑 06 和 04 即可，密码不变，已绑定的手机不受影响。

### 推送（可选）

推送让手机 App 在被关掉时也能收到短信提醒和来电。agent 有两种发推送的方式（`zte-agent/src/push.rs`）：

- **经推送网关**：路由器不持有 Apple 的密钥，用自己的隧道身份给请求签名，交给推送网关 `openu60-pushgw`（`pushgw/`，自建），由网关转发给 Apple。网关地址用环境变量 `ZTE_AGENT_PUSH_GATEWAY` 指定，或编译时用 `OPENU60_PUSH_GATEWAY` 写进去。**公开版本没有默认网关**，不配置就不发推送。
- **直连 APNs**：路由器自己持有 `.p8` 密钥。只适合 `.p8` 和 App 属于同一个 Apple 开发者团队的情况。把密钥和配置交给安装脚本：`APNS_P8=/path/AuthKey_XXXXXXXXXX.p8 TEAM_ID=… BUNDLE_ID=… APNS_ENV=production desktop/U60Toolkit/Resources/payload/u60.sh install`，它会把密钥放到 `/data/apns.p8`、配置写到 `/data/zte-agent-apns.json`（都是 0600）。

检查（`api` 函数见第 5 节）：

```bash
api GET /api/push/status | jq .
api POST /api/push/test
```

## 4. 装好之后

**触摸屏**：路由器开机满 1 分钟后，快速连按三下电源键（每两下之间不超过 0.6 秒），屏幕切换到 OpenU60 界面；再连按三下或在“设置”里点“回到原厂界面”就切回去。单按熄屏/点亮，长按约 2 秒弹出重启/关机菜单。详见 [esim-and-screen.md](esim-and-screen.md#触摸屏程序)。

**网页管理面板**：连上路由器的 Wi-Fi，浏览器打开 `http://192.168.0.1:9090`，用 U60 助手密码登录。短信、电话、Wi-Fi、移动网络等都在这里。

**手机 App**：U60 助手 App 不开源，即将在 App Store 上架。iPhone 连上 U60 的 Wi-Fi，打开 App，按引导输入 U60 助手密码即可。在外面使用需要先[自建中继](remote-access.md)。

**接家里的 Wi-Fi**：在触摸屏的“上网方式”页面、网页面板或 App 里选择客户端/中继模式和家里的网络，见 [router-runtime.md](router-runtime.md#wi-fi-上行)。

**查看 agent 密码**：命令行路线在 Mac 上 `sed -n 's/^ZTE_AGENT_PASSWORD=//p' "${U60_HOME:-$HOME/u60}/agent-credentials.txt"`；也可以从路由器上读回：

```bash
adb shell "sed -n \"s/^export ZTE_AGENT_PASSWORD='\(.*\)'$/\1/p\" /data/local/tmp/start_zte_agent.sh"
```

## 5. 从 Mac 调用 agent API

文档里的 `api …` 指的是下面这个小函数。它经 ADB 转发连接 agent，密码直接从 `agent-credentials.txt` 读取，不用手打，也不会进 shell 历史：

```bash
adb forward tcp:19090 tcp:9090
PW=$(sed -n 's/^ZTE_AGENT_PASSWORD=//p' "${U60_HOME:-$HOME/u60}/agent-credentials.txt")
TOKEN=$(jq -n --arg pw "$PW" '{password: $pw}' \
  | curl -s http://127.0.0.1:19090/api/auth/login -H 'Content-Type: application/json' -d @- \
  | jq -r .data.token)
api() {  # api <方法> <路径> [JSON]，bash 和 zsh 都能用
  if [ -n "${3:-}" ]; then
    curl -s -X "$1" "http://127.0.0.1:19090$2" -H "Authorization: Bearer $TOKEN" \
         -H 'Content-Type: application/json' -d "$3"
  else
    curl -s -X "$1" "http://127.0.0.1:19090$2" -H "Authorization: Bearer $TOKEN"
  fi
}
api GET /api/device | jq .
```

## 6. 卸载与还原

- 卸载 agent：用助手的“卸载…”，或执行 `desktop/U60Toolkit/Resources/payload/u60.sh uninstall`。开机 15 秒后开启 ADB 的那行钩子仍会留在 `/etc/rc.local` 里，不需要时手动删掉。
- 恢复中兴固件升级和云服务（恢复后固件升级会重新生效，一次升级就可能清掉 agent 和 ADB）：

```bash
adb shell 'for s in zte_dm zte_topsw_fota_result zte_smart_manage zte_topsw_get_brand scp_agent zte_cutoff_protect; do
  [ -x /etc/init.d/$s ] && /etc/init.d/$s enable; done
cp /data/zte_topsw_daemon.conf.orig /etc/config/zte_topsw_daemon.conf'
adb reboot
```

- 开机卡在 logo 时，最常见的原因是同步列表（见上文 03 的警告）。只要 ADB 还能连上，就可以修复：注释掉同步列表里被停用的进程，或者重新 `enable` 它们。init.d 的 disable 会在 `/zteoverlay/etc-upper_a/rc.d/` 里留下 whiteout 文件，可用 `ls -la /zteoverlay/etc-upper_a/rc.d/ | grep '^c'` 查看。

## 7. 脚本一览

| 脚本 | 用途 |
|---|---|
| `01_enable_adb.sh` | 网页后台登录（密码交互输入、不保存），开启 USB 调试，检查 root shell |
| `02_backup.sh [目录]` | 整块 eMMC、boot 区、分区表、UCI 配置备份，带 md5 校验 |
| `03_disable_cloud.sh` | 关闭 FOTA、中兴云、TR-069 周期上报和断网自动重启；之后马上执行 04 |
| `04_deploy_agent.sh <二进制>` | 修正开机同步列表，部署 agent 和守护进程，写开机钩子，关闭每周定时重启，生成并保存 agent 密码 |
| `06_build_agent.sh` | 交叉编译 agent 和触摸屏程序（aarch64 musl） |
| `07_sms_probe.sh` | 在 Mac 上验证 STD 固件的短信加密（要后台管理员密码和 node） |
| `08`–`11` | 通话音频实验与测试，会真的拨打电话，见 [call-audio.md](call-audio.md) |
| `device/zte-agent-run.sh` | 路由器上的守护进程（崩溃重启、卡死检测、日志裁剪） |
| `device/wifi_uplink.sh`、`device/wifi_uplink_dhcp.sh` | Wi-Fi 上行（客户端/中继模式） |
| `device/bridge_client.lua`、`device/bridge_client_run.sh`、`device/audio_probe.sh` | 通话音频测试用 |
