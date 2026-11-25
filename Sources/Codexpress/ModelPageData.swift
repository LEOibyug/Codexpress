import Foundation

/// A read-only projection for the shared views. Writes use the captured backend, never a projected settings snapshot.
@MainActor struct ModelPageData {
    enum Backend {
        case local(AppModel)
        case remote(RemoteModelDraft)
    }

    let backend: Backend
    let settings: AppSettings
    let sources: [Source]
    let configuredModelIDs: Set<String>
    let modelInformation: [String: JSONValue]
    let running: Bool
    let busy: Bool
    let metadataLoadFailed: Bool
    let modelInformationRefreshing: Bool
    let modelInformationError: String?
    let refreshingModels: Set<String>
    let remoteUsage: [String: JSONValue]?
    let metadataNotice: String?
    let canStageMetadata: Bool
    let canDiscover: Bool
    let syncNote: String

    init(local: AppModel) {
        backend = .local(local); settings = local.settings; sources = local.sources
        configuredModelIDs = Set(local.configuredModelIDs); modelInformation = local.modelInformation
        running = local.running; busy = local.busy; metadataLoadFailed = local.metadataLoadFailed
        modelInformationRefreshing = local.modelInformationRefreshing; modelInformationError = local.modelInformationError
        refreshingModels = local.refreshingModels; remoteUsage = nil; metadataNotice = nil; canStageMetadata = true; canDiscover = true
        let localCodex = local.connectionMode == "official" ? "" : "本地 Codex 在「接入」中手动同步；"
        syncNote = "OMP 自动同步；\(localCodex)远端 Codex 在「远端 SSH」中预览并应用；聊天客户端请刷新模型列表。"
    }

    init(remote: RemoteModelDraft) throws {
        backend = .remote(remote); settings = try remote.settings(); sources = settings.sources
        var ids = Set(sources.lazy.flatMap(\.models))
        ids.formUnion((settings.modelPolicies ?? []).lazy.map(\.model)); ids.formUnion((settings.modelCompaction ?? []).lazy.map(\.model))
        if sources.contains(where: \.official) { ids.insert("gpt-image-2") }
        configuredModelIDs = ids; modelInformation = remote.information
        running = remote.target.current; busy = remote.draft.submitting; metadataLoadFailed = !remote.canEdit
        modelInformationRefreshing = false; modelInformationError = nil; refreshingModels = []
        remoteUsage = remote.target.management.capabilities.contains("usage.models") ? remote.draft.modelUsage : nil
        metadataNotice = remote.draft.metadataStaged ? remote.draft.metadataFile.map { "已选择 \($0.count) 个模型的能力，提交后替换远端导入。" } ?? "已选择恢复默认模型能力，提交后生效。" : nil
        canStageMetadata = remote.canStageMetadata; canDiscover = remote.canEdit && remote.target.management.capabilities.contains("models.list")
        syncNote = "提交后应用到远端网关；连接该网关的客户端请刷新模型列表。"
    }

    var remote: RemoteModelDraft? { if case let .remote(remote) = backend { return remote }; return nil }
    var editsDisabled: Bool { busy || metadataLoadFailed }

    func setEnabled(_ ids: [String], on source: String? = nil, enabled: Bool) throws {
        switch backend {
        case let .local(local):
            if let source { try local.setProviderModelsEnabled(sourceID: source, modelIDs: ids, enabled: enabled) }
            else { try local.setModelsEnabled(ids, enabled: enabled) }
        case let .remote(remote): try remote.setEnabled(ids, on: source, enabled: enabled)
        }
    }

    func saveModelSettings(model id: String, policy: ModelPolicy?, compaction: ModelCompactionRule?) async throws {
        switch backend {
        case let .local(local): try await local.saveModelSettings(model: id, policy: policy, compaction: compaction)
        case let .remote(remote): try remote.saveModelSettings(model: id, policy: policy, compaction: compaction)
        }
    }
}

@MainActor struct RemoteModelDraft {
    let draft: RemotePreferencesDraft
    let target: RemoteConsoleTarget
    private let metadataRevision: String?
    private let session: UUID

    init(draft: RemotePreferencesDraft, target: RemoteConsoleTarget) {
        self.draft = draft; self.target = target; metadataRevision = draft.modelInfo["metadata_revision"].string
        session = draft.editingSession
    }

    var current: Bool {
        target.current && draft.editingSession == session && draft.target?.generation == target.generation &&
            draft.target?.pool.revision == target.pool.revision && !draft.submitting && !draft.needsReload
    }
    var canEdit: Bool { current && target.management.capabilities.contains("config.save") }
    var canStageMetadata: Bool {
        current && target.management.capabilities.contains("models.metadata.replace") && metadataRevision != nil && metadataRevision == draft.modelInfo["metadata_revision"].string
    }

    func settings() throws -> AppSettings {
        var settings = AppSettings()
        settings.sources = try draft.sources.map(RemoteConsoleConfig.source)
        if let rows = draft.settings["model_policies"], rows != .null { settings.modelPolicies = try remoteConsoleDecode([ModelPolicy].self, rows) }
        if let rows = draft.settings["model_compaction"], rows != .null { settings.modelCompaction = try remoteConsoleDecode([ModelCompactionRule].self, rows) }
        try ModelPolicy.validate(settings.modelPolicies ?? [])
        try ModelCompactionRule.validate(settings.modelCompaction ?? [])
        return settings
    }

    var information: [String: JSONValue] {
        let baseline = draft.config?.settings["model_compaction"]?.array ?? []
        let windowOverrides = Set(baseline.compactMap { $0["context_window"].number == nil ? nil : $0["model"].string })
        let limitOverrides = Set(baseline.compactMap { $0["auto_compact_token_limit"].number == nil ? nil : $0["model"].string })
        var info = [String: JSONValue]()
        for row in draft.modelInfo["models"].array {
            guard let id = row["id"].string, case var .object(fields) = row else { continue }
            // The management projection may expose effective budgets only. A budget capped by a saved rule is not a model default.
            let replacingImport = draft.metadataStaged && fields["local_metadata"] == .bool(true)
            if replacingImport { fields.removeValue(forKey: "local_metadata") }
            for (key, overridden) in [("max_context_window", windowOverrides.contains(id)), ("context_window", windowOverrides.contains(id)),
                                       ("auto_compact_token_limit", windowOverrides.contains(id) || limitOverrides.contains(id))] {
                let name = "default_" + key
                if replacingImport { fields[name] = nil }
                else if fields[name] == nil && !overridden { fields[name] = fields[key] }
            }
            // Values feed integer formatting and budget arithmetic in the local UI. Unsupported remote numbers are unknown, not zero.
            for key in ["default_max_context_window", "default_context_window", "default_auto_compact_token_limit", "max_output_tokens"] {
                if let n = fields[key]?.number, Int(exactly: n) == nil || !(0...Double(Int.max / 10)).contains(n) { fields[key] = nil }
            }
            info[id] = .object(fields)
        }
        if draft.metadataStaged, let file = draft.metadataFile {
            for row in file.models {
                guard let id = row["slug"].string else { continue }
                var fields: [String: JSONValue] = [:]
                if case let .object(existing)? = info[id] { fields = existing }
                fields["local_metadata"] = .bool(true); fields["default_max_context_window"] = row["max_context_window"]
                fields["default_auto_compact_token_limit"] = row["auto_compact_token_limit"]
                info[id] = .object(fields)
            }
        }
        return info
    }

    func setEnabled(_ ids: [String], on source: String?, enabled: Bool) throws {
        guard canEdit else { throw RemoteConsoleError.invalid }
        var next = try settings()
        let previous = next.modelPolicies ?? []
        if let source { try ModelPolicy.setProviderModelsEnabled(sourceID: source, modelIDs: ids, enabled: enabled, in: &next) }
        else { try ModelPolicy.setModelsEnabled(ids, enabled: enabled, in: &next) }
        var fields = draft.settings
        try Self.updateRows(key: "model_policies", old: previous, new: next.modelPolicies ?? [], id: \.model, in: &fields)
        draft.settings = fields
    }

    func saveModelSettings(model id: String, policy: ModelPolicy?, compaction: ModelCompactionRule?) throws {
        guard canEdit else { throw RemoteConsoleError.invalid }
        let next = try settings()
        let previousPolicies = next.modelPolicies ?? [], previousRules = next.modelCompaction ?? []
        var policies = previousPolicies.filter { $0.model != id }, rules = previousRules.filter { $0.model != id }
        if let policy { policies.append(policy) }; if let compaction { rules.append(compaction) }
        try ModelPolicy.validate(policies); try ModelCompactionRule.validate(rules)
        var fields = draft.settings
        try Self.updateRows(key: "model_policies", old: previousPolicies, new: policies, id: \.model, in: &fields)
        try Self.updateRows(key: "model_compaction", old: previousRules, new: rules, id: \.model, in: &fields)
        draft.settings = fields
    }

    /// Keep the wire order, unknown fields, and missing-vs-explicit defaults when only part of a policy changes.
    private static func updateRows<T: Equatable & Encodable>(key: String, old: [T], new: [T], id: KeyPath<T, String>, in fields: inout [String: JSONValue]) throws {
        guard old != new else { return }
        var pending = Dictionary(uniqueKeysWithValues: new.lazy.map { ($0[keyPath: id], $0) })
        let existing = fields[key]?.array ?? []
        var rows: [JSONValue] = []
        rows.reserveCapacity(new.count)
        for (original, raw) in zip(old, existing) {
            guard let replacement = pending.removeValue(forKey: original[keyPath: id]) else { continue }
            guard replacement != original else { rows.append(raw); continue }
            guard case var .object(row) = raw, case let .object(before) = try JSONValue.from(original),
                  case let .object(after) = try JSONValue.from(replacement) else { throw RemoteConsoleError.invalid }
            for (name, value) in after where before[name] != value { row[name] = value }
            for name in before.keys where after[name] == nil { row[name] = nil }
            rows.append(.object(row))
        }
        if !pending.isEmpty {
            for row in new where pending[row[keyPath: id]] != nil { rows.append(try JSONValue.from(row)) }
        }
        if rows != existing { fields[key] = .array(rows) }
    }

    static func stageDiscoveredPolicies(_ source: Source, previous: [String], config: inout RemoteConsoleConfig) throws {
        var next = AppSettings()
        next.sources = try config.sources.map(RemoteConsoleConfig.source)
        let old = try config.settings["model_policies"].flatMap { $0 == .null ? nil : try remoteConsoleDecode([ModelPolicy].self, $0) } ?? []
        try ModelPolicy.validate(old); next.modelPolicies = old
        AppModel.applyDefaultDisabledPolicies(for: source, previousModels: previous, in: &next)
        try updateRows(key: "model_policies", old: old, new: next.modelPolicies ?? [], id: \.model, in: &config.settings)
    }

    func discover(_ source: Source) async throws {
        let pool = RemotePoolDraft(draft, target: target)
        guard canEdit, target.management.capabilities.contains("models.list"), pool.isSavedDestination(source) else { throw RemoteConsoleError.invalid }
        let result = try await target.call("models.list", ["source_id": .string(source.id)])
        guard canEdit, case let .array(rows) = result["models"], !rows.isEmpty, pool.isSavedDestination(source),
              try pool.list().contains(source) else { throw RemoteConsoleError.invalid }
        let ids = try rows.map { value -> String in
            guard let id = value.string else { throw RemoteConsoleError.invalid }; try ModelPolicy.validateModelID(id); return id
        }
        guard source.models != ids else { return }
        var discovered = source; discovered.models = ids
        try pool.stage(discovered)
    }
}
