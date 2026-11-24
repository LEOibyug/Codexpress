import Foundation

struct LaunchctlEnvironment: Sendable {
    let setValue: @Sendable (String, String) throws -> Void
    let unsetValue: @Sendable (String) throws -> Void

    static let noop = LaunchctlEnvironment(
        setValue: { _, _ in },
        unsetValue: { _ in }
    )

    static let system = LaunchctlEnvironment(
        setValue: { name, value in
            try run(arguments: ["setenv", name, value], failureMessage: "无法更新 macOS 图形环境（launchctl）。")
        },
        unsetValue: { name in
            try run(arguments: ["unsetenv", name], failureMessage: "无法清理 macOS 图形环境（launchctl）。")
        }
    )

    private static func run(arguments: [String], failureMessage: String) throws {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/launchctl")
        process.arguments = arguments
        process.standardInput = FileHandle.nullDevice
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        do {
            try process.run()
        } catch {
            throw AppError.message(failureMessage)
        }
        process.waitUntilExit()
        guard process.terminationStatus == 0 else {
            throw AppError.message(failureMessage)
        }
    }
}

struct ChatbotEnvironmentAccess: Sendable {
    static let baseURLVariable = "CODEXPRESS_BASE_URL"
    static let apiKeyVariable = "CODEXPRESS_API_KEY"
    static let blockStart = "# >>> Codexpress environment >>>"
    static let blockEnd = "# <<< Codexpress environment <<<"
    static let zshrcBlock = """
    # >>> Codexpress environment >>>
    [ -f "$HOME/.codexpress/env.sh" ] && . "$HOME/.codexpress/env.sh"
    # <<< Codexpress environment <<<
    """
    private static let maxZshrcBytes = 2 * 1024 * 1024

    struct Inspection: Equatable, Sendable {
        let envFilePresent: Bool
        let zshrcHasBlock: Bool
        let zshrcMalformed: Bool
        var synced: Bool { envFilePresent && zshrcHasBlock && !zshrcMalformed }
    }

    private enum ZshrcBlockState {
        case none
        case valid(startIndex: Int, endIndex: Int)
        case malformed
    }

    private struct ZshrcTarget {
        let url: URL
        let exists: Bool
        let permissions: Int
        let existingText: String?
    }

    let homeDirectory: URL
    let launchctl: LaunchctlEnvironment

    var envDirectory: URL { homeDirectory.appendingPathComponent(".codexpress", isDirectory: true) }
    var envFile: URL { envDirectory.appendingPathComponent("env.sh") }
    var zshrcFile: URL { homeDirectory.appendingPathComponent(".zshrc") }

    static var live: Self {
        Self(homeDirectory: FileManager.default.homeDirectoryForCurrentUser, launchctl: .system)
    }

    static func shellSingleQuoted(_ value: String) -> String {
        "'\(value.replacingOccurrences(of: "'", with: "'\\''"))'"
    }

    func apply(baseURL: String, key: String) throws {
        try validate(baseURL: baseURL, key: key)
        let manager = FileManager.default
        if let attributes = try? manager.attributesOfItem(atPath: envDirectory.path),
           attributes[.type] as? FileAttributeType != .typeDirectory {
            throw AppError.message("环境变量配置目录 ~/.codexpress 不是普通目录。")
        }
        if let attributes = try? manager.attributesOfItem(atPath: envFile.path),
           attributes[.type] as? FileAttributeType != .typeRegular {
            throw AppError.message("环境变量文件 ~/.codexpress/env.sh 不是普通文件。")
        }
        let target = try resolveZshrcTarget()
        let nextZshrc = try updatedZshrcContent(existing: target.existingText)

        try manager.createDirectory(at: envDirectory, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        try manager.setAttributes([.posixPermissions: 0o700], ofItemAtPath: envDirectory.path)

        let script = """
        # Managed by Codexpress. Do not edit manually.
        export \(Self.baseURLVariable)=\(Self.shellSingleQuoted(baseURL))
        export \(Self.apiKeyVariable)=\(Self.shellSingleQuoted(key))

        """
        try writeAtomically(
            Data(script.utf8),
            to: envFile,
            temporaryPrefix: ".env-",
            permissions: 0o600,
            failureMessage: "无法保存环境变量文件 ~/.codexpress/env.sh。"
        )

        if let nextZshrc, nextZshrc != target.existingText {
            try writeAtomically(
                Data(nextZshrc.utf8),
                to: target.url,
                temporaryPrefix: ".codexpress-zshrc-",
                permissions: target.permissions,
                failureMessage: "无法更新 ~/.zshrc 中的环境变量配置。"
            )
        }

        try launchctl.setValue(Self.baseURLVariable, baseURL)
        try launchctl.setValue(Self.apiKeyVariable, key)
    }

    func remove() throws {
        var firstError: Error?

        do {
            let target = try resolveZshrcTarget()
            if target.exists, let existing = target.existingText, let cleaned = try removedZshrcContent(existing: existing), cleaned != existing {
                try writeAtomically(
                    Data(cleaned.utf8),
                    to: target.url,
                    temporaryPrefix: ".codexpress-zshrc-",
                    permissions: target.permissions,
                    failureMessage: "无法清理 ~/.zshrc 中的环境变量配置。"
                )
            }
        } catch {
            firstError = firstError ?? error
        }

        let manager = FileManager.default
        if let attributes = try? manager.attributesOfItem(atPath: envFile.path) {
            if attributes[.type] as? FileAttributeType != .typeRegular {
                if firstError == nil { firstError = AppError.message("环境变量文件 ~/.codexpress/env.sh 不是普通文件，未能删除。") }
            } else {
                do {
                    try manager.removeItem(at: envFile)
                } catch {
                    if firstError == nil { firstError = AppError.message("无法删除环境变量文件 ~/.codexpress/env.sh。") }
                }
            }
        }

        do {
            try launchctl.unsetValue(Self.baseURLVariable)
        } catch {
            firstError = firstError ?? error
        }
        do {
            try launchctl.unsetValue(Self.apiKeyVariable)
        } catch {
            firstError = firstError ?? error
        }

        if let firstError { throw firstError }
    }

    func inspect() -> Inspection {
        let manager = FileManager.default
        let envPresent = (try? manager.attributesOfItem(atPath: envFile.path))?[.type] as? FileAttributeType == .typeRegular
        guard let target = try? resolveZshrcTarget(), let text = target.existingText else {
            return Inspection(envFilePresent: envPresent, zshrcHasBlock: false, zshrcMalformed: false)
        }
        let lines = splitLines(text)
        switch parseZshrcBlock(lines) {
        case .none:
            return Inspection(envFilePresent: envPresent, zshrcHasBlock: false, zshrcMalformed: false)
        case .valid:
            return Inspection(envFilePresent: envPresent, zshrcHasBlock: true, zshrcMalformed: false)
        case .malformed:
            return Inspection(envFilePresent: envPresent, zshrcHasBlock: false, zshrcMalformed: true)
        }
    }

    private func validate(baseURL: String, key: String) throws {
        guard let url = URLComponents(string: baseURL),
              url.scheme == "http",
              ["127.0.0.1", "localhost"].contains(url.host ?? ""),
              let port = url.port, (1...65535).contains(port),
              url.path == "/v1",
              url.user == nil, url.password == nil, url.query == nil, url.fragment == nil,
              baseURL.utf8.allSatisfy({ (33...126).contains($0) }) else {
            throw AppError.message("聊天客户端接口地址无效，无法写入环境变量。")
        }
        guard (16...256).contains(key.utf8.count),
              key.utf8.allSatisfy({ (33...126).contains($0) }) else {
            throw AppError.message("聊天客户端 Key 格式无效，无法写入环境变量。")
        }
    }

    private func resolveZshrcTarget() throws -> ZshrcTarget {
        let manager = FileManager.default
        guard let attributes = try? manager.attributesOfItem(atPath: zshrcFile.path) else {
            return ZshrcTarget(url: zshrcFile, exists: false, permissions: 0o644, existingText: nil)
        }
        let targetURL: URL
        let permissions: Int
        switch attributes[.type] as? FileAttributeType {
        case .typeRegular:
            targetURL = zshrcFile
            permissions = (attributes[.posixPermissions] as? NSNumber)?.intValue ?? 0o644
        case .typeSymbolicLink:
            let resolved = zshrcFile.resolvingSymlinksInPath()
            guard let targetAttributes = try? manager.attributesOfItem(atPath: resolved.path),
                  targetAttributes[.type] as? FileAttributeType == .typeRegular else {
                throw AppError.message("~/.zshrc 符号链接目标无效，未修改配置。")
            }
            targetURL = resolved
            permissions = (targetAttributes[.posixPermissions] as? NSNumber)?.intValue ?? 0o644
        default:
            throw AppError.message("~/.zshrc 不是普通文件，未修改配置。")
        }

        let handle: FileHandle
        do {
            handle = try FileHandle(forReadingFrom: targetURL)
        } catch {
            throw AppError.message("无法读取 ~/.zshrc，未修改配置。")
        }
        defer { try? handle.close() }
        let data = try handle.read(upToCount: Self.maxZshrcBytes + 1) ?? Data()
        guard data.count <= Self.maxZshrcBytes else {
            throw AppError.message("~/.zshrc 体积超过 2 MiB 限制，未自动修改。")
        }
        guard let text = String(data: data, encoding: .utf8) else {
            throw AppError.message("~/.zshrc 不是有效的 UTF-8 文本，未自动修改。")
        }
        return ZshrcTarget(url: targetURL, exists: true, permissions: permissions, existingText: text)
    }

    private func splitLines(_ text: String) -> [String] {
        guard !text.isEmpty else { return [] }
        var lines = text.split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
        if text.hasSuffix("\n"), lines.last == "" {
            lines.removeLast()
        }
        return lines
    }

    private func normalizedLine(_ line: String) -> String {
        line.hasSuffix("\r") ? String(line.dropLast()) : line
    }

    private func parseZshrcBlock(_ lines: [String]) -> ZshrcBlockState {
        var starts: [Int] = []
        var ends: [Int] = []
        for (index, raw) in lines.enumerated() {
            let line = normalizedLine(raw)
            if line == Self.blockStart { starts.append(index) }
            else if line == Self.blockEnd { ends.append(index) }
        }
        if starts.isEmpty && ends.isEmpty { return .none }
        if starts.count == 1, ends.count == 1, starts[0] < ends[0] {
            return .valid(startIndex: starts[0], endIndex: ends[0])
        }
        return .malformed
    }

    private func updatedZshrcContent(existing: String?) throws -> String? {
        let blockLines = Self.zshrcBlock.split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
        guard let existing, !existing.isEmpty else {
            return Self.zshrcBlock + "\n"
        }
        let lines = splitLines(existing)
        switch parseZshrcBlock(lines) {
        case .none:
            var output = existing
            if !output.hasSuffix("\n") { output += "\n\n" }
            else if !output.hasSuffix("\n\n") { output += "\n" }
            return output + Self.zshrcBlock + "\n"
        case .valid(let startIndex, let endIndex):
            let current = lines[startIndex...endIndex].map(normalizedLine)
            if current == blockLines { return nil }
            var updated = lines
            updated.replaceSubrange(startIndex...endIndex, with: blockLines)
            return updated.joined(separator: "\n") + (existing.hasSuffix("\n") ? "\n" : "")
        case .malformed:
            throw AppError.message("~/.zshrc 中的 Codexpress 环境变量标记区块不完整，请手动清理后重试。")
        }
    }

    private func removedZshrcContent(existing: String) throws -> String? {
        let lines = splitLines(existing)
        switch parseZshrcBlock(lines) {
        case .none:
            return nil
        case .malformed:
            throw AppError.message("~/.zshrc 中的 Codexpress 环境变量标记区块不完整，请手动清理。")
        case .valid(let startIndex, let endIndex):
            var updated = lines
            var removeStart = startIndex
            var removeEnd = endIndex
            let hasLeading = removeStart > 0
            let hasTrailing = removeEnd + 1 < updated.count
            if !hasTrailing {
                while removeStart > 0 && normalizedLine(updated[removeStart - 1]).isEmpty {
                    removeStart -= 1
                }
            } else if !hasLeading {
                while removeEnd + 1 < updated.count && normalizedLine(updated[removeEnd + 1]).isEmpty {
                    removeEnd += 1
                }
            } else if normalizedLine(updated[removeStart - 1]).isEmpty && normalizedLine(updated[removeEnd + 1]).isEmpty {
                removeStart -= 1
            }
            updated.removeSubrange(removeStart...removeEnd)
            if updated.isEmpty { return "" }
            return updated.joined(separator: "\n") + (existing.hasSuffix("\n") || !hasTrailing ? "\n" : "")
        }
    }

    private func writeAtomically(_ data: Data, to url: URL, temporaryPrefix: String, permissions: Int, failureMessage: String) throws {
        let manager = FileManager.default
        let temp = url.deletingLastPathComponent().appendingPathComponent("\(temporaryPrefix)\(UUID().uuidString)")
        defer { try? manager.removeItem(at: temp) }
        guard manager.createFile(atPath: temp.path, contents: data, attributes: [.posixPermissions: permissions]),
              rename(temp.path, url.path) == 0 else {
            throw AppError.message(failureMessage)
        }
    }
}
