import Foundation
import Darwin

struct OMPInstallation: Equatable, Sendable {
    var executable: String?
    var version: String?
    var agentDirectory: String

    static func detect(
        environment: [String: String] = ProcessInfo.processInfo.environment,
        home: URL = FileManager.default.homeDirectoryForCurrentUser,
        candidatePaths: [String]? = nil,
        currentDirectory: URL = URL(fileURLWithPath: FileManager.default.currentDirectoryPath),
        timeout: TimeInterval = 4
    ) async -> Self {
        await Task.detached(priority: .utility) {
            let directory = agentDirectory(environment: environment, home: home, currentDirectory: currentDirectory)
            let candidates = candidatePaths ?? executableCandidates(environment: environment, home: home)
            let executable = candidates.first { path in
                var isDirectory: ObjCBool = false
                return path.hasPrefix("/") && FileManager.default.fileExists(atPath: path, isDirectory: &isDirectory)
                    && !isDirectory.boolValue && FileManager.default.isExecutableFile(atPath: path)
            }
            guard let executable else { return Self(agentDirectory: directory) }
            let output = versionOutput(executable: executable, environment: environment, timeout: timeout)
            return Self(executable: executable, version: output.flatMap(parseVersion), agentDirectory: directory)
        }.value
    }

    private static func executableCandidates(environment: [String: String], home: URL) -> [String] {
        let path = (environment["PATH"] ?? "").split(separator: ":").filter { $0.hasPrefix("/") }
            .map { URL(fileURLWithPath: String($0)).appendingPathComponent("omp").path }
        return path + ["/opt/homebrew/bin/omp", "/usr/local/bin/omp",
                       home.appendingPathComponent(".local/bin/omp").path,
                       home.appendingPathComponent(".bun/bin/omp").path]
    }

    private static func agentDirectory(environment: [String: String], home: URL, currentDirectory: URL) -> String {
        // OMP 18's `config path` initializes writable storage and can migrate settings.
        // Resolve its documented directory rules without opening config or credentials.
        // XDG moves categorized data/state/cache, but does not move this config directory.
        let rootName = environment["PI_CONFIG_DIR"].flatMap { $0.isEmpty ? nil : $0 } ?? ".omp"
        let root = home.appendingPathComponent(rootName).standardizedFileURL
        let profile = (environment["OMP_PROFILE"] ?? environment["PI_PROFILE"] ?? "")
            .trimmingCharacters(in: .whitespacesAndNewlines)
        if !profile.isEmpty && profile != "default" {
            let pattern = #"^[a-z0-9][a-z0-9._-]{0,63}$"#
            let reserved = #"^(?i:CON|PRN|AUX|NUL|COM[0-9]|LPT[0-9])(?:\..*)?$"#
            guard profile.range(of: pattern, options: .regularExpression) != nil,
                  profile != ".", profile != "..", !profile.hasSuffix("."),
                  profile.range(of: reserved, options: .regularExpression) == nil else {
                return home.appendingPathComponent(".omp/agent").path
            }
            return root.appendingPathComponent("profiles").appendingPathComponent(profile)
                .appendingPathComponent("agent").path
        }
        if let override = environment["PI_CODING_AGENT_DIR"], !override.isEmpty,
           !override.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) }) {
            let resolved = override.hasPrefix("/") ? URL(fileURLWithPath: override)
                : currentDirectory.appendingPathComponent(override)
            return resolved.standardizedFileURL.path
        }
        return root.appendingPathComponent("agent").path
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
        let process = Process()
        process.executableURL = URL(fileURLWithPath: executable)
        process.arguments = ["--version"]
        process.environment = environment
        process.currentDirectoryURL = FileManager.default.temporaryDirectory
        process.standardInput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        let pipe = Pipe()
        process.standardOutput = pipe
        defer {
            try? pipe.fileHandleForReading.close()
            try? pipe.fileHandleForWriting.close()
        }
        let descriptor = pipe.fileHandleForReading.fileDescriptor
        let flags = fcntl(descriptor, F_GETFL)
        guard flags >= 0, fcntl(descriptor, F_SETFL, flags | O_NONBLOCK) >= 0 else { return nil }
        do { try process.run() } catch { return nil }
        // Keeping reads nonblocking also bounds a broken wrapper whose child retains stdout.
        let deadline = ProcessInfo.processInfo.systemUptime + min(max(timeout.isFinite ? timeout : 4, 0.05), 5)
        var output = Data()
        var bytes = [UInt8](repeating: 0, count: 4096)
        while true {
            let count = Darwin.read(descriptor, &bytes, bytes.count)
            if count > 0 {
                guard output.count + count <= 8192 else { stop(process); return nil }
                output.append(contentsOf: bytes.prefix(count))
            } else if count < 0 && errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR {
                stop(process); return nil
            } else if !process.isRunning {
                guard process.terminationStatus == 0 else { return nil }
                return String(data: output, encoding: .utf8)
            }
            guard ProcessInfo.processInfo.systemUptime < deadline else { stop(process); return nil }
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
