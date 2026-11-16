import Foundation
import Darwin

struct OMPInstallation: Equatable, Sendable {
    var executable: String?
    var version: String?

    static func detect(
        environment: [String: String] = ProcessInfo.processInfo.environment,
        home: URL = FileManager.default.homeDirectoryForCurrentUser,
        candidatePaths: [String]? = nil,
        timeout: TimeInterval = 4
    ) async -> Self {
        await Task.detached(priority: .utility) {
            let candidates = candidatePaths ?? executableCandidates(environment: environment, home: home)
            let executable = candidates.first { path in
                var isDirectory: ObjCBool = false
                return path.hasPrefix("/") && FileManager.default.fileExists(atPath: path, isDirectory: &isDirectory)
                    && !isDirectory.boolValue && FileManager.default.isExecutableFile(atPath: path)
            }
            guard let executable else { return Self() }
            let output = versionOutput(executable: executable, environment: environment, timeout: timeout)
            return Self(executable: executable, version: output.flatMap(parseVersion))
        }.value
    }

    func installExtension(packageURL: URL, environment: [String: String] = ProcessInfo.processInfo.environment) async throws {
        guard let executable else {
            throw NSError(domain: "OMPInstallation", code: 1, userInfo: [NSLocalizedDescriptionKey: "未找到 OMP，请先安装 OMP 或选择其可执行文件。"])
        }
        try await Task.detached(priority: .utility) {
            let result = try Self.commandOutput(
                executable: executable,
                arguments: ["plugin", "install", "@codexpress/omp-extension@file:\(packageURL.path)"],
                environment: environment,
                currentDirectory: packageURL.deletingLastPathComponent(),
                timeout: 180,
                outputLimit: 1024 * 1024
            )
            guard result.status == 0 else {
                let detail = result.output.trimmingCharacters(in: .whitespacesAndNewlines)
                throw NSError(domain: "OMPInstallation", code: Int(result.status), userInfo: [
                    NSLocalizedDescriptionKey: "OMP 扩展安装失败（\(result.status)）。\(detail.isEmpty ? "" : "\n\(detail)")"
                ])
            }
        }.value
    }

    private static func executableCandidates(environment: [String: String], home: URL) -> [String] {
        let path = (environment["PATH"] ?? "").split(separator: ":").filter { $0.hasPrefix("/") }
            .map { URL(fileURLWithPath: String($0)).appendingPathComponent("omp").path }
        return path + ["/opt/homebrew/bin/omp", "/usr/local/bin/omp",
                       home.appendingPathComponent(".local/bin/omp").path,
                       home.appendingPathComponent(".bun/bin/omp").path]
    }

    private static func parseVersion(_ output: String) -> String? {
        let line = output.trimmingCharacters(in: .whitespacesAndNewlines)
        let pattern = #"^omp(?:/|[ \t]+v?)([0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?)$"#
        guard let regex = try? NSRegularExpression(pattern: pattern),
              let match = regex.firstMatch(in: line, range: NSRange(line.startIndex..., in: line)),
              let range = Range(match.range(at: 1), in: line) else { return nil }
        return String(line[range])
    }

    private static func versionOutput(executable: String, environment: [String: String], timeout: TimeInterval) -> String? {
        let deadline = min(max(timeout.isFinite ? timeout : 4, 0.05), 5)
        guard let result = try? commandOutput(
            executable: executable, arguments: ["--version"], environment: environment,
            currentDirectory: FileManager.default.temporaryDirectory, timeout: deadline, outputLimit: 8192
        ), result.status == 0 else { return nil }
        return result.output
    }

    private static func commandOutput(
        executable: String, arguments: [String], environment: [String: String],
        currentDirectory: URL, timeout: TimeInterval, outputLimit: Int
    ) throws -> (status: Int32, output: String) {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: executable)
        process.arguments = arguments
        process.environment = environment
        process.currentDirectoryURL = currentDirectory
        process.standardInput = FileHandle.nullDevice
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = pipe
        defer {
            stop(process)
            try? pipe.fileHandleForReading.close()
            try? pipe.fileHandleForWriting.close()
        }
        let descriptor = pipe.fileHandleForReading.fileDescriptor
        let flags = fcntl(descriptor, F_GETFL)
        guard flags >= 0, fcntl(descriptor, F_SETFL, flags | O_NONBLOCK) >= 0 else {
            throw POSIXError(.EIO)
        }
        try process.run()
        let deadline = ProcessInfo.processInfo.systemUptime + timeout
        var output = Data()
        var bytes = [UInt8](repeating: 0, count: 4096)
        while true {
            let count = Darwin.read(descriptor, &bytes, bytes.count)
            if count > 0 {
                guard output.count + count <= outputLimit else {
                    throw NSError(domain: "OMPInstallation", code: 2, userInfo: [NSLocalizedDescriptionKey: "OMP 输出过多，操作已停止。"])
                }
                output.append(contentsOf: bytes.prefix(count))
            } else if count < 0 && errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR {
                throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
            } else if !process.isRunning {
                return (process.terminationStatus, String(decoding: output, as: UTF8.self))
            }
            guard ProcessInfo.processInfo.systemUptime < deadline else {
                throw NSError(domain: "OMPInstallation", code: 3, userInfo: [NSLocalizedDescriptionKey: "OMP 操作超时，请检查 OMP 是否能正常启动后重试。"])
            }
            if count <= 0 { usleep(10_000) }
        }
    }

    private static func stop(_ process: Process) {
        guard process.isRunning else { return }
        process.terminate()
        let deadline = ProcessInfo.processInfo.systemUptime + 0.1
        while process.isRunning && ProcessInfo.processInfo.systemUptime < deadline { usleep(5_000) }
        if process.isRunning { kill(process.processIdentifier, SIGKILL) }
    }
}
