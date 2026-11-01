import Foundation

struct CreditResetRequest: Identifiable {
    let id = UUID()
    let sourceID: String
    let accountID: String?
    let quickReset: Bool

    init(source: Source, quickReset: Bool) {
        sourceID = source.id
        accountID = source.account_id
        self.quickReset = quickReset
    }
}

/// The same inventory rules drive the main window, menu and confirmation flow.
struct CreditInventory {
    let snapshot: JSONValue
    let cards: [JSONValue]

    init(_ snapshot: JSONValue) {
        self.snapshot = snapshot
        cards = snapshot["credits"].array.sorted {
            let first = $0["expires_at"].number ?? .greatestFiniteMagnitude
            let second = $1["expires_at"].number ?? .greatestFiniteMagnitude
            return first == second ? ($0["id"].string ?? "") < ($1["id"].string ?? "") : first < second
        }
    }

    var availableCount: Int? {
        guard let count = snapshot["available_count"].number, count.isFinite, count >= 0, count < Double(Int.max) else { return nil }
        return Int(count)
    }
    var countLabel: String { availableCount.map { String($0) } ?? "—" }
    var isFresh: Bool { snapshot["stale"].bool == false && snapshot["error"] == .null }
    var hasAvailableCredits: Bool { (availableCount ?? 0) > 0 || cards.contains { Self.isUsable($0) } }
    var usableCards: [JSONValue] { isFresh ? cards.filter { Self.isUsable($0) } : [] }

    static func isUsable(_ card: JSONValue, now: Date = Date()) -> Bool {
        guard card["id"].string?.isEmpty == false, card["status"].string == "available", card["expiry_known"].bool == true else { return false }
        return card["expires_at"].number.map { $0.isFinite && $0 > now.timeIntervalSince1970 } ?? true
    }
}

extension QuotaOverview {
    var isExhausted: Bool { mainWindows.contains { $0.remaining == 0 } }
}

struct ResetOperation: Codable, Sendable {
    let sourceID: String
    let creditID: String
    let operationID: String
    let createdAt: Double
    var result: String
    var accountID: String? = nil

    func matches(source: Source, accountID: String, creditID: String) -> Bool {
        guard self.creditID == creditID else { return false }
        if let savedAccount = self.accountID { return savedAccount == accountID }
        // Legacy records have no account binding; keep their original source protected.
        return sourceID == source.id
    }
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
        guard let card = CreditInventory(snapshot).usableCards.first(where: { $0["id"].string == creditID }) else {
            throw AppError.message("此卡片状态未知、已过期或当前不可用，请刷新后重新选择。")
        }
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
