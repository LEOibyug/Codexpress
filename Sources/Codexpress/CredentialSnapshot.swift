import Foundation

/// Memory mirror of helper-owned durable storage. The app never opens the Keychain.
struct CredentialSnapshot {
    private var values: [String: JSONValue] = [:]
    func load(_ id: String) -> JSONValue? { values[id] }
    mutating func apply(_ response: JSONValue, requested ids: [String]) throws {
        let items = response["items"].array
        var remaining = Set(ids)
        guard items.count == ids.count, remaining.count == ids.count else { throw AppError.message("凭据快照不完整，未更新来源。") }
        var updates: [String: JSONValue] = [:]
        for item in items {
            guard case .object(let fields) = item, let secret = fields["secret"],
                  let id = fields["source_id"]?.string, remaining.remove(id) != nil else { throw AppError.message("凭据快照无效，未更新来源。") }
            switch secret { case .null, .object: updates[id] = secret; default: throw AppError.message("凭据快照无效，未更新来源。") }
        }
        for (id, value) in updates { values[id] = value == .null ? nil : value }
    }
}
