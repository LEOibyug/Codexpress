import Foundation

struct RemoteSourceConfiguration {
    var sources: [Source]
    private var settings: [String: JSONValue]
    private var consentBindings: [String: String] = [:]
    var keys: [String: String] = [:]
    var mode: String
    var fixed: String

    init(_ wire: JSONValue) throws {
        guard case .object(let settings) = wire["settings"], case .array(let rows) = wire["sources"],
              let mode = settings["mode"]?.string, ["auto", "fixed"].contains(mode) else { throw AppError.message("来源配置格式无效。") }
        self.settings = settings; self.mode = mode; fixed = settings["fixed_source_id"]?.string ?? ""
        sources = try rows.map { row in
            guard case .object(var fields) = row else { throw AppError.message("来源配置格式无效。") }
            fields["automaticModelDiscovery"] = fields.removeValue(forKey: "automatic_model_discovery")
            fields["balanceWarningThreshold"] = fields.removeValue(forKey: "balance_warning_threshold")
            return try JSONDecoder().decode(Source.self, from: JSONEncoder().encode(JSONValue.object(fields)))
        }.sorted { $0.priority < $1.priority }
        guard Set(sources.map(\.id)).count == sources.count else { throw AppError.message("来源 ID 重复。") }
        for row in rows {
            if let id = row["id"].string, let binding = row["reset_consent_binding"].string { consentBindings[id] = binding }
        }
    }
    mutating func remove(_ id: String) {
        sources.removeAll { $0.id == id }; keys[id] = nil
        if fixed == id { fixed = ""; mode = "auto" }
        revokeConsent(id)
    }
    private mutating func revokeConsent(_ id: String) {
        consentBindings[id] = nil
        if case .array(let enrolled) = settings["auto_reset_weekly_accounts"] { settings["auto_reset_weekly_accounts"] = .array(enrolled.filter { $0.string != id }) }
    }
    mutating func apply(_ source: Source, key: String) {
        if let idx = sources.firstIndex(where: { $0.id == source.id }) {
            if source.official && sources[idx].proxy != source.proxy { revokeConsent(source.id) }
            sources[idx] = source
        } else { sources.append(source) }
        keys[source.id] = key.isEmpty ? nil : key
        if fixed == source.id && !source.enabled { fixed = ""; mode = "auto" }
    }
    mutating func importSource(_ local: Source, includeKey: Bool, credential: @autoclosure () -> JSONValue?) throws {
        var source = local; source.id = UUID().uuidString; source.account_id = nil
        source.proxy = ProxyPolicy(mode: source.official ? "system" : "direct")
        source.enabled = false
        try source.validate()
        var apiKey = ""
        if includeKey {
            guard !source.official, let value = credential()?["api_key"].string, !value.isEmpty else { throw AppError.message("此来源没有可导入的 API 密钥。") }
            apiKey = value
        }
        apply(source, key: apiKey)
    }
    func payload() throws -> JSONValue {
        guard mode != "fixed" || sources.contains(where: { $0.id == fixed && $0.enabled }) else { throw AppError.message("请选择一个已启用的固定来源。") }
        let rows = try sources.enumerated().map { index, source -> JSONValue in
            try source.validate()
            guard case .object(var fields) = try JSONValue.from(source) else { throw AppError.message("来源配置格式无效。") }
            fields.removeValue(forKey: "account_id"); fields.removeValue(forKey: "automaticModelDiscovery"); fields.removeValue(forKey: "balanceWarningThreshold")
            fields["automatic_model_discovery"] = .bool(source.discoversModels)
            fields["balance_warning_threshold"] = source.balanceWarningThreshold.map(JSONValue.number) ?? .null
            fields["priority"] = .number(Double(index)); fields["show_in_cmux"] = .bool(source.showInCmux); fields["is_subscription"] = .bool(source.is_subscription ?? false)
            fields["active_conversation_window_seconds"] = .number(Double(source.activeConversationWindowSeconds))
            if let binding = consentBindings[source.id] { fields["reset_consent_binding"] = .string(binding) }
            if let key = keys[source.id], !key.isEmpty, !source.official { fields["api_key"] = .string(key) }
            return .object(fields)
        }
        var updated = settings; updated["mode"] = .string(mode); updated["fixed_source_id"] = mode == "fixed" ? .string(fixed) : .null
        return .object(["sources": .array(rows), "settings": .object(updated)])
    }
}
