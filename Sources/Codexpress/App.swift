import SwiftUI
import AppKit

@main struct CodexpressApp: App {
    @StateObject private var model = AppModel.shared
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    var body: some Scene {
        MenuBarExtra {
            MenuView().environmentObject(model).task { delegate.model = model; await model.start() }
        } label: {
            GatewayMenuIcon(running: model.running, active: model.gatewayHasActivity)
        }
        .menuBarExtraStyle(.window)

    }
}

struct GatewayMenuIcon: View {
    let running: Bool
    let active: Bool
    @Environment(\.colorScheme) private var colorScheme
    private static let cachedImages = NSCache<NSNumber, NSImage>()

    var body: some View {
        Image(nsImage: Self.image(running: running, active: active, dark: colorScheme == .dark))
            .renderingMode(.original)
            .accessibilityLabel(active ? "Codexpress · 正在处理请求" : "Codexpress")
    }

    static func image(running: Bool, active: Bool, dark: Bool) -> NSImage {
        let key = NSNumber(value: (running ? 1 : 0) | (active ? 2 : 0) | (dark ? 4 : 0))
        if let cached = cachedImages.object(forKey: key) { return cached }
        let image = NSImage(size: NSSize(width: 18, height: 18), flipped: false) { _ in
            let symbol = NSImage(systemSymbolName: running ? "arrow.triangle.branch" : "exclamationmark.circle", accessibilityDescription: nil)
            symbol?.draw(in: NSRect(x: 1, y: 1, width: 16, height: 16))
            (dark ? NSColor.white : NSColor.black).setFill()
            NSRect(x: 0, y: 0, width: 18, height: 18).fill(using: .sourceAtop)
            if active {
                NSGraphicsContext.saveGraphicsState()
                NSGraphicsContext.current?.compositingOperation = .copy
                NSColor.clear.setFill()
                NSBezierPath(ovalIn: NSRect(x: 11, y: 0, width: 7, height: 7)).fill()
                NSGraphicsContext.restoreGraphicsState()
                NSColor.systemGreen.setFill()
                NSBezierPath(ovalIn: NSRect(x: 12, y: 1, width: 5, height: 5)).fill()
            }
            return true
        }
        image.isTemplate = false
        cachedImages.setObject(image, forKey: key)
        return image
    }
}

@MainActor final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    static weak var current: AppDelegate?
    weak var model: AppModel?
    private var settingsWindow: NSWindow?
    private let terminationGuard = TerminationGuard()
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
            window.delegate = self
            window.center()
            settingsWindow = window
        }
        settingsWindow?.makeKeyAndOrderFront(nil)
        AppModel.shared.accountPageOpened()
        NSApp.activate(ignoringOtherApps: true)
        updateWindowActivity()
    }
    private func updateWindowActivity() {
        let active = settingsWindow.map { $0.isVisible && !$0.isMiniaturized && $0.occlusionState.contains(.visible) && NSApp.isActive } ?? false
        (model ?? AppModel.shared).setSettingsWindowActive(active)
    }
    func windowDidBecomeKey(_ notification: Notification) { updateWindowActivity() }
    func windowDidResignKey(_ notification: Notification) { updateWindowActivity() }
    func windowDidChangeOcclusionState(_ notification: Notification) { updateWindowActivity() }
    func windowDidMiniaturize(_ notification: Notification) { updateWindowActivity() }
    func windowDidDeminiaturize(_ notification: Notification) { updateWindowActivity() }
    func windowWillClose(_ notification: Notification) { (model ?? AppModel.shared).setSettingsWindowActive(false) }
    func applicationDidBecomeActive(_ notification: Notification) { updateWindowActivity() }
    func applicationDidResignActive(_ notification: Notification) { updateWindowActivity() }
    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
        Self.current = self
        model = AppModel.shared
        if model?.sources.isEmpty == true { showSettings() }
        Task { await model?.start() }
    }
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        let model = model ?? AppModel.shared
        Task {
            let approved = await terminationGuard.request(check: {
                try await model.activeRequestsBeforeTermination()
            }, confirm: { count in
                self.confirmTermination(activeRequests: count)
            }, shutdown: {
                await model.shutdown()
            })
            if let approved { sender.reply(toApplicationShouldTerminate: approved) }
        }
        return .terminateLater
    }
    private func confirmTermination(activeRequests: Int?) -> Bool {
        let alert = NSAlert()
        alert.alertStyle = .warning
        if let activeRequests {
            alert.messageText = "仍有对话正在进行"
            alert.informativeText = "网关正在处理 \(activeRequests) 个请求。退出会停止网关并中断这些请求。"
        } else {
            alert.messageText = "无法确认网关是否空闲"
            alert.informativeText = "暂时无法获取网关状态。退出会停止网关，可能中断正在进行的请求。"
        }
        let cancel = alert.addButton(withTitle: "继续运行")
        cancel.keyEquivalent = "\u{1b}"
        alert.window.defaultButtonCell = cancel.cell as? NSButtonCell
        let quit = alert.addButton(withTitle: "仍然退出")
        quit.keyEquivalent = ""
        quit.hasDestructiveAction = true
        NSApp.activate(ignoringOtherApps: true)
        return alert.runModal() == .alertSecondButtonReturn
    }
    func applicationWillTerminate(_ notification: Notification) { model?.stop() }
}
