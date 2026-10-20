import Foundation
import Darwin

struct SSHRemoteProfile: Codable, Equatable, Identifiable, Sendable {
    var id = UUID().uuidString
    var destination = ""
    var username = ""
    var sshPort = ""
    var identityFile = ""
    var codexHome = ""
    var gatewayHost = ""
    var backupID: String? = nil
    var mapExistingProviders = false

    func arguments() throws -> [String] {
        let allowed = CharacterSet(charactersIn: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.-_:")
        guard !destination.isEmpty, !destination.hasPrefix("-"), destination.unicodeScalars.allSatisfy(allowed.contains), destination.utf8.count <= 253 else { throw AppError.message("请填写 SSH 配置别名、服务器 IP 或主机名；用户名单独填写。") }
        guard username.isEmpty || (!username.hasPrefix("-") && username.unicodeScalars.allSatisfy(allowed.contains)) else { throw AppError.message("SSH 用户名无效。") }
        var args = ["-T", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes", "-o", "ConnectTimeout=12", "-o", "ServerAliveInterval=10", "-o", "ServerAliveCountMax=2", "-o", "LogLevel=ERROR"]
        if !username.isEmpty { args += ["-l", username] }
        if !sshPort.isEmpty {
            guard let port = Int(sshPort), (1...65535).contains(port), sshPort.allSatisfy(\.isNumber) else { throw AppError.message("SSH 端口须为 1–65535。") }
            args += ["-p", String(port)]
        }
        if !identityFile.isEmpty {
            guard identityFile.hasPrefix("/"), !identityFile.contains("\n") else { throw AppError.message("私钥文件须为绝对路径。") }
            args += ["-i", identityFile, "-o", "IdentitiesOnly=yes"]
        }
        args += ["--", destination, "python3 -c " + Self.shellQuote(SSHRemoteAgent.script)]
        return args
    }
    static func shellQuote(_ text: String) -> String { "'" + text.replacingOccurrences(of: "'", with: "'\"'\"'") + "'" }
}

private final class SSHOutput: @unchecked Sendable {
    private let lock = NSLock()
    private var value = Data()
    private var oversized = false
    func append(_ data: Data) { lock.lock(); defer { lock.unlock() }; if value.count + data.count > 12 * 1024 * 1024 { oversized = true } else { value.append(data) } }
    func result() throws -> Data { lock.lock(); defer { lock.unlock() }; guard !oversized else { throw AppError.message("SSH 响应过大，操作已停止。") }; return value }
}

struct SSHRemoteClient: Sendable {
    let profile: SSHRemoteProfile
    func call(_ action: String, fields: [String: JSONValue] = [:]) async throws -> JSONValue {
        let args = try profile.arguments()
        var payload = fields; payload["action"] = .string(action); payload["codex_home"] = .string(profile.codexHome)
        let input = try JSONEncoder().encode(JSONValue.object(payload))
        return try await Task.detached(priority: .utility) { try Self.execute(arguments: args, input: input) }.value
    }
    private static func execute(arguments args: [String], input: Data) throws -> JSONValue {
            let process = Process(); process.executableURL = URL(fileURLWithPath: "/usr/bin/ssh"); process.arguments = args
            let stdout = Pipe(), stderr = Pipe(), stdin = Pipe()
            process.standardOutput = stdout; process.standardError = stderr; process.standardInput = stdin
            _ = fcntl(stdin.fileHandleForWriting.fileDescriptor, F_SETNOSIGPIPE, 1)
            let output = SSHOutput(), errors = SSHOutput(); let readers = DispatchGroup()
            let completed = DispatchSemaphore(value: 0)
            process.terminationHandler = { _ in completed.signal() }
            try process.run()
            for (pipe, buffer) in [(stdout, output), (stderr, errors)] {
                readers.enter()
                DispatchQueue.global(qos: .utility).async {
                    while true { let data = pipe.fileHandleForReading.availableData; if data.isEmpty { break }; buffer.append(data) }
                    readers.leave()
                }
            }
            readers.enter()
            DispatchQueue.global(qos: .utility).async {
                defer { readers.leave(); try? stdin.fileHandleForWriting.close() }
                do { try stdin.fileHandleForWriting.write(contentsOf: input) }
                catch { if process.isRunning { process.terminate() } }
            }
            if completed.wait(timeout: .now() + 60) == .timedOut {
                process.terminate()
                if completed.wait(timeout: .now() + 2) == .timedOut { kill(process.processIdentifier, SIGKILL) }
                throw AppError.message("SSH 操作超时；应用或还原配置时请重新检查远端状态。")
            }
            guard readers.wait(timeout: .now() + 3) == .success else { throw AppError.message("SSH 输出未能完整读取。") }
            guard process.terminationStatus == 0 else {
                throw AppError.message("SSH 连接失败。请先在终端确认主机指纹并完成密钥登录或解锁，再检查用户名、端口、SSH 别名及远端 Python 3。")
            }
            let data = try output.result()
            guard let result = try? JSONDecoder().decode(JSONValue.self, from: data) else { throw AppError.message("远端未返回有效结果，请检查 Python 3 和 SSH 启动脚本输出。") }
            guard result["ok"].bool == true else { throw AppError.message(result["error"].string ?? "远端操作失败。") }
            return result["data"]
    }
}
