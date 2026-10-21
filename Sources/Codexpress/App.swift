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

    }
}
@MainActor final class AppDelegate: NSObject, NSApplicationDelegate {
    static weak var current: AppDelegate?
    weak var model: AppModel?
    private var settingsWindow: NSWindow?
    func showSettings() {
        if settingsWindow == nil {
            let content = SettingsView().environmentObject(AppModel.shared)
            let window = NSWindow(contentViewController: NSHostingController(rootView: content))
            window.title = "Codexpress 设置"
            window.styleMask = [.titled, .closable, .miniaturizable, .resizable]
            window.titlebarAppearsTransparent = true
            window.titleVisibility = .hidden
            window.setContentSize(NSSize(width: 960, height: 780))
            window.minSize = NSSize(width: 860, height: 650)
            window.isReleasedWhenClosed = false
            window.center()
            settingsWindow = window
        }
        settingsWindow?.makeKeyAndOrderFront(nil)
        AppModel.shared.accountPageOpened()
        NSApp.activate(ignoringOtherApps: true)
    }
    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
        Self.current = self
        model = AppModel.shared
        if model?.sources.isEmpty == true { showSettings() }
        Task { await model?.start() }
    }
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        Task { await model?.shutdown(); NSApp.reply(toApplicationShouldTerminate: true) }
        return .terminateLater
    }
    func applicationWillTerminate(_ notification: Notification) { model?.stop() }
}
