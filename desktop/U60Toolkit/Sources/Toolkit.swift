import AppKit
import Foundation
import Observation

/// Runs the bundled installer script (payload/u60.sh) and keeps what the window shows:
/// the router's state, the current step and the log.
@Observable
@MainActor
final class Toolkit {
    var adbPath: String?
    var status: [String: String] = [:]
    var isRunning = false
    var stepNumber = 0
    var stepTotal = 0
    var stepTitle = ""
    var log: [String] = []
    var result: (ok: Bool, text: String)?
    /// Byte progress inside a long step ("@@PROGRESS <done> <total>"), with rate and ETA.
    var progressDone: Int64 = 0
    var progressTotal: Int64 = 0
    var progressRate: Double = 0
    /// When the script last printed anything, so a silent step can be explained.
    var lastOutputAt = Date()
    private var progressStartedAt: Date?
    private var progressStartBytes: Int64 = 0
    private var sleepActivity: NSObjectProtocol?

    var progressFraction: Double? {
        progressTotal > 0 ? min(Double(progressDone) / Double(progressTotal), 1) : nil
    }

    /// Seconds left at the current rate, once it has settled.
    var secondsLeft: Double? {
        guard progressTotal > 0, progressRate > 0, let started = progressStartedAt,
              Date().timeIntervalSince(started) > 15 else { return nil }
        return Double(progressTotal - progressDone) / progressRate
    }

    static let commandNames: [String: String] = [
        "backup-config": "配置备份", "backup-full": "完整备份",
        "install": "安装 U60 Pro 软件", "uninstall": "卸载 U60 助手", "status": "读取状态",
    ]

    private var statusTask: Task<Void, Never>?

    var payload: URL? { Bundle.main.url(forResource: "payload", withExtension: nil) }
    var deviceConnected: Bool { status["adb"] == "connected" }
    var agentInstalled: Bool { status["agent"] == "installed" }

    init() {
        adbPath = Self.findADB()
    }

    static func findADB() -> String? {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        let candidates = ["/opt/homebrew/bin/adb", "/usr/local/bin/adb",
                          "\(home)/Library/Android/sdk/platform-tools/adb",
                          "\(home)/platform-tools/adb"]
        return candidates.first { FileManager.default.isExecutableFile(atPath: $0) }
    }

    /// Polls the router every few seconds while the window is open and nothing is running.
    func startWatching() {
        statusTask?.cancel()
        statusTask = Task { [weak self] in
            while !Task.isCancelled {
                await self?.refreshStatus()
                try? await Task.sleep(for: .seconds(4))
            }
        }
    }

    func refreshStatus() async {
        guard !isRunning, adbPath != nil else { return }
        var lines: [String] = []
        _ = await execute(["status"], env: [:]) { lines.append($0) }
        var parsed: [String: String] = [:]
        for line in lines {
            let parts = line.split(separator: "=", maxSplits: 1).map(String.init)
            if parts.count == 2 { parsed[parts[0]] = parts[1] }
        }
        if !parsed.isEmpty { status = parsed }
    }

    /// Runs one installer command, streaming its progress into the window.
    @discardableResult
    func run(_ args: [String], env: [String: String] = [:]) async -> Bool {
        guard !isRunning else { return false }
        isRunning = true
        result = nil
        stepNumber = 0
        stepTotal = 0
        stepTitle = ""
        progressDone = 0
        progressTotal = 0
        progressRate = 0
        progressStartedAt = nil
        lastOutputAt = Date()
        let name = args.first.map { Self.commandNames[$0] ?? $0 } ?? ""
        log.append("▶︎ 开始：" + name)
        // A full backup takes a quarter of an hour; the Mac must not doze off meanwhile.
        sleepActivity = ProcessInfo.processInfo.beginActivity(options: [.idleSystemSleepDisabled, .userInitiated],
                                                              reason: "U60 刷机助手：\(name)")
        var failure: String?
        var success: String?
        let code = await execute(args, env: env) { [weak self] line in
            guard let self else { return }
            self.lastOutputAt = Date()
            if line.hasPrefix("@@PROGRESS ") {
                let parts = line.dropFirst(11).split(separator: " ")
                if parts.count == 2, let done = Int64(parts[0]), let total = Int64(parts[1]) {
                    if self.progressStartedAt == nil { self.progressStartedAt = Date(); self.progressStartBytes = done }
                    self.progressDone = done
                    self.progressTotal = total
                    if let started = self.progressStartedAt {
                        let elapsed = Date().timeIntervalSince(started)
                        if elapsed > 0 { self.progressRate = Double(done - self.progressStartBytes) / elapsed }
                    }
                }
                return
            }
            if line.hasPrefix("@@STEP ") {
                let rest = line.dropFirst(7)
                let parts = rest.split(separator: " ", maxSplits: 1)
                let counts = parts.first?.split(separator: "/") ?? []
                if counts.count == 2 { self.stepNumber = Int(counts[0]) ?? 0; self.stepTotal = Int(counts[1]) ?? 0 }
                self.stepTitle = parts.count > 1 ? String(parts[1]) : ""
                self.log.append("— " + self.stepTitle)
            } else if line.hasPrefix("@@OK ") {
                success = String(line.dropFirst(5))
                self.log.append("✓ " + (success ?? ""))
            } else if line.hasPrefix("@@FAIL ") {
                failure = String(line.dropFirst(7))
                self.log.append("✗ " + (failure ?? ""))
            } else if !line.isEmpty {
                self.log.append(line)
            }
        }
        isRunning = false
        if let sleepActivity { ProcessInfo.processInfo.endActivity(sleepActivity) }
        sleepActivity = nil
        let ok = code == 0 && failure == nil
        result = (ok, ok ? (success ?? "完成") : (failure ?? "失败（退出码 \(code)）"))
        await refreshStatus()
        return ok
    }

    /// Process plumbing: bash payload/u60.sh <args>, adb on PATH, output line by line.
    private func execute(_ args: [String], env: [String: String], onLine: @escaping @MainActor (String) -> Void) async -> Int32 {
        guard let payload, let adbPath else { return 127 }
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/bash")
        process.arguments = [payload.appendingPathComponent("u60.sh").path] + args
        var environment = ProcessInfo.processInfo.environment
        let adbDir = (adbPath as NSString).deletingLastPathComponent
        environment["PATH"] = "\(adbDir):/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
        environment["ADB"] = adbPath
        for (k, v) in env { environment[k] = v }
        process.environment = environment
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = pipe
        let buffer = LineBuffer()
        pipe.fileHandleForReading.readabilityHandler = { handle in
            let data = handle.availableData
            guard !data.isEmpty else { return }
            for line in buffer.feed(data) {
                Task { @MainActor in onLine(line) }
            }
        }
        return await withCheckedContinuation { continuation in
            process.terminationHandler = { p in
                pipe.fileHandleForReading.readabilityHandler = nil
                let rest = pipe.fileHandleForReading.readDataToEndOfFile()
                let lines = buffer.feed(rest) + buffer.flush()
                Task { @MainActor in
                    for line in lines { onLine(line) }
                    continuation.resume(returning: p.terminationStatus)
                }
            }
            do { try process.run() } catch {
                Task { @MainActor in
                    onLine("无法启动安装脚本：\(error.localizedDescription)")
                    continuation.resume(returning: 126)
                }
            }
        }
    }

    static func newPassword() -> String {
        let chars = Array("ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789")
        return String((0..<20).map { _ in chars[Int.random(in: 0..<chars.count)] })
    }

    func copyLog() {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(log.joined(separator: "\n"), forType: .string)
    }
}

/// Splits a byte stream into lines across reads.
final class LineBuffer: @unchecked Sendable {
    private var pending = Data()
    private let lock = NSLock()

    func feed(_ data: Data) -> [String] {
        lock.lock(); defer { lock.unlock() }
        pending.append(data)
        var lines: [String] = []
        while let nl = pending.firstIndex(of: 0x0A) {
            let line = pending[pending.startIndex..<nl]
            lines.append(String(decoding: line, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines))
            pending.removeSubrange(pending.startIndex...nl)
        }
        return lines
    }

    func flush() -> [String] {
        lock.lock(); defer { lock.unlock() }
        defer { pending.removeAll() }
        let s = String(decoding: pending, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
        return s.isEmpty ? [] : [s]
    }
}
