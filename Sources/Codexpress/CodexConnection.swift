import AppKit
import SwiftUI

@MainActor protocol CodexApplicationControlling {
    func applicationURL() throws -> URL
    func quit() async throws
    func launch(at url: URL, codexHome: String, target: CodexConnectionTarget) async throws
}

@MainActor struct NativeCodexApplication: CodexApplicationControlling {
    private let bundleID = "com.openai.codex"

    func applicationURL() throws -> URL {
        guard let url = NSWorkspace.shared.urlForApplication(withBundleIdentifier: bundleID) else {
            throw AppError.message("未找到 Codex 应用，请先安装并打开一次 Codex。")
        }
        return url
    }

    func quit() async throws {
        for app in NSRunningApplication.runningApplications(withBundleIdentifier: bundleID) where !app.isTerminated {
            guard app.terminate() else { throw AppError.message("Codex 未能正常退出。请在 Codex 中结束当前操作后重试；连接设置未更改。") }
        }
        // Let Codex save its own state and respect any quit/save prompt. Never
        // kill the process or write connection settings while it is still live.
        for _ in 0..<300 {
            if NSRunningApplication.runningApplications(withBundleIdentifier: bundleID).allSatisfy(\.isTerminated) { return }
            try await Task.sleep(for: .milliseconds(100))
        }
        throw AppError.message("Codex 仍在运行，请处理退出提示后重试；连接设置未更改。")
    }

    func launch(at url: URL, codexHome: String, target: CodexConnectionTarget) async throws {
        let configuration = NSWorkspace.OpenConfiguration()
        configuration.activates = true
        // Launch Services supplies the normal desktop environment and system
        // network settings. No source proxy or upstream credentials are injected.
        configuration.environment = CodexLaunchPolicy.environment(codexHome: codexHome, target: target)
        _ = try await NSWorkspace.shared.openApplication(at: url, configuration: configuration)
    }
}

enum CodexConnectionTarget { case official, gateway, restored }

enum CodexLaunchPolicy {
    static let clearedVariables = ["OPENAI_API_KEY", "OPENAI_BASE_URL", "OPENAI_API_BASE", "CODEX_API_KEY", "CODEX_ACCESS_TOKEN", "CODEX_AUTH_JSON", "CHATGPT_BASE_URL", "CODEX_APP_SERVER_WS_URL", "CODEX_APP_SERVER_USE_LOCAL_DAEMON", "CODEX_APP_SERVER_LOGIN_ISSUER", "CODEX_APP_SERVER_LOGIN_CLIENT_ID", "CODEX_REFRESH_TOKEN_URL_OVERRIDE", "CODEX_REVOKE_TOKEN_URL_OVERRIDE", "CODEX_CLI_PATH"]
    static func environment(codexHome: String, target: CodexConnectionTarget) -> [String: String] {
        // Explicit empty values override Launch Services' inherited environment.
        // Merely omitting a key would retain a preexisting custom URL/API key.
        var env = Dictionary(uniqueKeysWithValues: clearedVariables.map { ($0, "") })
        env["CODEX_HOME"] = codexHome
        env["CODEX_APP_SERVER_FORCE_CLI"] = "1"
        env["CODEX_APP_SERVER_OPENAI_BASE_URL"] = target == .official ? "https://chatgpt.com/backend-api/codex" : ""
        env["CODEX_APP_SERVER_CHATGPT_BASE_URL"] = target == .official ? "https://chatgpt.com/backend-api/" : ""
        return env
    }
}

@MainActor enum CodexConnectionTransition {
    static func run(
        controller: any CodexApplicationControlling,
        codexHome: String,
        target: CodexConnectionTarget = .gateway,
        preflight: () async throws -> Void,
        apply: () async throws -> Void,
        phase: (String) -> Void
    ) async throws {
        let applicationURL = try controller.applicationURL()
        phase("检查连接设置…")
        try await preflight()
        phase("等待 Codex 退出…")
        try await controller.quit()
        phase("更新连接设置…")
        do { try await apply() }
        catch {
            // A failed configuration write must not strand an app that we quit.
            try? await controller.launch(at: applicationURL, codexHome: codexHome, target: .restored)
            throw error
        }
        phase("正在启动 Codex…")
        do { try await controller.launch(at: applicationURL, codexHome: codexHome, target: target) }
        catch { throw AppError.message("连接设置已更新，但 Codex 未能启动，请手动打开 Codex。") }
    }
}

struct DirectOfficialLoginButton: View {
    @EnvironmentObject var model: AppModel
    var body: some View {
        Button { model.switchCodexConnection(.official) } label: {
            Label(model.codexConnectionPhase ?? "直接官方登录", systemImage: "person.crop.circle")
        }
        .disabled(model.busy || !model.running || model.metadataLoadFailed)
        .help("重启 Codex，使用原生 ChatGPT 登录和系统代理")
    }
}
