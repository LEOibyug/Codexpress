import SwiftUI

/// This identity lasts across sidebar navigation, but never across a gateway switch.
struct RemoteSettingsWorkspace: View {
    @EnvironmentObject private var management: RemoteGatewayManagement
    @StateObject private var draft = RemotePreferencesDraft()
    let page: SettingsPage
    @State private var confirmingReload = false
    @State private var confirmingCatalog = false

    var body: some View {
        switch page {
        case .sources: RemoteGatewayPoolView()
        case .general, .models:
            VStack(spacing: 0) {
                if draft.config != nil {
                    Group {
                        if page == .models { RemoteModelSettingsPage(draft: draft) }
                        else { RemoteGeneralSettingsPage(draft: draft) }
                    }.id(draft.editingSession)
                        .disabled(draft.submitting || draft.needsReload || management.snapshot == nil)
                } else if draft.notice == nil && management.message == nil { ProgressView("读取远端设置…").frame(maxHeight: .infinity) }
                if let text = management.message ?? draft.notice {
                    HStack {
                        InfoNote(text: text, color: .orange)
                        Spacer()
                        Button(draft.needsReload ? "重新载入…" : "重试") {
                            if draft.needsReload { confirmingReload = true }
                            else { Task { await refresh(discard: false) } }
                        }.disabled(draft.submitting)
                    }.padding(.horizontal).padding(.vertical, 6)
                }
                if draft.config != nil && (page == .general || !draft.modelEditorOpen) {
                    Divider()
                    HStack {
                        Spacer()
                        Button("提交修改") {
                            if draft.metadataStaged { confirmingCatalog = true }
                            else { Task { await draft.submit(page: page) } }
                        }.buttonStyle(.borderedProminent).disabled(!draft.canSubmit)
                            .alert("提交修改并替换模型能力？", isPresented: $confirmingCatalog) {
                                Button("取消", role: .cancel) {}
                                Button("提交修改", role: .destructive) { Task { await draft.submit(page: page) } }
                            } message: {
                                if let file = draft.metadataFile { Text("将向 \(management.endpoint) 上传 \(file.count) 个模型的能力，替换之前的导入。") }
                                else { Text("将清除 \(management.endpoint) 上导入的模型能力，恢复默认值。") }
                            }
                    }.padding(.horizontal, 22).padding(.vertical, 9)
                }
            }
            .task(id: management.fetchedAt) {
                if let pool = management.snapshot { await draft.load(.init(management, pool), page: page) }
            }
            .id(page)
            .background(WindowOpenRefresh(refresh: { Task { await management.refresh() } }))
            .alert("重新载入服务器设置？", isPresented: $confirmingReload) {
                Button("取消", role: .cancel) {}
                Button("重新载入", role: .destructive) { Task { await refresh(discard: true) } }
            } message: { Text("将放弃此网关尚未提交的模型和设置修改。") }
        default: RemoteGatewayConsole(page: page)
        }
    }

    private func refresh(discard: Bool) async {
        await management.refresh()
        if let pool = management.snapshot { await draft.load(.init(management, pool), page: page, discard: discard) }
    }
}
