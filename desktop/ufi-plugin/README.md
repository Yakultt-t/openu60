# UFI-TOOLS 插件：SIM 随行 / OpenU60 安装与更新

给已经在中兴 U60 Pro 上装了 [UFI-TOOLS](https://github.com/kanoqwq/UFI-TOOLS) 的用户：不用电脑，在 UFI-TOOLS 网页里安装、在线更新或卸载 OpenU60。

## 用法

1. 在 UFI-TOOLS 里打开「高级功能」（插件需要 root 权限），等 1–2 分钟生效。
2. 下载 [`SIM随行-OpenU60安装.js`](SIM随行-OpenU60安装.js)，在 UFI-TOOLS「插件 → 添加插件」里导入并启用。
3. 页面下方会多出「SIM 随行 / OpenU60」卡片，点「展开」：
   - **安装**：选「带作者的中继」（在外面也能用手机连路由器）或「不带中继」。装好后会弹出 **SIM 随行密码**，只显示一次，请抄下来，iPhone App「SIM 随行」配对时要用。
   - **在线更新**：有新版本时出现。路由器上的 OpenU60 会自己下载、核对签名后安装，新版本启动不了会自动恢复旧版本；密码和配对都保留。
   - **重新安装**：旧版本（还不支持在线更新）用它更新一次，保留密码。
   - **卸载**：恢复原厂屏幕和热点模式，/data/openu60 里的数据保留。

如果已经在用别的屏幕插件（比如「U60 屏幕界面」），安装时勾选「不装触摸屏程序」，免得两个屏幕程序抢屏幕。

## 它做了什么

插件只是一个按钮界面。实际工作由路由器上的 [`openu60-netinstall.sh`](../../scripts/u60/device/openu60-netinstall.sh) 完成：

- 从本仓库 Releases 的 `router-update` 下载发布清单，用 Ed25519 公钥核对签名，再逐个核对文件的 SHA-256，任何一项不对就停止。
- 之后的步骤和电脑上的安装程序一样：关闭中兴固件自动升级和云服务、安装程序和开机启动、生成密码、启动并检查。
- 在线更新调用 OpenU60 自带的更新接口（和手机 App 里的「路由器软件」是同一套）。

也可以不用 UFI-TOOLS，直接在路由器的 root shell 里运行：

```sh
curl -fsSL -o /tmp/openu60-netinstall.sh https://github.com/Yakultt-t/openu60/releases/download/router-update/openu60-netinstall.sh
sh /tmp/openu60-netinstall.sh install relay     # 或 public；加 noscreen 不装触摸屏程序
sh /tmp/openu60-netinstall.sh ota               # 在线更新
sh /tmp/openu60-netinstall.sh status
```
