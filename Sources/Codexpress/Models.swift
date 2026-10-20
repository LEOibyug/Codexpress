import Foundation

// JSON values are used only inside the transient, owner-only IPC boundary.
enum JSONValue: Codable, Sendable, Equatable {
    case object([String: JSONValue]), array([JSONValue]), string(String), number(Double), bool(Bool), null
    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null }
        else if let x = try? c.decode(Bool.self) { self = .bool(x) }
        else if let x = try? c.decode(Double.self) { self = .number(x) }
        else if let x = try? c.decode(String.self) { self = .string(x) }
        else if let x = try? c.decode([String: JSONValue].self) { self = .object(x) }
        else { self = .array(try c.decode([JSONValue].self)) }
    }
    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .object(let x): try c.encode(x)
        case .array(let x): try c.encode(x)
        case .string(let x): try c.encode(x)
        case .number(let x): try c.encode(x)
        case .bool(let x): try c.encode(x)
        case .null: try c.encodeNil()
        }
    }
    subscript(_ key: String) -> JSONValue { if case .object(let x) = self { return x[key] ?? .null }; return .null }
    var string: String? { if case .string(let x) = self { return x }; return nil }
    var number: Double? { if case .number(let x) = self { return x }; return nil }
    var bool: Bool? { if case .bool(let x) = self { return x }; return nil }
    var array: [JSONValue] { if case .array(let x) = self { return x }; return [] }
    static func from<T: Encodable>(_ value: T) throws -> JSONValue { try JSONDecoder().decode(Self.self, from: JSONEncoder().encode(value)) }
}

struct ProxyPolicy: Codable, Equatable, Sendable {
    var mode = "direct"
    var url: String? = nil
}
struct Source: Codable, Identifiable, Equatable, Sendable {
    var id = UUID().uuidString
    var name = ""
    var kind = "api"
    var base_url = "https://api.openai.com/v1"
    var enabled = true
    var priority = 0
    var proxy = ProxyPolicy()
    var models: [String] = []
    var account_id: String? = nil
    var official: Bool { kind == "official" }
    var policyLabel: String { proxy.mode == "direct" ? "强制直连" : "指定代理" }
    static func newOfficial() -> Source {
        var s = Source(); s.kind = "official"; s.name = "官方账号"; s.base_url = "https://chatgpt.com/backend-api/codex"; s.proxy.mode = "explicit"; return s
    }
    func validate() throws {
        guard !name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { throw AppError.message("请填写来源名称。") }
        guard let u = URLComponents(string: base_url), ["http", "https"].contains(u.scheme?.lowercased() ?? ""), u.host != nil, u.user == nil, u.password == nil, u.query == nil, u.fragment == nil else { throw AppError.message("来源地址须为不含账号、密码、查询参数的 HTTP 或 HTTPS 地址。") }
        guard !official || base_url.trimmingCharacters(in: CharacterSet(charactersIn: "/")) == "https://chatgpt.com/backend-api/codex" else { throw AppError.message("官方来源必须使用官方 Codex 地址。") }
        guard ["api", "official"].contains(kind), ["direct", "explicit"].contains(proxy.mode) else { throw AppError.message("来源类型或网络策略无效。") }
        guard !official || proxy.mode == "explicit" else { throw AppError.message("官方账号必须使用指定代理。") }
        if proxy.mode == "explicit" {
            guard let raw = proxy.url, let p = URLComponents(string: raw), ["http", "https", "socks5", "socks5h"].contains(p.scheme?.lowercased() ?? ""), p.host != nil, p.port != nil, p.user == nil, p.password == nil, p.query == nil, p.fragment == nil, p.path.isEmpty || p.path == "/" else { throw AppError.message("请填写明确的代理地址和端口，例如 http://127.0.0.1:7890；不支持在代理地址中保存密码。") }
        }
    }
}
struct AppSettings: Codable, Sendable {
    var sources: [Source] = []
    var mode = "auto"
    var fixed_source_id: String? = nil
    var port = 17863
    var codexHome = ""
    // Optional for backwards-compatible decoding of existing sources.json files.
    var modelCompaction: [ModelCompactionRule]? = nil
}

struct ModelCompactionRule: Codable, Equatable, Sendable {
    var model: String
    var auto_compact_token_limit: Int
    var context_window: Int?

    static func parse(model: String, threshold: String, window: String) throws -> Self {
        let name = model.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !name.isEmpty, name.utf8.count <= 256, !name.contains(where: \.isWhitespace) else { throw AppError.message("请填写不含空白的模型 ID。") }
        guard let limit = Int(threshold.trimmingCharacters(in: .whitespacesAndNewlines)), (1...1_000_000_000).contains(limit) else { throw AppError.message("模型 \(name) 的压缩阈值须为正整数，最大十亿 Token。") }
        let rawWindow = window.trimmingCharacters(in: .whitespacesAndNewlines)
        var context: Int?
        if !rawWindow.isEmpty {
            guard let value = Int(rawWindow), (2...1_000_000_000).contains(value), limit <= value * 9 / 10 else { throw AppError.message("模型 \(name) 的上下文窗口须为整数，压缩阈值不能超过窗口的 90%。") }
            context = value
        }
        return Self(model: name, auto_compact_token_limit: limit, context_window: context)
    }

    static func validate(_ rules: [Self]) throws {
        guard rules.count <= 200, Set(rules.map(\.model)).count == rules.count else { throw AppError.message("模型 ID 不能重复，最多支持 200 条规则。") }
        for rule in rules { _ = try parse(model: rule.model, threshold: String(rule.auto_compact_token_limit), window: rule.context_window.map(String.init) ?? "") }
    }
}
enum AppError: LocalizedError {
    case message(String)
    var errorDescription: String? { switch self { case .message(let text): return text } }
}
struct MetadataStore: Sendable {
    let directory: URL
    var file: URL { directory.appendingPathComponent("sources.json") }
    func load() throws -> AppSettings {
        guard FileManager.default.fileExists(atPath: file.path) else { return AppSettings() }
        return try JSONDecoder().decode(AppSettings.self, from: Data(contentsOf: file))
    }
    func save(_ settings: AppSettings) throws {
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: directory.path)
        let data = try JSONEncoder().encode(settings)
        // Set permissions on a private temporary file before atomic rename.
        let temp = directory.appendingPathComponent(".metadata-\(UUID().uuidString)")
        guard FileManager.default.createFile(atPath: temp.path, contents: data, attributes: [.posixPermissions: 0o600]) else { throw AppError.message("无法写入来源设置。") }
        defer { try? FileManager.default.removeItem(at: temp) }
        guard rename(temp.path, file.path) == 0 else { throw AppError.message("无法原子保存来源设置。") }
    }
}
func countLabel(_ value: JSONValue) -> String { value.number.map { Int($0).formatted() } ?? "未知" }
func timestampLabel(_ value: JSONValue) -> String {
    guard let seconds = value.number else { return "未知" }
    return Date(timeIntervalSince1970: seconds).formatted(date: .abbreviated, time: .shortened)
}
func healthLabel(_ value: String?) -> String {
    switch value { case "ready": return "可用"; case "cooldown": return "冷却中"; case "exhausted": return "额度耗尽"; case "auth_required": return "需要登录"; case "proxy_error": return "代理故障"; default: return "等待状态" }
}

struct ResetOperation: Codable, Sendable {
    let sourceID: String
    let creditID: String
    let operationID: String
    let createdAt: Double
    var result: String
    var accountID: String? = nil
}
struct ResetOperationStore {
    let directory: URL
    var file: URL { directory.appendingPathComponent("reset-operations.json") }
    func load() throws -> [String: ResetOperation] {
        guard FileManager.default.fileExists(atPath: file.path) else { return [:] }
        return try JSONDecoder().decode([String: ResetOperation].self, from: Data(contentsOf: file))
    }
    func save(_ value: [String: ResetOperation]) throws {
        let temp = directory.appendingPathComponent(".reset-operations-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: temp) }
        guard FileManager.default.createFile(atPath: temp.path, contents: try JSONEncoder().encode(value), attributes: [.posixPermissions: 0o600]), rename(temp.path, file.path) == 0 else { throw AppError.message("无法保存重置卡操作记录，已阻止提交。") }
    }
}

extension Source {
    // Editors may hold a snapshot across an OAuth callback; editable fields never own identity.
    func preservingOfficialIdentity(current: Source?, live: JSONValue) -> Source {
        guard official else { return self }
        var edited = self
        edited.account_id = current?.account_id ?? live["account_id"].string
        return edited
    }
    func sameConfiguration(as other: Source) -> Bool {
        var first = self; var second = other
        first.account_id = nil; second.account_id = nil
        return first == second
    }
}

struct CreditConfirmation: Identifiable {
    var id: String { source.id + ":" + accountID + ":" + (credit["id"].string ?? "") }
    let source: Source
    let accountID: String
    let credit: JSONValue
    let quota: JSONValue
    static func make(source: Source, live: JSONValue, creditID: String?) throws -> Self {
        guard source.official, live["id"].string == source.id, live["kind"].string == "official",
              let account = live["account_id"].string, !account.isEmpty,
              source.account_id == account, let creditID, !creditID.isEmpty else { throw AppError.message("无法确认当前账号身份，请刷新来源后重新选择卡片。") }
        let snapshot = live["credits"]
        guard snapshot["stale"].bool == false, snapshot["error"] == .null,
              let card = snapshot["credits"].array.first(where: { $0["id"].string == creditID }),
              card["status"].string == "available", card["expiry_known"].bool == true,
              card["expires_at"].number.map({ $0 > Date().timeIntervalSince1970 }) ?? true else { throw AppError.message("此卡片状态未知、已过期或当前不可用，请刷新后重新选择。") }
        return Self(source: source, accountID: account, credit: card, quota: live["quota"])
    }
    func consumePayload(operationID: String) -> JSONValue {
        .object(["source_id": .string(source.id), "credit_id": credit["id"], "operation_id": .string(operationID), "expected_account_id": .string(accountID), "confirmed": .bool(true)])
    }
    func validate(live: JSONValue) throws {
        let current = try Self.make(source: source, live: live, creditID: credit["id"].string)
        guard current.accountID == accountID,
              ["status", "expires_at", "reset_type", "expiry_known"].allSatisfy({ current.credit[$0] == credit[$0] }) else { throw AppError.message("账号或卡片信息已变化，请重新确认。") }
    }
}

enum ConfigConnectionPolicy {
    static func requireCurrentPort(saved: Int, live: Double?) throws {
        guard let live, Int(live) == saved else { throw AppError.message("本地端口尚未生效。请重新启动 Codexpress，再预览并接管连接；恢复接管前连接仍可操作。") }
    }
}


enum OfficialCredentialMirror {
    static func requireDurable(_ value: JSONValue?) throws -> JSONValue {
        guard let value, value["access_token"].string?.isEmpty == false,
              value["account_id"].string?.isEmpty == false else { throw AppError.message("官方凭据尚未持久保存到钥匙串，请重新登录；未使用 IPC 中的旧令牌覆盖凭据。") }
        return value
    }
    static func mergedSources(_ sources: [Source], response: JSONValue, loadDurable: (String) throws -> JSONValue?) throws -> [Source] {
        var next = sources
        // An IPC result is an update notification only. Its secret may already be superseded.
        for item in response["updated_secrets"].array {
            guard let id = item["source_id"].string, let index = next.firstIndex(where: { $0.id == id && $0.official }) else { continue }
            let durable = try requireDurable(loadDurable(id))
            next[index].account_id = durable["account_id"].string
        }
        return next
    }
}
