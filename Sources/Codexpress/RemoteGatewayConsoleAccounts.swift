import CryptoKit
import SwiftUI

struct RemoteConsoleReceipt: Codable, Identifiable {
    let id: UUID
    let source: String
    let binding: String
    let card: String
    var code: String
    var windows: Int?

    var resultFields: [String: JSONValue] {
        ["source_id": .string(source), "account_binding": .string(binding), "operation_id": .string(id.uuidString)]
    }
}

/// Persist intent *before* the one consuming POST. Reopening a sheet or restarting the app only offers result lookup.
@MainActor struct RemoteConsoleReceiptBook {
    let file: RemoteManagementFile
    init(directory: URL, endpoint: String, instance: String) {
        let hash = SHA256.hash(data: Data((endpoint + "\0" + instance).utf8)).map { String(format: "%02x", $0) }.joined()
        file = RemoteManagementFile(directory: directory, name: "remote-reset-\(hash).json")
    }
    func read() throws -> [RemoteConsoleReceipt] {
        do { return try JSONDecoder().decode([RemoteConsoleReceipt].self, from: file.read()) }
        catch let e as CocoaError where e.code == .fileReadNoSuchFile { return [] }
    }
    func reserve(source: String, binding: String, card: String) throws -> RemoteConsoleReceipt {
        var records = try read()
        guard !source.isEmpty, !binding.isEmpty, !card.isEmpty, !records.contains(where: { $0.binding == binding && $0.card == card }) else { throw RemoteConsoleError.invalid }
        let record = RemoteConsoleReceipt(id: UUID(), source: source, binding: binding, card: card, code: "pending_verification")
        records.append(record); try file.replace(with: JSONEncoder().encode(records)); return record
    }
    func update(_ record: RemoteConsoleReceipt, result: JSONValue) throws {
        var records = try read()
        guard let index = records.firstIndex(where: { $0.id == record.id }), let code = result["code"].string else { throw RemoteConsoleError.invalid }
        records[index].code = ["reset", "nothing_to_reset", "no_credit", "already_redeemed", "not_found", "pending_verification"].contains(code) ? code : "unknown"
        records[index].windows = result["windows_reset"].number.flatMap { Int(exactly: $0) }
        try file.replace(with: JSONEncoder().encode(records))
    }
}

struct RemoteConsoleAccountSession: Identifiable {
    let id = UUID()
    let target: RemoteConsoleTarget
    let source: RemotePoolSource
}

struct RemoteConsoleAccount: View {
    @EnvironmentObject private var model: AppModel
    @EnvironmentObject private var management: RemoteGatewayManagement
    @Environment(\.dismiss) private var dismiss
    let session: RemoteConsoleAccountSession
    @State private var value: JSONValue = .null
    @State private var records: [RemoteConsoleReceipt] = []
    @State private var confirmation: RemoteCardSelection?
    @State private var busy = false
    @State private var failed = false
    @State private var journalAvailable = false

    private var book: RemoteConsoleReceiptBook { .init(directory: model.directory, endpoint: session.target.endpoint, instance: session.target.pool.instanceID) }
    private var inventory: CreditInventory { CreditInventory(value["credits"]) }
    private var binding: String? { value["account_binding"].string }
    private var sourceFields: [String: JSONValue] { ["source_id": .string(session.source.id)] }
    private var official: Bool { session.source.kind == "official" }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(session.source.name).font(.title2)
            Text(session.target.endpoint).foregroundStyle(.secondary)
            if failed { InfoNote(text: "无法确认操作。已提交的重置卡只可查询结果，请勿重新使用。", color: .orange) }
            if !session.target.current { InfoNote(text: "来源配置已改变。请关闭并重新选择。", color: .orange) }
            ScrollView {
                VStack(alignment: .leading, spacing: 12) {
                    if official { officialAccount } else { balance }
                }
            }
            HStack {
                Button(official ? "刷新额度和卡片" : "查询上游余额") { Task { await load(force: true) } }.disabled(busy || !session.target.current)
                Spacer(); Button("关闭") { dismiss() }.keyboardShortcut(.cancelAction)
            }
        }.padding(22).frame(width: 550, height: 550)
        .task(id: management.fetchedAt) { await load(force: false) }
        .alert("确认使用这张重置卡？", isPresented: Binding(get: { confirmation != nil }, set: { if !$0 { confirmation = nil } }), presenting: confirmation) { selected in
            Button("取消", role: .cancel) {}; Button("确认消耗一张", role: .destructive) { redeem(selected) }
        } message: { selected in
            Text("\(session.target.endpoint) · \(session.source.name)\n账号标识：\(selected.binding.prefix(12))\n卡片：\(selected.id)\n到期：\(timestampLabel(selected.card["expires_at"]))\n此操作无法撤销。")
        }
    }

    private var balance: some View {
        SettingsCard("账户余额", symbol: "creditcard") {
            if let amount = value["amount"].number, let currency = value["currency"].string {
                LabeledContent(value["estimated"].bool == true ? "估算余额" : "余额", value: balanceMoney(amount, currency: currency))
            } else { Text("暂无可用余额").foregroundStyle(.secondary) }
            if let time = value["updated_at"].number { LabeledContent("更新于", value: timestampLabel(.number(time))) }
            if value["stale"].bool == true { Text("余额待更新").foregroundStyle(.orange) }
        }
    }

    @ViewBuilder private var officialAccount: some View {
        SettingsCard("官方额度", symbol: "gauge.with.dots.needle.50percent") {
            ForEach(QuotaOverview(value["quota"]).windows) { window in
                LabeledContent(window.title, value: "剩余 \(window.remainingLabel) · \(window.resetLabel)")
            }
            if value["quota"]["stale"].bool == true { Text("额度待更新").foregroundStyle(.orange) }
        }
        SettingsCard("重置卡", symbol: "ticket") {
            LabeledContent("可用卡片", value: inventory.countLabel)
            if !journalAvailable { Text("未能读取本机操作记录，不允许提交。").foregroundStyle(.secondary) }
            if !inventory.isFresh { Text("卡片状态待更新").foregroundStyle(.secondary) }
            ForEach(Array(inventory.cards.enumerated()), id: \.offset) { _, card in
                if let id = card["id"].string {
                    HStack {
                        VStack(alignment: .leading) {
                            Text(id).textSelection(.enabled)
                            Text("到期 \(timestampLabel(card["expires_at"]))").font(.caption).foregroundStyle(.secondary)
                        }
                        Spacer()
                        Button("使用…") { if let binding { confirmation = RemoteCardSelection(id: id, binding: binding, card: card) } }
                            .disabled(!session.target.current || busy || !journalAvailable || binding == nil || !inventory.isFresh || !CreditInventory.isUsable(card) || records.contains { $0.binding == binding && $0.card == id })
                    }
                }
            }
        }
        let matching = records.filter { $0.source == session.source.id && $0.binding == binding }
        if !matching.isEmpty {
            SettingsCard("操作记录", symbol: "clock.arrow.circlepath") {
                ForEach(matching.reversed()) { record in
                    HStack {
                        VStack(alignment: .leading) {
                            Text("\(record.card) · \(resultLabel(record.code))")
                            if let windows = record.windows { Text("已重置 \(windows) 个额度窗口").font(.caption) }
                            Text(record.id.uuidString).font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
                        }
                        Spacer(); Button("核对结果") { lookup(record) }.disabled(busy || !session.target.current)
                    }
                }
            }
        }
    }

    private func resultLabel(_ code: String) -> String {
        switch code {
        case "reset": "重置成功"
        case "already_redeemed": "卡片已使用"
        case "nothing_to_reset": "无需重置"
        case "no_credit": "没有可用卡片"
        case "not_found": "未找到回执，请人工核实"
        default: "结果待核实，不会重复提交"
        }
    }
    private func load(force: Bool) async {
        guard !busy else { return }; busy = true; defer { busy = false }
        do {
            if official {
                records = try book.read(); journalAvailable = true
                value = try await session.target.call(force ? "official.refresh" : "official.snapshot", sourceFields)
            } else { value = try await session.target.call("balance.query", ["source_id": .string(session.source.id), "force": .bool(force)]) }
            failed = false
        } catch { if !Task.isCancelled { failed = true; value = .null; journalAvailable = false } }
    }

    private func redeem(_ selected: RemoteCardSelection) {
        guard !busy, session.target.current, journalAvailable else { return }
        busy = true
        Task {
            defer { busy = false }
            do {
                // Account changes can occur without a pool revision change. Revalidate the exact visible card.
                let live = try await session.target.call("official.snapshot", sourceFields)
                guard live["account_binding"].string == selected.binding,
                      let current = CreditInventory(live["credits"]).usableCards.first(where: { $0["id"].string == selected.id }),
                      ["status", "expires_at", "expiry_known", "reset_type"].allSatisfy({ current[$0] == selected.card[$0] }), session.target.current else { throw RemoteConsoleError.invalid }
                let record = try book.reserve(source: session.source.id, binding: selected.binding, card: selected.id)
                records = try book.read()
                var fields = record.resultFields; fields["confirmed"] = .bool(true); fields["credit_id"] = .string(record.card)
                let result = try await session.target.call("official.credits.consume", fields)
                try book.update(record, result: result); records = try book.read(); failed = false
                value = try await session.target.call("official.snapshot", sourceFields)
            } catch { failed = true }
        }
    }
    private func lookup(_ record: RemoteConsoleReceipt) {
        guard !busy else { return }; busy = true
        Task { defer { busy = false }; do {
            let result = try await session.target.call("official.credits.result", record.resultFields)
            try book.update(record, result: result); records = try book.read(); failed = false
        } catch { failed = true } }
    }
}

private struct RemoteCardSelection { let id: String; let binding: String; let card: JSONValue }
