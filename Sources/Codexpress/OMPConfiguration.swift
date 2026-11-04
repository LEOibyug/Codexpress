import Foundation

/// A transient editor for the stored global OMP configuration, not application preferences.
struct OMPConfiguration {
    struct Field: Hashable {
        enum Kind { case text, positiveInteger, integer, number, boolean, threshold, methods }
        var file: String
        var path: [String]
        var kind: Kind
        var title: String

        static func config(_ key: String, _ title: String, _ kind: Kind = .positiveInteger) -> Self {
            Self(file: "config", path: ["compaction", key], kind: kind, title: title)
        }
        static func model(_ id: String, _ key: String, _ title: String, _ kind: Kind = .positiveInteger) -> Self {
            Self(file: "models", path: ["providers", "codexpress", "modelOverrides", id, key], kind: kind, title: title)
        }
        static func remote(_ key: String, _ title: String, model: String? = nil, _ kind: Kind = .text) -> Self {
            let root = ["providers", "codexpress"] + (model.map { ["modelOverrides", $0] } ?? [])
            return Self(file: "models", path: root + ["remoteCompaction", key], kind: kind, title: title)
        }
        static func selector(_ selector: String) -> Self {
            Self(file: "config", path: ["compaction", "modelThresholds", selector], kind: .threshold, title: selector)
        }
        static func == (lhs: Self, rhs: Self) -> Bool { lhs.file == rhs.file && lhs.path == rhs.path }
        func hash(into hasher: inout Hasher) { hasher.combine(file); hasher.combine(path) }
    }

    var stored: JSONValue = .null
    var changes: [Field: String] = [:]
    var errors: [Field: String] = [:]
    var addedSelectors: [String] = []

    var available: Bool { if case .object = stored { return true }; return false }
    var modelIDs: [String] { stored["models"].array.compactMap { $0["id"].string } }
    var selectors: [String] {
        let existing: [String]
        if case .object(let values) = stored["compaction"]["modelThresholds"] { existing = Array(values.keys) }
        else { existing = [] }
        return Array(Set(existing + addedSelectors)).sorted()
    }
    var hasChanges: Bool { !changes.isEmpty }

    func value(_ field: Field) -> JSONValue? {
        var root: JSONValue
        var path: [String]
        if field.file == "config" { root = stored["compaction"]; path = Array(field.path.dropFirst()) }
        else if field.path.count > 3, field.path[2] == "modelOverrides" {
            root = stored["models"].array.first { $0["id"].string == field.path[3] }?["overrides"] ?? .null
            path = Array(field.path.dropFirst(4))
        } else { root = stored["providerRemoteCompaction"]; path = Array(field.path.dropFirst(3)) }
        for key in path {
            guard case .object(let values) = root, let next = values[key] else { return nil }
            root = next
        }
        return root
    }

    func base(_ id: String, _ key: String) -> JSONValue? {
        guard let row = stored["models"].array.first(where: { $0["id"].string == id }),
              case .object(let values) = row["base"] else { return nil }
        return values[key]
    }

    static func text(_ value: JSONValue?) -> String {
        switch value {
        case .string(let text): return text
        case .number(let number): return number.rounded() == number ? String(format: "%.0f", number) : String(number)
        case .bool(let flag): return flag ? "true" : "false"
        case .array(let values): return values.compactMap(\.string).joined(separator: ",")
        case .null?: return "null"
        default: return ""
        }
    }

    func text(_ field: Field) -> String { changes[field] ?? Self.text(value(field)) }

    mutating func set(_ field: Field, _ text: String) {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed == Self.text(value(field)) && !(trimmed.isEmpty && value(field) != nil) { changes.removeValue(forKey: field) }
        else { changes[field] = trimmed }
        errors.removeValue(forKey: field)
    }

    mutating func addSelector(_ text: String) throws {
        let selector = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard Self.validSelector(selector) else { throw AppError.message("选择器须为 provider/model 或 provider/前缀*（如 codexpress/*），且不能包含空白。") }
        guard !selectors.contains(selector) else { throw AppError.message("此选择器已存在，请编辑现有规则。") }
        addedSelectors.append(selector)
    }

    static func validSelector(_ value: String) -> Bool {
        guard !value.isEmpty, !value.contains(where: \.isWhitespace) else { return false }
        let parts = value.split(separator: "/", maxSplits: 1, omittingEmptySubsequences: false)
        return parts.count == 2 && parts.allSatisfy { !$0.isEmpty } && !value.dropLast().contains("*")
    }

    private static let safeInteger: Double = 9_007_199_254_740_991
    private func parse(_ field: Field, _ text: String) throws -> JSONValue {
        func invalid(_ message: String) -> AppError { .message(message) }
        func integer(_ value: String, positive: Bool = true) throws -> Double {
            let digits = value.hasPrefix("-") ? value.dropFirst() : value[...]
            guard !digits.isEmpty, digits.allSatisfy({ $0.isASCII && $0.isNumber }),
                  let number = Double(value), number.isFinite, abs(number) <= Self.safeInteger,
                  !positive || number > 0 else { throw invalid(positive ? "请输入 1–9007199254740991 的整数；留空恢复继承。" : "请输入安全范围内的整数；留空恢复继承。") }
            return number
        }
        switch field.kind {
        case .text: return .string(text)
        case .boolean:
            guard ["true", "false"].contains(text) else { throw invalid("请选择继承、启用或禁用。") }
            return .bool(text == "true")
        case .positiveInteger: return .number(try integer(text))
        case .integer: return .number(try integer(text, positive: false))
        case .number:
            guard let number = Double(text), number.isFinite, abs(number) <= Self.safeInteger else { throw invalid("请输入有限数值；留空恢复继承。") }
            return .number(number)
        case .methods:
            let methods = text.split(separator: ",").map(String.init)
            guard !methods.isEmpty, Set(methods).count == methods.count,
                  methods.allSatisfy({ ["remote", "snapcompact", "handoff", "shake", "soft"].contains($0) }) else { throw invalid("请选择至少一种不重复的压缩方法。") }
            return .array(methods.map(JSONValue.string))
        case .threshold:
            guard let selector = field.path.last, Self.validSelector(selector) else { throw invalid("选择器须为 provider/model 或 provider/前缀*。") }
            if text == "null" { return .null }
            if text.hasPrefix("f") || text.hasPrefix("F") { _ = try integer(String(text.dropFirst())); return .string(text) }
            if text.hasSuffix("%") {
                let decimal = text.dropLast().split(separator: ".", omittingEmptySubsequences: false)
                guard (1...2).contains(decimal.count), decimal.allSatisfy({ !$0.isEmpty && $0.allSatisfy { $0.isASCII && $0.isNumber } }),
                      let percent = Double(text.dropLast()), percent.isFinite, percent > 0, percent <= 100 else { throw invalid("请输入大于 0 且不超过 100 的十进制百分比。") }
                return .string(text)
            }
            return .number(try integer(text))
        }
    }

    mutating func edits() -> [JSONValue]? {
        errors = [:]
        var result: [JSONValue] = []
        var parsed: [Field: JSONValue?] = [:]
        for (field, text) in changes.sorted(by: { ($0.key.file + $0.key.path.joined(separator: "/")) < ($1.key.file + $1.key.path.joined(separator: "/")) }) {
            var edit: [String: JSONValue] = ["file": .string(field.file), "path": .array(field.path.map(JSONValue.string))]
            if text.isEmpty { edit["remove"] = .bool(true); parsed[field] = .some(nil) }
            else {
                do {
                    let next = try parse(field, text)
                    parsed[field] = .some(next)
                    if next == value(field) { continue }
                    edit["value"] = next
                }
                catch { errors[field] = error.localizedDescription; continue }
            }
            result.append(.object(edit))
        }
        for id in modelIDs {
            let window = Field.model(id, "contextWindow", "上下文窗口")
            let extended = Field.model(id, "maxContextWindow", "扩展窗口")
            guard changes[window] != nil || changes[extended] != nil else { continue }
            func effective(_ field: Field) -> Double? {
                if let changed = parsed[field] { return (changed ?? base(id, field.path.last!))?.number }
                return (value(field) ?? base(id, field.path.last!))?.number
            }
            if let normal = effective(window), let maximum = effective(extended), maximum < normal {
                let field = changes[extended] != nil ? extended : window
                errors[field] = "扩展窗口不能小于上下文窗口（含模型基础值）。"
            }
        }
        return errors.isEmpty ? result : nil
    }
    /// Replaces editable compression settings, never provider credentials or catalog metadata.
    /// Only model IDs present in both catalogs receive model overrides.
    mutating func importSaved(_ configuration: JSONValue) -> [String] {
        let local = OMPConfiguration(stored: configuration)
        func copy(_ fields: [Field], into draft: inout Self) {
            for field in fields { draft.set(field, Self.text(local.value(field))) }
        }
        copy(Self.compactionFields, into: &self)
        copy(Self.remoteFields(), into: &self)

        for selector in selectors where selector.hasPrefix("codexpress/") {
            set(.selector(selector), "")
        }
        for selector in local.selectors where selector.hasPrefix("codexpress/") {
            if !selectors.contains(selector) { addedSelectors.append(selector) }
            set(.selector(selector), local.text(.selector(selector)))
        }

        let remoteIDs = Set(modelIDs)
        for id in local.modelIDs where remoteIDs.contains(id) {
            copy(Self.modelFields(id) + Self.remoteFields(model: id), into: &self)
        }
        return local.modelIDs.filter { !remoteIDs.contains($0) }.sorted()
    }

    private static let compactionFields: [Field] = [
        .config("enabled", "自动压缩", .boolean),
        .config("midTurnEnabled", "轮次中压缩", .boolean),
        .config("autoContinue", "压缩后自动继续", .boolean),
        .config("thresholdTokens", "固定阈值", .integer),
        .config("thresholdPercent", "阈值百分比", .number),
        .config("reserveTokens", "预留预算", .number),
        .config("keepRecentTokens", "保留近期预算", .number),
        .config("methodOrder", "压缩方法顺序", .methods),
        .config("modelThresholdsEnabled", "模型阈值规则", .boolean),
        .config("asyncEnabled", "异步压缩", .boolean),
        .config("idleEnabled", "空闲压缩", .boolean),
        .config("idleThresholdTokens", "空闲触发预算", .number),
        .config("idleTimeoutSeconds", "空闲超时（秒）", .number),
        .config("remoteStreamingV2Enabled", "允许远程流式 V2", .boolean),
        .config("v2RetainedMessageBudget", "V2 保留消息预算", .number),
        .config("handoffSaveToDisk", "Handoff 写入磁盘", .boolean),
        .config("supersedeReads", "替代重复读取", .boolean),
        .config("dropUseless", "移除无用内容", .boolean),
        .config("experimentalContextManagement", "实验性上下文管理", .boolean)
    ]

    private static func modelFields(_ id: String) -> [Field] {
        [
            .model(id, "contextWindow", "上下文窗口"),
            .model(id, "maxContextWindow", "扩展窗口"),
            .model(id, "maxTokens", "输出上限"),
            .model(id, "omitMaxOutputTokens", "省略输出上限参数", .boolean),
            .model(id, "compactionModel", "压缩模型", .text)
        ]
    }

    private static func remoteFields(model id: String? = nil) -> [Field] {
        [
            .remote("enabled", "原生远程压缩", model: id, .boolean),
            .remote("api", "API", model: id),
            .remote("model", "远程压缩模型", model: id),
            .remote("endpoint", "V1 压缩端点", model: id),
            .remote("v2Endpoint", "V2 压缩端点", model: id),
            .remote("streamingEndpoint", "流式端点", model: id),
            .remote("v2StreamingEnabled", "流式 V2", model: id, .boolean)
        ]
    }

    mutating func applyOpenAIPreset() {
        for key in ["enabled", "midTurnEnabled", "autoContinue", "modelThresholdsEnabled",
                    "asyncEnabled", "supersedeReads", "dropUseless", "remoteStreamingV2Enabled"] {
            set(.config(key, key, .boolean), "true")
        }
        for key in ["idleEnabled", "experimentalContextManagement"] {
            set(.config(key, key, .boolean), "false")
        }
        set(.config("methodOrder", "压缩方法顺序", .methods), "remote,handoff,soft")
        for key in ["thresholdTokens", "thresholdPercent", "reserveTokens", "keepRecentTokens"] {
            set(.config(key, key, .number), "")
        }
        // Exact selectors outrank prefixes; remove only this provider's old rules.
        for selector in selectors where selector.hasPrefix("codexpress/") {
            set(.selector(selector), "")
        }
        if !selectors.contains("codexpress/*") { addedSelectors.append("codexpress/*") }
        set(.selector("codexpress/*"), "80%")
        set(.remote("enabled", "原生远程压缩", .boolean), "true")
        set(.remote("api", "API"), "openai-responses")
        set(.remote("v2StreamingEnabled", "流式 V2", .boolean), "true")
        for key in ["endpoint", "model", "v2Endpoint", "streamingEndpoint"] {
            set(.remote(key, key), "")
        }
        for id in modelIDs {
            for key in ["enabled", "api", "endpoint", "model", "v2Endpoint", "streamingEndpoint", "v2StreamingEnabled"] {
                set(.remote(key, key, model: id), "")
            }
            set(.model(id, "omitMaxOutputTokens", "省略输出上限参数", .boolean), "false")
            set(.model(id, "compactionModel", "压缩模型", .text), "")
        }
        // Official Sol: 922K maximum input / 128K output. Leave other capacities intact.
        // https://developers.openai.com/api/docs/models/gpt-6.1-sol
        if modelIDs.contains("gpt-6.1-sol") {
            let id = "gpt-6.1-sol"
            let selector = "codexpress/" + id
            if !selectors.contains(selector) { addedSelectors.append(selector) }
            set(.selector(selector), "f600000")
            set(.model(id, "contextWindow", "上下文窗口"), "900000")
            let inheritedMaximum = base(id, "maxContextWindow")?.number
            set(.model(id, "maxContextWindow", "扩展窗口"), inheritedMaximum.map { $0 < 900000 } == true ? "900000" : "")
            set(.model(id, "maxTokens", "输出上限"), "128000")
        }
    }

    mutating func importCodex(_ rules: [ModelCompactionRule]) -> [String] {
        var skipped: [String] = []
        for rule in rules {
            let selector = "codexpress/" + rule.model
            guard Self.validSelector(selector), rule.auto_compact_token_limit > 0 else { skipped.append(rule.model); continue }
            if !selectors.contains(selector) { addedSelectors.append(selector) }
            set(.selector(selector), "f\(rule.auto_compact_token_limit)")
            if let window = rule.context_window {
                if modelIDs.contains(rule.model) { set(.model(rule.model, "contextWindow", "上下文窗口"), String(window)) }
                else { skipped.append("\(rule.model)（窗口：请先接入该模型）") }
            }
        }
        return skipped
    }
}
