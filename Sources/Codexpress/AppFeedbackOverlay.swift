import SwiftUI
import AppKit

private struct FeedbackMessage: Identifiable {
    let id = UUID()
    let text: String
    let isError: Bool
}

struct AppFeedbackOverlay: View {
    @EnvironmentObject var model: AppModel
    @State private var messages: [FeedbackMessage] = []

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            ForEach(messages) { message in
                FeedbackCard(message: message) { dismiss(message, explicit: true) }
                    onExpire: { dismiss(message, explicit: false) }
                    .transition(.move(edge: .bottom).combined(with: .opacity))
            }
        }.frame(maxWidth: 380, alignment: .leading).padding(20)
            .animation(.easeInOut(duration: 0.2), value: messages.map(\.id))
            .onChange(of: model.notice, initial: true) { _, text in present(text, isError: false) }
            .onChange(of: model.error, initial: true) { _, text in present(text, isError: true) }
    }

    private func present(_ text: String?, isError: Bool) {
        messages.removeAll { $0.isError == isError }
        guard let text, !text.isEmpty else { return }
        messages.append(FeedbackMessage(text: text, isError: isError))
    }
    private func dismiss(_ message: FeedbackMessage, explicit: Bool) {
        guard messages.contains(where: { $0.id == message.id }) else { return }
        messages.removeAll { $0.id == message.id }
        if !message.isError, model.notice == message.text { model.notice = nil }
        // Keep a persistent fault in the model after the toast times out so
        // identical failures in the local status poll do not repeatedly toast.
        if message.isError, explicit, model.error == message.text { model.error = nil }
    }
}

private struct FeedbackCard: View {
    let message: FeedbackMessage
    let onDismiss: () -> Void
    let onExpire: () -> Void
    @State private var hovering = false

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: message.isError ? "exclamationmark.circle.fill" : "checkmark.circle.fill")
                .font(.system(size: 16)).foregroundStyle(message.isError ? Color.orange : Color.accentColor)
            Text(message.text).font(.system(size: 12)).lineSpacing(3)
                .textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
            Button(action: onDismiss) { Image(systemName: "xmark").font(.system(size: 10, weight: .semibold)) }
                .buttonStyle(.plain).foregroundStyle(.secondary).padding(3).accessibilityLabel("关闭通知")
        }.padding(14).background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
            .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(AppPalette.border))
            .shadow(color: .black.opacity(0.12), radius: 14, y: 5)
            .onHover { hovering = $0 }
            .task(id: hovering) {
                guard !hovering else { return }
                do { try await Task.sleep(for: .seconds(message.isError ? 10 : 5)) } catch { return }
                onExpire()
            }
            .contextMenu {
                Button("复制内容") { NSPasteboard.general.clearContents(); NSPasteboard.general.setString(message.text, forType: .string) }
            }
    }
}
