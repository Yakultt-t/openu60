import CryptoKit
import Foundation

/// The U60 Pro's own web admin (ubus over JSON-RPC at http://<gateway>/ubus/). Used once, to
/// switch the USB port to debug mode so ADB appears. The admin password is hashed here the
/// way the web page does it (SHA-256, then SHA-256 again with the router's salt) and is
/// never stored.
enum RouterWeb {
    struct Failure: LocalizedError {
        let message: String
        var errorDescription: String? { message }
    }

    private static let anonymous = String(repeating: "0", count: 32)

    static func enableDebugMode(gateway: String, password: String) async throws {
        let info = try await call(gateway, session: anonymous, object: "zwrt_web", method: "web_login_info", params: ["": ""])
        guard let salt = info["zte_web_sault"] as? String, !salt.isEmpty else {
            throw Failure(message: "路由器没有返回登录参数，确认地址是 \(gateway) 并且这台 Mac 连着它。")
        }
        let h1 = hex(SHA256.hash(data: Data(password.utf8)))
        let h2 = hex(SHA256.hash(data: Data((h1 + salt).utf8)))
        let login = try await call(gateway, session: anonymous, object: "zwrt_web", method: "web_login", params: ["password": h2])
        guard let session = login["ubus_rpc_session"] as? String, !session.isEmpty else {
            throw Failure(message: "后台密码不对，或者登录次数太多被暂时锁定了。")
        }
        _ = try await call(gateway, session: session, object: "zwrt_bsp.usb", method: "set", params: ["mode": "debug"])
    }

    private static func hex(_ digest: SHA256.Digest) -> String {
        digest.map { String(format: "%02X", $0) }.joined()
    }

    private static func call(_ gateway: String, session: String, object: String, method: String, params: [String: Any]) async throws -> [String: Any] {
        let ms = Int(Date().timeIntervalSince1970 * 1000)
        guard let url = URL(string: "http://\(gateway)/ubus/?t=\(ms)") else { throw Failure(message: "地址无效：\(gateway)") }
        var request = URLRequest(url: url, timeoutInterval: 10)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.setValue("http://\(gateway)/", forHTTPHeaderField: "Referer")
        request.setValue("http://\(gateway)", forHTTPHeaderField: "Origin")
        request.setValue("XMLHttpRequest", forHTTPHeaderField: "X-Requested-With")
        let body: [[String: Any]] = [["jsonrpc": "2.0", "id": 1, "method": "call", "params": [session, object, method, params]]]
        request.httpBody = try JSONSerialization.data(withJSONObject: body)
        let data: Data
        do {
            (data, _) = try await URLSession.shared.data(for: request)
        } catch {
            throw Failure(message: "连不上 http://\(gateway)：\(error.localizedDescription)")
        }
        guard let list = try JSONSerialization.jsonObject(with: data) as? [[String: Any]],
              let result = list.first?["result"] as? [Any] else {
            throw Failure(message: "路由器的回应看不懂。")
        }
        if let code = result.first as? Int, code != 0 {
            throw Failure(message: "路由器拒绝了请求（错误 \(code)）。")
        }
        return result.count > 1 ? (result[1] as? [String: Any] ?? [:]) : [:]
    }
}
