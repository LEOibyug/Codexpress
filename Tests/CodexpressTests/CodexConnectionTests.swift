import XCTest
@testable import Codexpress

@MainActor private final class FakeCodexApplication: CodexApplicationControlling {
    var events: [String] = []
    var quitFails = false
    var launchFails = false
    var installed = true
    func applicationURL() throws -> URL {
        events.append("resolve")
        guard installed else { throw AppError.message("missing app") }
        return URL(fileURLWithPath: "/temporary/Codex.app")
    }
    func quit() async throws {
        events.append("quit")
        if quitFails { throw AppError.message("quit declined") }
    }
    func launch(at url: URL, codexHome: String, target: CodexConnectionTarget) async throws {
        events.append("launch")
        XCTAssertEqual(url.path, "/temporary/Codex.app")
        XCTAssertEqual(codexHome, "/temporary/codex")
        if launchFails { throw AppError.message("launch failed") }
    }
}

final class CodexConnectionTests: XCTestCase {
    func testNativeLaunchOverridesInheritedCustomURLsKeysAndDaemon() {
        let inherited = ["CODEX_APP_SERVER_OPENAI_BASE_URL":"https://custom.example/v1", "OPENAI_API_KEY":"PRIVATE-OLD-KEY", "CODEX_APP_SERVER_WS_URL":"ws://old-daemon", "HTTPS_PROXY":"http://127.0.0.1:7890"]
        let official = inherited.merging(CodexLaunchPolicy.environment(codexHome: "/temporary/codex", target: .official)) { _, new in new }
        XCTAssertEqual(official["CODEX_APP_SERVER_OPENAI_BASE_URL"], "https://chatgpt.com/backend-api/codex")
        XCTAssertEqual(official["CODEX_APP_SERVER_CHATGPT_BASE_URL"], "https://chatgpt.com/backend-api/")
        XCTAssertEqual(official["OPENAI_API_KEY"], "")
        XCTAssertEqual(official["CODEX_APP_SERVER_WS_URL"], "")
        XCTAssertEqual(official["CODEX_APP_SERVER_FORCE_CLI"], "1")
        XCTAssertEqual(official["HTTPS_PROXY"], inherited["HTTPS_PROXY"])
        let gateway = CodexLaunchPolicy.environment(codexHome: "/temporary/codex", target: .gateway)
        XCTAssertEqual(gateway["CODEX_APP_SERVER_OPENAI_BASE_URL"], "")
    }
    @MainActor func testSwitchChecksThenQuitsBeforeWritingAndReopens() async throws {
        let app = FakeCodexApplication()
        try await CodexConnectionTransition.run(controller: app, codexHome: "/temporary/codex", preflight: {
            app.events.append("preflight")
        }, apply: { app.events.append("apply") }, phase: { _ in })
        XCTAssertEqual(app.events, ["resolve", "preflight", "quit", "apply", "launch"])
    }

    @MainActor func testMissingAppConflictAndRefusedQuitNeverWriteConfiguration() async {
        for failure in ["missing", "conflict", "quit"] {
            let app = FakeCodexApplication(); app.installed = failure != "missing"; app.quitFails = failure == "quit"
            do {
                try await CodexConnectionTransition.run(controller: app, codexHome: "/temporary/codex", preflight: {
                    app.events.append("preflight")
                    if failure == "conflict" { throw AppError.message("conflict") }
                }, apply: { app.events.append("apply") }, phase: { _ in })
                XCTFail("failure ignored")
            } catch { }
            XCTAssertFalse(app.events.contains("apply"))
            XCTAssertFalse(app.events.contains("launch"))
            if failure != "quit" { XCTAssertFalse(app.events.contains("quit")) }
        }
    }

    @MainActor func testWriteFailureReopensAndLaunchFailureReportsAppliedConnection() async {
        let app = FakeCodexApplication()
        do {
            try await CodexConnectionTransition.run(controller: app, codexHome: "/temporary/codex", preflight: {}, apply: {
                app.events.append("apply"); throw AppError.message("write failed")
            }, phase: { _ in })
            XCTFail("failure ignored")
        } catch { XCTAssertEqual(error.localizedDescription, "write failed") }
        XCTAssertEqual(app.events, ["resolve", "quit", "apply", "launch"])
        app.events = []; app.launchFails = true
        do {
            try await CodexConnectionTransition.run(controller: app, codexHome: "/temporary/codex", preflight: {}, apply: {}, phase: { _ in })
            XCTFail("failure ignored")
        } catch { XCTAssertTrue(error.localizedDescription.contains("连接设置已更新")) }
    }
}
