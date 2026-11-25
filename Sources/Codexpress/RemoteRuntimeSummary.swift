import SwiftUI

/// Old servers may return the pool without any live counters.
struct RemoteRuntimeSummary: View {
    private struct Snapshot: Decodable {
        let inflight: Int?
        let sources: [Row]
    }
    private struct Row: Identifiable, Decodable {
        let id: String
        let name: String
        let health: String?
        let active_requests: Int?

        var healthLabel: String? {
            switch health {
            case "ready": "可用"
            case "cooldown": "冷却中"
            case "exhausted": "额度耗尽"
            case "auth_required": "需要凭证"
            case "proxy_error": "代理异常"
            default: nil
            }
        }
    }
    private let snapshot: Snapshot?

    init(data: Data?) {
        snapshot = data.flatMap { try? JSONDecoder().decode(Snapshot.self, from: $0) }
    }

    @ViewBuilder var body: some View {
        if let snapshot {
            SettingsCard("网关运行状态", symbol: "waveform.path.ecg") {
                if let count = snapshot.inflight { LabeledContent("进行中的请求", value: "\(count)") }
                ForEach(snapshot.sources) { source in
                    if source.healthLabel != nil || source.active_requests != nil {
                        LabeledContent(source.name) {
                            if let label = source.healthLabel { Text(label) }
                            if let active = source.active_requests { Text("\(active) 个请求").foregroundStyle(.secondary) }
                        }
                    }
                }
            }
        }
    }
}
