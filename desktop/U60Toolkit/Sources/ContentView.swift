import AppKit
import SwiftUI
import UniformTypeIdentifiers

struct ContentView: View {
    @State private var kit = Toolkit()

    // Debug mode
    @State private var gateway = "192.168.0.1"
    @State private var adminPassword = ""
    @State private var enabling = false
    @State private var enableMessage: (ok: Bool, text: String)?

    // Backup
    @State private var fullBackup = false

    // Install
    @State private var keepPassword = true
    @State private var appPassword = Toolkit.newPassword()
    @State private var confirmInstall = false
    @State private var confirmFullBackup = false
    /// Folder of the backup made in this window, so the install step can point at it.
    @State private var lastBackup: URL?
    @State private var confirmUninstall = false
    @State private var showPasswordOnce: String?

    var body: some View {
        HSplitView {
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    header
                    if kit.adbPath == nil { adbMissing }
                    debugBox
                    backupBox
                    installBox
                    pairBox
                    uninstallBox
                }
                .padding(20)
            }
            .frame(minWidth: 430)
            logPane
                .frame(minWidth: 300)
        }
        .task { kit.startWatching() }
        .confirmationDialog(kit.agentInstalled ? "更新这台路由器上的 U60 Pro 软件？" : "给这台路由器安装 U60 Pro 软件？", isPresented: $confirmInstall) {
            Button(kit.agentInstalled ? "更新" : "安装") { Task { await install() } }
        } message: {
            Text(lastBackup == nil
                 ? "会关闭中兴的固件自动升级和云服务（固件升级会把 U60 助手 冲掉），然后装上 U60 助手 并设置开机自启。原来的短信、通话和网络设置不受影响，随时可以卸载。\n\n你在这次还没有做备份，建议先做一次。"
                 : "会关闭中兴的固件自动升级和云服务（固件升级会把 U60 助手 冲掉），然后装上 U60 助手 并设置开机自启。原来的短信、通话和网络设置不受影响，随时可以卸载。")
        }
        .confirmationDialog("从这台 U60 Pro 卸载 U60 助手？", isPresented: $confirmUninstall) {
            Button("卸载", role: .destructive) { Task { await kit.run(["uninstall"]) } }
        } message: {
            Text("路由器会切回普通热点模式，U60 助手 app 将连不上它。数据会保留，重新安装后可以继续使用。")
        }
        .alert("完整备份大约需要 15 分钟", isPresented: $confirmFullBackup) {
            Button("开始") { chooseBackupFolder() }
            Button("取消", role: .cancel) {}
        } message: {
            Text("要把路由器的整块存储（约 7.6 GB）通过 USB 读出来，每一块都会校验。进行中请：\n• 不要拔 USB 线，不要关路由器\n• 不要合上 Mac 的盖子（助手会阻止 Mac 自动休眠）\n• 准备好约 8 GB 空闲空间\n\n右边会显示已读取多少、速度和预计剩余时间。")
        }
        .alert("请记下 U60 助手密码", isPresented: Binding(get: { showPasswordOnce != nil }, set: { if !$0 { showPasswordOnce = nil } })) {
            Button("复制密码") {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(showPasswordOnce ?? "", forType: .string)
                showPasswordOnce = nil
            }
            Button("好了", role: .cancel) { showPasswordOnce = nil }
        } message: {
            Text("下一步：手机下载 U60 助手 app，打开后按引导连上路由器的 Wi-Fi，输入这个密码完成配对：\n\n\(showPasswordOnce ?? "")\n\n这个密码只保存在路由器上，这里不会再显示。")
        }
    }

    // MARK: - Header

    private var header: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("U60 Pro 刷机助手").font(.largeTitle.bold())
            Text("用 USB 线把中兴 U60 Pro 连到这台 Mac，按顺序做下面四步：开启调试 → 备份 → 安装 U60 Pro 软件 → 用手机配对。")
                .foregroundStyle(.secondary)
            HStack(spacing: 8) {
                chip(kit.deviceConnected ? "路由器已连接" : "路由器未连接", on: kit.deviceConnected)
                if kit.deviceConnected {
                    chip(kit.agentInstalled ? (kit.status["agent_running"] == "yes" ? "U60 助手 运行中" : "U60 助手 已安装（未运行）") : "未安装 U60 助手", on: kit.agentInstalled)
                    chip(kit.status["fota"] == "off" ? "固件升级已关" : "固件升级开着", on: kit.status["fota"] == "off")
                    if let fw = kit.status["firmware"], !fw.isEmpty {
                        Text(fw).font(.caption.monospaced()).foregroundStyle(.secondary)
                    }
                }
            }
        }
    }

    private func chip(_ text: String, on: Bool) -> some View {
        Label(text, systemImage: on ? "checkmark.circle.fill" : "circle")
            .font(.caption.weight(.medium))
            .padding(.horizontal, 8).padding(.vertical, 4)
            .background((on ? Color.green : Color.secondary).opacity(0.15), in: Capsule())
            .foregroundStyle(on ? Color.green : Color.secondary)
    }

    private var adbMissing: some View {
        GroupBox {
            VStack(alignment: .leading, spacing: 8) {
                Label("没有找到 adb（安卓调试工具，用来通过 USB 和路由器通信）", systemImage: "exclamationmark.triangle.fill").foregroundStyle(.orange)
                Text("在“终端”里运行下面这行安装，然后重新打开本应用：")
                Text("brew install android-platform-tools").font(.body.monospaced()).textSelection(.enabled)
                Button("复制命令") {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString("brew install android-platform-tools", forType: .string)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    // MARK: - 1. Debug mode

    private var debugBox: some View {
        GroupBox("① 开启 USB 调试") {
            VStack(alignment: .leading, spacing: 10) {
                if kit.deviceConnected {
                    Label("已经通过 USB 连上路由器，可以跳过这一步。", systemImage: "checkmark.seal.fill").foregroundStyle(.green)
                } else {
                    Text("Mac 先连上路由器的 Wi-Fi（或用 USB 线），输入中兴网页后台的管理员密码（默认印在路由器标签上）。路由器会在 USB 口打开调试模式。密码只用这一次，不会保存。")
                        .font(.callout).foregroundStyle(.secondary)
                }
                HStack {
                    TextField("路由器地址", text: $gateway).frame(width: 140)
                    SecureField("中兴后台管理员密码", text: $adminPassword)
                    Button {
                        Task { await enableDebug() }
                    } label: {
                        if enabling { ProgressView().controlSize(.small) } else { Text("开启") }
                    }
                    .disabled(adminPassword.isEmpty || enabling || kit.isRunning)
                }
                if let m = enableMessage {
                    Text(m.text).font(.callout).foregroundStyle(m.ok ? .green : .red)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(6)
        }
    }

    private func enableDebug() async {
        enabling = true
        enableMessage = nil
        defer { enabling = false }
        do {
            try await RouterWeb.enableDebugMode(gateway: gateway.trimmingCharacters(in: .whitespaces), password: adminPassword)
            adminPassword = ""
            enableMessage = (true, "已开启，正在等路由器出现在 USB 上（约 10 秒）…")
            for _ in 0..<15 {
                try? await Task.sleep(for: .seconds(2))
                await kit.refreshStatus()
                if kit.deviceConnected { enableMessage = (true, "已通过 USB 连上路由器。"); return }
            }
            enableMessage = (false, "调试模式已打开，但这台 Mac 还没通过 USB 看到路由器。请确认 USB 线能传数据（有些线只能充电），换个 USB 口再试。")
        } catch {
            enableMessage = (false, error.localizedDescription)
        }
    }

    // MARK: - 2. Backup

    private var backupBox: some View {
        GroupBox("② 备份") {
            VStack(alignment: .leading, spacing: 10) {
                Picker("内容", selection: $fullBackup) {
                    Text("配置备份（几 MB，几秒钟）").tag(false)
                    Text("完整备份（整块存储约 7.6 GB，15 分钟左右）").tag(true)
                }
                .pickerStyle(.radioGroup)
                Text(fullBackup
                     ? "逐块读出路由器的整块存储并逐块校验，出问题时可以整机恢复。需要约 8 GB 空闲空间。"
                     : "设备信息、分区表、全部系统配置、/etc 持久分区和 U60 助手 数据。里面有 Wi-Fi 密码等信息，请妥善保存。")
                    .font(.callout).foregroundStyle(.secondary)
                Button("选择保存位置并备份…") {
                    if fullBackup { confirmFullBackup = true } else { chooseBackupFolder() }
                }
                .disabled(!kit.deviceConnected || kit.isRunning)
                if let lastBackup {
                    Label("备份已保存到“\(lastBackup.lastPathComponent)”。下一步：在下面 ③ 安装 U60 Pro 软件。", systemImage: "arrow.down.circle.fill")
                        .font(.callout).foregroundStyle(.green)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(6)
        }
    }

    private func chooseBackupFolder() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.canCreateDirectories = true
        panel.prompt = "备份到这里"
        panel.directoryURL = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask).first
        guard panel.runModal() == .OK, let base = panel.url else { return }
        let stamp = Date().formatted(.iso8601.year().month().day().time(includingFractionalSeconds: false)).replacingOccurrences(of: ":", with: "")
        let folder = base.appendingPathComponent("U60 备份 \(stamp)")
        Task {
            let ok = await kit.run([fullBackup ? "backup-full" : "backup-config", folder.path])
            if ok {
                lastBackup = folder
                NSWorkspace.shared.activateFileViewerSelecting([folder])
            }
        }
    }

    // MARK: - 3. Install

    private var installBox: some View {
        GroupBox("③ 安装 U60 Pro 软件") {
            VStack(alignment: .leading, spacing: 10) {
                if kit.agentInstalled {
                    Toggle("保留现在的 U60 助手密码（已配对的手机不用重新登录）", isOn: $keepPassword)
                }
                if !kit.agentInstalled || !keepPassword {
                    HStack {
                        Text("U60 助手密码")
                        TextField("", text: $appPassword).font(.body.monospaced())
                        Button("换一个") { appPassword = Toolkit.newPassword() }
                    }
                    if !passwordValid {
                        Text("8 到 64 位，只能用字母、数字和 - _ .").font(.caption).foregroundStyle(.red)
                    }
                    Text("手机 app 第一次配对时要输入这个密码，安装完成后会再提示你一次。").font(.caption).foregroundStyle(.secondary)
                }
                Label("来电和短信推送由 U60 助手 服务器自动提供，不需要任何设置。", systemImage: "bell.badge.fill")
                    .font(.callout).foregroundStyle(.secondary)
                Button(kit.agentInstalled ? "更新" : "安装") { confirmInstall = true }
                    .buttonStyle(.borderedProminent)
                    .disabled(!kit.deviceConnected || kit.isRunning || !installReady)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(6)
        }
    }

    private var passwordValid: Bool {
        (8...64).contains(appPassword.count) && appPassword.allSatisfy { $0.isLetter && $0.isASCII || $0.isNumber && $0.isASCII || "-_.".contains($0) }
    }

    private var installReady: Bool {
        let passwordOK = (kit.agentInstalled && keepPassword) || passwordValid
        return passwordOK
    }

    private func install() async {
        var env: [String: String] = [:]
        let newPassword = !(kit.agentInstalled && keepPassword)
        if newPassword { env["AGENT_PASSWORD"] = appPassword }
        let ok = await kit.run(["install"], env: env)
        if ok && newPassword {
            showPasswordOnce = appPassword
            appPassword = Toolkit.newPassword()
        }
    }

    // MARK: - 4. Pair

    private var pairBox: some View {
        GroupBox("④ 用手机配对") {
            VStack(alignment: .leading, spacing: 6) {
                Text("1. 在 iPhone 上安装 U60 助手 app 并打开。")
                Text("2. 按引导连上路由器的 Wi-Fi（可以扫路由器屏幕上的 Wi-Fi 二维码）。")
                Text("3. 输入安装时设置的 U60 助手密码，配对完成后在哪里都能用。")
                if kit.status["agent_running"] == "yes" {
                    Label("路由器这边已经准备好了，可以拿起手机配对。", systemImage: "checkmark.circle.fill").foregroundStyle(.green)
                }
            }
            .font(.callout)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(6)
        }
    }

    // MARK: - 5. Uninstall

    private var uninstallBox: some View {
        GroupBox("卸载（可选）") {
            HStack {
                Text("停止并移除 U60 助手，路由器回到普通热点模式。").font(.callout).foregroundStyle(.secondary)
                Spacer()
                Button("卸载…", role: .destructive) { confirmUninstall = true }
                    .disabled(!kit.deviceConnected || !kit.agentInstalled || kit.isRunning)
            }
            .padding(6)
        }
    }

    // MARK: - Log

    @ViewBuilder
    private func runningStatus(now: Date) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            if let fraction = kit.progressFraction {
                ProgressView(value: fraction)
            } else {
                ProgressView(value: Double(max(kit.stepNumber - 1, 0)), total: Double(max(kit.stepTotal, 1)))
            }
            Text(kit.stepTotal > 0 ? "第 \(kit.stepNumber)/\(kit.stepTotal) 步：\(kit.stepTitle)" : "准备中…")
                .font(.caption).foregroundStyle(.secondary)
            if kit.progressTotal > 0 {
                Text(progressLine)
                    .font(.caption.monospacedDigit()).foregroundStyle(.secondary)
            }
            let silent = now.timeIntervalSince(kit.lastOutputAt)
            if silent > 60 {
                Label("已经 \(Int(silent / 60)) 分钟没有新进度了。路由器可能还在处理，请再等一等；如果超过 5 分钟仍没变化，检查 USB 线是否松动后重试。", systemImage: "hourglass")
                    .font(.caption).foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
            } else {
                Text("请不要拔 USB 线，也不要合上 Mac。").font(.caption2).foregroundStyle(.tertiary)
            }
        }
    }

    private var progressLine: String {
        let gb = { (b: Int64) in String(format: "%.1f", Double(b) / 1_073_741_824) }
        var text = "已读取 \(gb(kit.progressDone)) / \(gb(kit.progressTotal)) GB"
        if kit.progressRate > 0 { text += String(format: " · %.1f MB/s", kit.progressRate / 1_048_576) }
        if let left = kit.secondsLeft {
            text += left < 90 ? " · 马上就好" : " · 预计还要 \(Int((left / 60).rounded())) 分钟"
        }
        return text
    }

    private var logPane: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text("进度").font(.headline)
                Spacer()
                Button("复制日志") { kit.copyLog() }.disabled(kit.log.isEmpty)
                Button("清空") { kit.log.removeAll() }.disabled(kit.log.isEmpty || kit.isRunning)
            }
            if kit.isRunning {
                TimelineView(.periodic(from: .now, by: 1)) { context in
                    runningStatus(now: context.date)
                }
            } else if let r = kit.result {
                Label(r.text, systemImage: r.ok ? "checkmark.circle.fill" : "xmark.octagon.fill")
                    .foregroundStyle(r.ok ? .green : .red)
            }
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 2) {
                        ForEach(Array(kit.log.enumerated()), id: \.offset) { i, line in
                            Text(line).font(.caption.monospaced()).textSelection(.enabled).id(i)
                                .foregroundStyle(line.hasPrefix("✗") ? .red : (line.hasPrefix("✓") ? .green : .primary))
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(8)
                }
                .background(Color(nsColor: .textBackgroundColor), in: RoundedRectangle(cornerRadius: 8))
                .onChange(of: kit.log.count) { _, n in
                    if n > 0 { proxy.scrollTo(n - 1, anchor: .bottom) }
                }
            }
        }
        .padding(16)
    }
}
