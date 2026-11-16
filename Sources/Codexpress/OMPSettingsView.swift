import SwiftUI
import AppKit

struct OMPSettingsView: View {
    @EnvironmentObject var model: AppModel
    @State private var installation: OMPInstallation?
    @State private var detecting = false
    @State private var installing = false
    @State private var error: String?

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 20) {
                PageHeading("OMP") {
                    if detecting { ProgressView().controlSize(.small) }
                    else {
                        Button { Task { await detect() } } label: { Image(systemName: "arrow.clockwise") }
                            .help("重新检测 OMP").accessibilityLabel("重新检测 OMP").disabled(installing)
                    }
                }
                SettingsCard("Codexpress 扩展", symbol: "puzzlepiece.extension") {
                    Text("将 Codexpress 添加为 OMP 供应商，使用来源池模型、原生绘图与用量查询。")
                    SettingRow(title: "OMP 版本") {
                        Text(installation?.version ?? (installation?.executable == nil ? "未检测到" : "已安装"))
                            .foregroundStyle(.secondary)
                    }
                    if let executable = installation?.executable {
                        LabeledContent("程序") {
                            Text(executable).font(AppTypography.small).foregroundStyle(.secondary)
                                .lineLimit(1).truncationMode(.middle).textSelection(.enabled).help(executable)
                        }
                    } else {
                        Text("请先安装官方版 OMP，或选择已安装的 OMP 程序。")
                            .foregroundStyle(.secondary)
                    }
                    if let error { InfoNote(text: error, symbol: "exclamationmark.triangle", color: .orange) }
                    HStack(spacing: 12) {
                        Button("选择 OMP 程序…") { chooseExecutable() }.disabled(installing || detecting)
                        Spacer()
                        if installing { ProgressView().controlSize(.small) }
                        Button(installing ? "正在安装…" : "安装扩展") { install() }
                            .buttonStyle(.borderedProminent)
                            .disabled(installing || detecting || installation?.executable == nil)
                    }
                }
                SettingsCard("连接 Codexpress", symbol: "link") {
                    Text("安装后重新启动 OMP，在 /login 中选择 Codexpress，或执行：")
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
                model.notice = "扩展已安装。重新启动 OMP，在 /login 中选择 Codexpress。"
            } catch { self.error = error.localizedDescription }
        }
    }
}
