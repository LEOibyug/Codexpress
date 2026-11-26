import SwiftUI

/// The server replaces settings atomically. Keep untouched settings and source metadata exactly as received.
struct RemoteConsoleConfig {
    var sources: [JSONValue]
    var settings: [String: JSONValue]

    init(sources: [JSONValue], settings: [String: JSONValue]) {
        self.sources = sources; self.settings = settings
    }

    init(_ value: JSONValue) throws {
        guard case let .array(sources) = value["sources"], case let .object(settings) = value["settings"] else { throw RemoteConsoleError.invalid }
        self.sources = sources; self.settings = settings
    }
}

@MainActor final class RemotePreferencesDraft: ObservableObject {
    @Published private(set) var target: RemoteConsoleTarget?
    @Published private(set) var config: RemoteConsoleConfig?
    private(set) var editingSession = UUID()
    @Published var settings: [String: JSONValue] = [:]
    @Published var sources: [JSONValue] = []
    @Published private(set) var modelInfo: JSONValue = .null
    @Published private(set) var modelUsage: [String: JSONValue] = [:]
    @Published var modelEditorOpen = false
    var sourceEditorOpen = false
    @Published private(set) var historyStatus: JSONValue = .null
    @Published var historyEnabled = false
    @Published var historyCapacity = ""
    @Published private(set) var submitting = false
    @Published private(set) var notice: String?
    @Published private(set) var needsReload = false

    // Staged reset and no edit both have no file, but only a reset has a captured revision.
    @Published private(set) var metadataFile: RemoteConsoleMetadataFile?
    @Published private var stagedMetadataRevision: String?
    var metadataStaged: Bool { stagedMetadataRevision != nil }
    private var originalHistory: HistoryCacheSettings?
    var history: HistoryCacheSettings? { try? .parse(enabled: historyEnabled, gigabytes: historyCapacity) }
    private var historyChanged: Bool { originalHistory != nil && history != originalHistory }
    private var configChanged: Bool { config.map { settings != $0.settings || sources != $0.sources } ?? false }
    var changed: Bool { configChanged || metadataStaged || historyChanged }

    var canSubmit: Bool {
        guard let target, target.current, changed, !needsReload, !submitting else { return false }
        let capabilities = target.management.capabilities
        if historyChanged && (history == nil || !capabilities.contains("history.configure")) { return false }
        if configChanged && !capabilities.contains("config.save") { return false }
        for (key, range) in [("official_quota_refresh_minutes", 1...1440), ("idle_return_minutes", 0...10080)] {
            if let value = settings[key], !(value.number.flatMap { Int(exactly: $0) }.map(range.contains) ?? false) { return false }
        }
        return !metadataStaged || capabilities.contains("models.metadata.replace")
    }

    func stageMetadata(_ file: RemoteConsoleMetadataFile?) {
        guard !submitting, let revision = modelInfo["metadata_revision"].string else { return }
        stagedMetadataRevision = stagedMetadataRevision ?? revision; metadataFile = file
    }

    func load(_ peer: RemoteConsoleTarget, page: SettingsPage, discard: Bool = false) async {
        guard !submitting, peer.current, !needsReload || discard else { return }
        do {
            if target?.pool.revision != peer.pool.revision || config == nil || discard {
                if (changed || modelEditorOpen || sourceEditorOpen) && !discard { requireReload("服务器配置已变更。请重新载入后编辑，原草稿不会覆盖服务器。"); return }
                let next = try RemoteConsoleConfig(try await peer.call("config.get"))
                guard !submitting, peer.current else { return }
                editingSession = UUID()
                target = peer; config = next; sources = next.sources; settings = next.settings
                originalHistory = nil; historyStatus = .null; historyEnabled = false; historyCapacity = ""
                stagedMetadataRevision = nil; metadataFile = nil; modelInfo = .null; modelUsage = [:]; needsReload = false
            }
            notice = nil
            if page == .models { try await reloadModels() }
            else if page == .general { await reloadHistory() }
        } catch { if !Task.isCancelled { notice = "无法读取远端配置，请检查连接后重试。" } }
    }

    func reloadModels() async throws {
        guard let target, !submitting, target.current else { throw CancellationError() }
        if target.management.capabilities.contains("models.info") {
            let latest = try await target.call("models.info")
            guard target.current, !submitting else { return }
            if let base = stagedMetadataRevision, latest["metadata_revision"].string != base {
                requireReload("服务器的模型能力已变更。请重新载入后编辑。"); return
            }
            modelInfo = latest
        }
        if target.management.capabilities.contains("usage.models") {
            let stats = try await target.call("usage.models")
            guard target.current, !submitting else { return }
            modelUsage = stats["models"].array.reduce(into: [:]) { if let id = $1["model"].string { $0[id] = $1 } }
        }
    }

    private func historySettings(_ status: JSONValue) throws -> HistoryCacheSettings {
        guard let enabled = status["enabled"].bool, let bytes = status["max_bytes"].number.flatMap({ Int64(exactly: $0) }) else { throw RemoteConsoleError.invalid }
        return .init(enabled: enabled, maxBytes: bytes)
    }

    private func adoptHistory(_ status: JSONValue, _ value: HistoryCacheSettings) {
        historyStatus = status; originalHistory = value; historyEnabled = value.enabled
        historyCapacity = String(Double(value.maxBytes) / 1_000_000_000)
        if historyCapacity.hasSuffix(".0") { historyCapacity.removeLast(2) }
    }

    func reloadHistory() async {
        guard let target, !submitting, target.management.capabilities.contains("history.status") else { return }
        do {
            let latest = try await target.call("history.status")
            guard target.current, !submitting else { return }
            let preferences = try historySettings(latest)
            if historyChanged {
                historyStatus = latest
                if originalHistory != preferences { requireReload("服务器的原文缓存设置已变更。请重新载入后编辑。") }
            } else { adoptHistory(latest, preferences) }
        } catch { if !Task.isCancelled { notice = "无法读取服务器的原文缓存设置。" } }
    }

    private func requireReload(_ text: String) { needsReload = true; notice = text }

    func submit(page: SettingsPage) async {
        guard let target, canSubmit else { return }
        submitting = true; notice = nil
        // Only config.save advances the pool revision, so it must be last. A confirmed
        // independent write is never repeated if a later operation fails.
        var confirmed: [String] = []
        do {
            if historyChanged, let history {
                let latest = try await target.call("history.status")
                guard try historySettings(latest) == originalHistory else { throw RemoteConsoleError.invalid }
                let result = try await target.call("history.configure", ["enabled": .bool(history.enabled), "max_bytes": .number(Double(history.maxBytes))])
                guard try historySettings(result) == history else { throw RemoteConsoleError.invalid }
                adoptHistory(result, history); confirmed.append("原文缓存")
            }
            if let revision = stagedMetadataRevision {
                var fields: [String: JSONValue] = ["metadata_revision": .string(revision)]
                if let file = metadataFile { fields["json"] = .string(file.json) } else { fields["reset"] = .bool(true) }
                let result = try await target.call("models.metadata.replace", fields)
                guard result["metadata_revision"].string != nil else { throw RemoteConsoleError.invalid }
                stagedMetadataRevision = nil; metadataFile = nil; confirmed.append("模型能力")
                modelInfo = .null // Before another import, load the new independent metadata revision.
            }
            if configChanged {
                let result = try await target.saveConfiguration(sources: sources, settings: settings)
                guard result["revision"].string != nil else { throw RemoteConsoleError.invalid }
                config = nil // Never re-use the revision that was consumed by this write.
                sources = [] // Drop pending write-only API keys even if the subsequent refresh fails.
            }
            await target.management.refresh()
            submitting = false
            if let pool = target.management.snapshot, pool.loadID == target.generation {
                await load(RemoteConsoleTarget(target.management, pool), page: page)
            }
        } catch {
            submitting = false
            let prefix = confirmed.isEmpty ? "未能确认修改。" : "已保存\(confirmed.joined(separator: "、"))；其余修改未获确认。"
            requireReload(prefix + "请重新载入服务器配置，核对后再编辑。")
        }
    }
}
