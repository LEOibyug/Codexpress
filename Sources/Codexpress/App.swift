import SwiftUI
import AppKit

@main struct CodexpressApp: App {
    @StateObject private var model = AppModel.shared
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    var body: some Scene {
        MenuBarExtra {
            MenuView().environmentObject(model).task { delegate.model = model; await model.start() }
        } label: {
            Image(systemName: model.running ? "arrow.triangle.branch" : "exclamationmark.circle")
                .accessibilityLabel("Codexpress")
        }
        .menuBarExtraStyle(.window)
        Window("Codexpress 设置", id: "settings") {
            SettingsView().environmentObject(model).task { delegate.model = model; await model.start() }
        }
        .defaultSize(width: 860, height: 620)
        .windowResizability(.contentMinSize)
    }
}
@MainActor final class AppDelegate: NSObject, NSApplicationDelegate {
    weak var model: AppModel?
    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
        model = AppModel.shared
        Task { await model?.start() }
    }
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        Task { await model?.shutdown(); NSApp.reply(toApplicationShouldTerminate: true) }
        return .terminateLater
    }
    func applicationWillTerminate(_ notification: Notification) { model?.stop() }
}
