import SwiftUI
import AppKit

struct OMPSettingsView: View {
    @EnvironmentObject var model: AppModel
    @State private var installation: OMPInstallation?
    @State private var detecting = false
    @State private var installing = false
    @State private var installed = false
    @State private var error: String?

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 20) {
                PageHeading("本机 OMP", subtitle: "安装 Codexpress 扩展，在 OMP 中使用来源池模型、绘图与用量查询。")
                SettingsCard("扩展安装", symbol: "puzzlepiece.extension", accessory: {
                    if detecting || installing {
                        ProgressView().controlSize(.small).accessibilityLabel(detecting ? "正在检测 OMP" : "正在安装扩展")
                    } else if installed {
                        StatusPill(text: "本次已安装", color: .green)
                    } else if installation?.executable != nil {
                        StatusPill(text: "已找到 OMP", color: .green)
                    } else {
                        StatusPill(text: "未检测到 OMP")
                    }
                }) {
                    if let executable = installation?.executable {
                        if let version = installation?.version { LabeledContent("OMP 版本", value: version) }
                        LabeledContent("程序") {
                            Text(executable).font(AppTypography.small).foregroundStyle(.secondary)
                                .lineLimit(1).truncationMode(.middle).textSelection(.enabled).help(executable)
                        }
                    } else if detecting {
                        Text("正在查找本机 OMP…").foregroundStyle(.secondary)
                    } else {
                        InfoNote(text: "请先安装官方版 OMP。如果已经安装，可手动选择可执行文件。")
                    }
                    if let error { InfoNote(text: error, symbol: "exclamationmark.triangle", color: .orange) }
                    HStack(spacing: 12) {
                        Button("选择 OMP 程序…") { chooseExecutable() }.disabled(installing || detecting)
                        Button("重新检测") { Task { await detect() } }.disabled(installing || detecting)
                        Spacer(minLength: 0)
                        Button(installing ? "正在安装…" : installed ? "重新安装扩展" : "安装扩展") { install() }
                            .buttonStyle(.borderedProminent)
                            .disabled(installing || detecting || installation?.executable == nil)
                    }
                }
                SettingsCard("安装后登录", symbol: "link") {
                    if !model.running { InfoNote(text: "登录前请在设置中启动本机网关。") }
                    Text("重新启动 OMP，在 /login 中选择 Codexpress，或运行：")
                    Text("omp login codexpress")
                        .font(.system(size: 13, design: .monospaced)).textSelection(.enabled)
                    Text("登录后通过 /model 选择来源池模型；绘图工具在 OMP 设置中启用。")
                        .foregroundStyle(.secondary)
                }
            }.settingsPage()
        }
        .task { await detect() }
    }

    private func detect(candidates: [String]? = nil) async {
        guard !installing, !detecting else { return }
        detecting = true
        defer { detecting = false }
        let found = await OMPInstallation.detect(candidatePaths: candidates)
        guard !Task.isCancelled else { return }
        if found.executable != installation?.executable { installed = false }
        error = nil
        installation = found
    }

    private func chooseExecutable() {
        let picker = NSOpenPanel()
        picker.canChooseFiles = true; picker.canChooseDirectories = false
        picker.allowsMultipleSelection = false; picker.showsHiddenFiles = true
        picker.message = "选择 OMP 可执行文件"
        if picker.runModal() == .OK, let selected = picker.url {
            Task { await detect(candidates: [selected.path]) }
        }
    }

    private func install() {
        guard let installation, !installing else { return }
        guard let package = Bundle.main.url(forResource: "omp-extension", withExtension: "tgz") else {
            error = "内置扩展包缺失，请重新安装 Codexpress。"
            return
        }
        installing = true; error = nil
        Task {
            defer { installing = false }
            do {
                try await installation.installExtension(packageURL: package)
                installed = true
                model.notice = "扩展已安装。重新启动 OMP，在 /login 中选择 Codexpress。"
            } catch { self.error = error.localizedDescription }
        }
    }
}
