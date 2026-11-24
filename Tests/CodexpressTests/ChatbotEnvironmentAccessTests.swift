import XCTest
@testable import Codexpress

final class ChatbotEnvironmentAccessTests: XCTestCase {
    private func makeHome() throws -> URL {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("cx-env-test-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    private final class RecordedLaunchctl: @unchecked Sendable {
        var sets: [(String, String)] = []
        var unsets: [String] = []
        var failOnSet = false

        var environment: LaunchctlEnvironment {
            LaunchctlEnvironment(
                setValue: { [self] name, value in
                    if failOnSet { throw AppError.message("launchctl mock failure") }
                    sets.append((name, value))
                },
                unsetValue: { [self] name in
                    unsets.append(name)
                }
            )
        }
    }

    func testApplyCreatesRestrictedFilesIsIdempotentSupportsSpecialCharactersAndCleansUp() throws {
        let home = try makeHome()
        defer { try? FileManager.default.removeItem(at: home) }

        let initialZshrc = "export PATH=\"$HOME/bin:$PATH\"\n# user comment"
        try initialZshrc.write(to: home.appendingPathComponent(".zshrc"), atomically: true, encoding: .utf8)

        let recorder = RecordedLaunchctl()
        let access = ChatbotEnvironmentAccess(homeDirectory: home, launchctl: recorder.environment)
        let key = "cxb-a'b$c`d\\e!f123456"

        try access.apply(baseURL: "http://127.0.0.1:17865/v1", key: key)

        let dirAttrs = try FileManager.default.attributesOfItem(atPath: access.envDirectory.path)
        XCTAssertEqual((dirAttrs[.posixPermissions] as? NSNumber)?.intValue, 0o700)
        let fileAttrs = try FileManager.default.attributesOfItem(atPath: access.envFile.path)
        XCTAssertEqual((fileAttrs[.posixPermissions] as? NSNumber)?.intValue, 0o600)
        XCTAssertTrue(access.inspect().synced)
        XCTAssertEqual(recorder.sets.count, 2)
        XCTAssertEqual(recorder.sets[0].0, ChatbotEnvironmentAccess.baseURLVariable)
        XCTAssertEqual(recorder.sets[0].1, "http://127.0.0.1:17865/v1")
        XCTAssertEqual(recorder.sets[1].0, ChatbotEnvironmentAccess.apiKeyVariable)
        XCTAssertEqual(recorder.sets[1].1, key)

        // Real /bin/zsh sourcing inside an isolated temporary HOME must preserve exact characters.
        let zsh = Process()
        zsh.executableURL = URL(fileURLWithPath: "/bin/zsh")
        zsh.arguments = ["-f", "-c", #"source "$HOME/.zshrc"; printf "%s|%s" "$CODEXPRESS_BASE_URL" "$CODEXPRESS_API_KEY""#]
        var env = ProcessInfo.processInfo.environment
        env["HOME"] = home.path
        env["ZDOTDIR"] = home.path
        zsh.environment = env
        let pipe = Pipe()
        zsh.standardOutput = pipe
        try zsh.run()
        zsh.waitUntilExit()
        XCTAssertEqual(zsh.terminationStatus, 0)
        let evaluated = String(data: pipe.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8)
        XCTAssertEqual(evaluated, "http://127.0.0.1:17865/v1|\(key)")

        // Rotating port and key updates env.sh and launchctl without duplicating the .zshrc block.
        let rotatedKey = "cxb-rotated-9876543210abcdef"
        try access.apply(baseURL: "http://127.0.0.1:18865/v1", key: rotatedKey)
        let zshrcText = try String(contentsOf: access.zshrcFile, encoding: .utf8)
        XCTAssertEqual(zshrcText.components(separatedBy: ChatbotEnvironmentAccess.blockStart).count - 1, 1)
        XCTAssertEqual(zshrcText.components(separatedBy: ChatbotEnvironmentAccess.blockEnd).count - 1, 1)
        let envScript = try String(contentsOf: access.envFile, encoding: .utf8)
        XCTAssertTrue(envScript.contains("http://127.0.0.1:18865/v1"))
        XCTAssertTrue(envScript.contains(rotatedKey))

        // Cleanup removes env.sh, removes the managed block while keeping user lines, and unsets launchctl.
        try access.remove()
        XCTAssertFalse(FileManager.default.fileExists(atPath: access.envFile.path))
        XCTAssertFalse(access.inspect().synced)
        let cleanedZshrc = try String(contentsOf: access.zshrcFile, encoding: .utf8)
        XCTAssertFalse(cleanedZshrc.contains(ChatbotEnvironmentAccess.blockStart))
        XCTAssertFalse(cleanedZshrc.contains("env.sh"))
        XCTAssertEqual(cleanedZshrc, "export PATH=\"$HOME/bin:$PATH\"\n# user comment\n")
        XCTAssertEqual(recorder.unsets, [ChatbotEnvironmentAccess.baseURLVariable, ChatbotEnvironmentAccess.apiKeyVariable])
    }

    func testSymlinkSafetyAndMalformedMarkerProtection() throws {
        let home = try makeHome()
        defer { try? FileManager.default.removeItem(at: home) }

        let manager = FileManager.default
        let access = ChatbotEnvironmentAccess(homeDirectory: home, launchctl: .noop)

        // Reject symlink env.sh before writing.
        try manager.createDirectory(at: access.envDirectory, withIntermediateDirectories: true)
        let decoy = home.appendingPathComponent("decoy")
        try Data("decoy".utf8).write(to: decoy)
        try manager.createSymbolicLink(at: access.envFile, withDestinationURL: decoy)
        XCTAssertThrowsError(try access.apply(baseURL: "http://127.0.0.1:17865/v1", key: "cxb-validkey-1234567890"))
        try manager.removeItem(at: access.envFile)

        // Preserve symlink ~/.zshrc when it targets a regular file (dotfiles setup).
        let dotfilesDir = home.appendingPathComponent("dotfiles", isDirectory: true)
        try manager.createDirectory(at: dotfilesDir, withIntermediateDirectories: true)
        let realZshrc = dotfilesDir.appendingPathComponent("zshrc")
        try "alias ll='ls -la'\n".write(to: realZshrc, atomically: true, encoding: .utf8)
        try manager.createSymbolicLink(at: access.zshrcFile, withDestinationURL: realZshrc)

        try access.apply(baseURL: "http://127.0.0.1:17865/v1", key: "cxb-validkey-1234567890")
        let linkAttrs = try manager.attributesOfItem(atPath: access.zshrcFile.path)
        XCTAssertEqual(linkAttrs[.type] as? FileAttributeType, .typeSymbolicLink)
        let targetContent = try String(contentsOf: realZshrc, encoding: .utf8)
        XCTAssertTrue(targetContent.contains(ChatbotEnvironmentAccess.blockStart))
        XCTAssertTrue(targetContent.contains("alias ll='ls -la'"))

        // Malformed marker in .zshrc: apply refuses without editing; remove still deletes env.sh and unsets launchctl.
        let malformed = "alias ll='ls -la'\n\(ChatbotEnvironmentAccess.blockStart)\nbroken\n"
        try malformed.write(to: realZshrc, atomically: true, encoding: .utf8)
        XCTAssertTrue(access.inspect().zshrcMalformed)
        XCTAssertThrowsError(try access.apply(baseURL: "http://127.0.0.1:17866/v1", key: "cxb-validkey-0987654321"))
        XCTAssertEqual(try String(contentsOf: realZshrc, encoding: .utf8), malformed)

        let recorder = RecordedLaunchctl()
        let removeAccess = ChatbotEnvironmentAccess(homeDirectory: home, launchctl: recorder.environment)
        XCTAssertThrowsError(try removeAccess.remove())
        XCTAssertFalse(manager.fileExists(atPath: removeAccess.envFile.path))
        XCTAssertEqual(recorder.unsets, [ChatbotEnvironmentAccess.baseURLVariable, ChatbotEnvironmentAccess.apiKeyVariable])
        XCTAssertEqual(try String(contentsOf: realZshrc, encoding: .utf8), malformed)
    }

    @MainActor func testAppModelAutoExportChatbotEnvironmentLifecycle() async throws {
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let directory = root.appendingPathComponent(".runtime/env-\(UUID().uuidString.prefix(8))", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }

        let helper = directory.appendingPathComponent("gateway-fixture")
        let script = #"""
        #!/usr/bin/env python3
        import json, os, pathlib, socket, sys
        directory = pathlib.Path(sys.argv[sys.argv.index('--state-dir') + 1])
        server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        server.bind(str(directory / 'control.sock'))
        server.listen()
        while True:
            conn, _ = server.accept()
            with conn:
                req = json.loads(conn.makefile('rb').readline())
                op = req['op']
                if op == 'status':
                    data = dict(process_id=os.getpid(), port=17863, sources=[], connection_mode='unmanaged')
                elif op == 'gateway.update.info':
                    data = dict(protocol=1, version='0.8.26')
                elif op == 'credentials.storage.status':
                    data = dict(mode='file')
                elif op == 'chatbot.status':
                    data = dict(enabled=True, running=True, port=17899, base_url='http://127.0.0.1:17899/v1')
                elif op == 'chatbot.key':
                    data = dict(key='cxb-fixture-chatbot-key-123456')
                elif op in ('credentials', 'sync', 'history.configure', 'models.info', 'notifications.list'):
                    data = dict(updated_secrets=[], items=[], models=[], events=[])
                else:
                    data = {}
                conn.sendall((json.dumps(dict(id=req['id'], ok=True, data=data)) + '\n').encode())
        """#
        try script.write(to: helper, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: helper.path)
        let previousHelper = ProcessInfo.processInfo.environment["CODEXPRESS_GATEWAY_PATH"]
        setenv("CODEXPRESS_GATEWAY_PATH", helper.path, 1)
        defer {
            if let previousHelper { setenv("CODEXPRESS_GATEWAY_PATH", previousHelper, 1) }
            else { unsetenv("CODEXPRESS_GATEWAY_PATH") }
        }

        let envHome = directory.appendingPathComponent("home", isDirectory: true)
        try FileManager.default.createDirectory(at: envHome, withIntermediateDirectories: true)
        let recorder = RecordedLaunchctl()
        let access = ChatbotEnvironmentAccess(homeDirectory: envHome, launchctl: recorder.environment)
        var settings = AppSettings()
        settings.codexHome = directory.appendingPathComponent("codex").path
        try MetadataStore(directory: directory).save(settings)

        let model = AppModel(stateDirectory: directory, environmentAccess: access)
        defer { model.stop() }
        await model.startHelper()
        XCTAssertTrue(model.running, model.error ?? "Helper failed to start")
        XCTAssertFalse(model.settings.autoExportChatbotEnvironmentEnabled)
        XCTAssertFalse(model.environmentSynced)

        model.setAutoExportChatbotEnvironment(true)
        let enableDeadline = Date().addingTimeInterval(3)
        while !model.environmentSynced && Date() < enableDeadline {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertTrue(model.environmentSynced)
        XCTAssertNil(model.environmentSyncError)
        XCTAssertTrue(access.inspect().synced)
        XCTAssertTrue(try String(contentsOf: access.envFile, encoding: .utf8).contains("http://127.0.0.1:17899/v1"))
        XCTAssertEqual(recorder.sets.last?.1, "cxb-fixture-chatbot-key-123456")

        model.setAutoExportChatbotEnvironment(false)
        let disableDeadline = Date().addingTimeInterval(3)
        while model.environmentSynced && Date() < disableDeadline {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertFalse(model.environmentSynced)
        XCTAssertFalse(access.inspect().synced)
        XCTAssertEqual(recorder.unsets.suffix(2), [ChatbotEnvironmentAccess.baseURLVariable, ChatbotEnvironmentAccess.apiKeyVariable])
    }
}
