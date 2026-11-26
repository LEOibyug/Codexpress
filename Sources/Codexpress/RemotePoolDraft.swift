import Foundation

extension RemoteConsoleConfig {
    static func source(_ row: JSONValue) throws -> Source {
        guard case .object(var fields) = row else { throw RemoteConsoleError.invalid }
        fields["automaticModelDiscovery"] = fields.removeValue(forKey: "automatic_model_discovery")
        fields["balanceWarningThreshold"] = fields.removeValue(forKey: "balance_warning_threshold")
        return try remoteConsoleDecode(Source.self, .object(fields))
    }

    private static func fields(_ source: Source) throws -> [String: JSONValue] {
        guard case .object(var fields) = try JSONValue.from(source) else { throw RemoteConsoleError.invalid }
        fields.removeValue(forKey: "account_id")
        fields["automatic_model_discovery"] = fields.removeValue(forKey: "automaticModelDiscovery")
        fields["balance_warning_threshold"] = fields.removeValue(forKey: "balanceWarningThreshold")
        return fields
    }

    @MainActor mutating func stage(_ source: Source, key: String, saved: JSONValue?) throws {
        try source.validate()
        let index = sources.firstIndex { $0["id"].string == source.id }
        let row = index.map { sources[$0] }
        let previous = try row.map(Self.source)
        let moved = previous.map { $0.base_url != source.base_url || $0.proxy != source.proxy } ?? false
        guard previous == nil || previous?.kind == source.kind else { throw RemoteConsoleError.invalid }
        let pending = !key.isEmpty ? key : (moved ? nil : row?["api_key"].string)
        if source.official {
            guard pending == nil else { throw RemoteConsoleError.invalid }
        } else if pending == nil {
            if let saved, !Self.matchesDestination(source, saved) { throw AppError.message("更换 API 地址或代理时，请填写用于新地址的新密钥。") }
            if saved == nil && source.enabled { throw AppError.message("请填写 API 密钥，或先停用此来源。") }
        }

        // Patch only changed public fields: preserve server extensions, explicit nulls and pending model edits.
        var next = try Self.fields(source)
        if let row, case var .object(original) = row, let previous {
            let old = try Self.fields(previous)
            for field in Set(old.keys).union(next.keys) where next[field] != old[field] { original[field] = next[field] }
            next = original
        }
        next["priority"] = .number(Double(index ?? sources.count))
        next["api_key"] = pending.map(JSONValue.string)
        if moved && source.official { next["reset_consent_binding"] = nil; revokeConsent(source.id) }
        if let index { sources[index] = .object(next) } else { sources.append(.object(next)) }
        if source.defaultsDiscoveredModelsDisabled, source.discoversModels, !source.models.isEmpty, previous?.models != source.models {
            try RemoteModelDraft.stageDiscoveredPolicies(source, previous: previous?.models ?? [], config: &self)
        }
        if !source.enabled { releaseFixed(source.id) }
    }

    static func matchesDestination(_ source: Source, _ row: JSONValue) -> Bool {
        row["id"].string == source.id && row["kind"].string == source.kind && row["base_url"].string == source.base_url &&
            row["proxy"]["mode"].string == source.proxy.mode && row["proxy"]["url"].string == source.proxy.url
    }

    @MainActor mutating func importSource(_ local: Source, includeKey: Bool, credential: @autoclosure () -> JSONValue?) throws {
        var source = local; source.id = UUID().uuidString; source.account_id = nil
        if source.isGoogle && source.remote_compaction_v2 == true { source.remote_compaction_v2 = nil }
        source.proxy = ProxyPolicy(mode: source.official ? "system" : "direct"); source.enabled = false
        var key = ""
        if includeKey {
            guard !source.official, let value = credential()?["api_key"].string, !value.isEmpty else { throw AppError.message("此来源没有可导入的 API 密钥。") }
            key = value
        }
        try stage(source, key: key, saved: nil)
    }

    mutating func remove(_ id: String) throws {
        guard let index = sources.firstIndex(where: { $0["id"].string == id }) else { throw RemoteConsoleError.invalid }
        sources.remove(at: index); renumber(); releaseFixed(id); revokeConsent(id)
    }

    mutating func move(_ id: String, by delta: Int) throws {
        guard let index = sources.firstIndex(where: { $0["id"].string == id }), [-1, 1].contains(delta), sources.indices.contains(index + delta) else { throw RemoteConsoleError.invalid }
        sources.swapAt(index, index + delta); renumber()
    }

    private mutating func renumber() {
        for i in sources.indices where sources[i]["priority"].number != Double(i) {
            if case .object(var row) = sources[i] { row["priority"] = .number(Double(i)); sources[i] = .object(row) }
        }
    }

    private mutating func releaseFixed(_ id: String) {
        if settings["fixed_source_id"]?.string == id { settings["mode"] = .string("auto"); settings["fixed_source_id"] = .null }
    }

    private mutating func revokeConsent(_ id: String) {
        if case let .array(enrolled) = settings["auto_reset_weekly_accounts"] { settings["auto_reset_weekly_accounts"] = .array(enrolled.filter { $0.string != id }) }
    }
}

@MainActor struct RemotePoolDraft {
    let draft: RemotePreferencesDraft
    let target: RemoteConsoleTarget
    private let session: UUID

    init(_ draft: RemotePreferencesDraft, target: RemoteConsoleTarget) {
        self.draft = draft; self.target = target; session = draft.editingSession
    }

    private var current: Bool {
        target.current && draft.editingSession == session && draft.target?.generation == target.generation &&
            !draft.needsReload && !draft.submitting
    }
    var valid: Bool { current && target.management.capabilities.contains("config.save") }

    func list() throws -> [Source] { try draft.sources.map(RemoteConsoleConfig.source) }

    private func edit(_ change: (inout RemoteConsoleConfig) throws -> Void) throws {
        guard valid else { throw AppError.message("服务器配置已变更，请重新载入后编辑。") }
        var config = RemoteConsoleConfig(sources: draft.sources, settings: draft.settings)
        try change(&config)
        draft.sources = config.sources; draft.settings = config.settings
    }

    func stage(_ source: Source, key: String = "") throws {
        let saved = draft.config?.sources.first { $0["id"].string == source.id }
        try edit { try $0.stage(source, key: key, saved: saved) }
    }

    func remove(_ source: Source) throws { try edit { try $0.remove(source.id) } }
    func move(_ source: Source, by delta: Int) throws { try edit { try $0.move(source.id, by: delta) } }
    func importSource(_ source: Source, includeKey: Bool, credential: @autoclosure () -> JSONValue?) throws {
        try edit { try $0.importSource(source, includeKey: includeKey, credential: credential()) }
    }

    func savedDestinationIDs(for sources: [Source]) -> Set<String> {
        guard current, let saved = draft.config?.sources else { return [] }
        var byID: [String: JSONValue] = [:]
        for row in saved { if let id = row["id"].string { byID[id] = row } }
        return Set(zip(sources, draft.sources).compactMap { source, row in
            guard row["api_key"] == .null, let original = byID[source.id], RemoteConsoleConfig.matchesDestination(source, original),
                  RemoteConsoleConfig.matchesDestination(source, row) else { return nil }
            return source.id
        })
    }

    func isSavedDestination(_ source: Source) -> Bool {
        guard current, let saved = draft.config?.sources.first(where: { $0["id"].string == source.id }),
              let row = draft.sources.first(where: { $0["id"].string == source.id }), row["api_key"] == .null else { return false }
        return RemoteConsoleConfig.matchesDestination(source, saved) && RemoteConsoleConfig.matchesDestination(source, row)
    }
}
