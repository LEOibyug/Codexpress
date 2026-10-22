import SwiftUI

struct SourceCapacityLabel: View {
    let source: Source
    let admission: JSONValue
    var showsRoutingNote = false

    private var active: Int? { admission["capacity_active_conversations"].number.map { Int($0) } }
    private var limit: Int? {
        active != nil ? admission["max_active_conversations"].number.map { Int($0) } : source.max_active_conversations
    }
    private var isFull: Bool { limit != nil && admission["capacity_available"].bool == false }
    private var label: String? {
        let capacity: String?
        if let active {
            capacity = limit.map { "对话 \(active)/\($0)" + (isFull ? " · 已满" : "") } ?? "活跃 \(active) · 不限"
        } else { capacity = limit.map { "对话上限 \($0)" } }
        guard let capacity else { return nil }
        guard showsRoutingNote, source.official, !isFull,
              let note = admission["routing_note"].string, !note.isEmpty else { return capacity }
        return capacity + " · " + note
    }
    private var help: String {
        let seconds = admission["activity_window_seconds"].number.map { Int($0) } ?? source.activeConversationWindowSeconds
        return "进行中的请求持续占用名额；结束后闲置 \(seconds) 秒释放，同一对话使用同一模型时复用名额。"
    }

    var body: some View {
        if let label {
            Text(label).monospacedDigit().foregroundStyle(isFull ? Color.orange : Color.secondary)
                .lineLimit(1).help(help)
        }
    }
}
