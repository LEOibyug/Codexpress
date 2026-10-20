import Foundation
import Security

struct KeychainStore {
    // Scope dev/isolated instances separately; never query another app's items.
    let service: String
    init(stateDirectory: URL) { service = "com.codexpress.upstream." + stateDirectory.path.data(using: .utf8)!.base64EncodedString() }
    private func query(_ id: String) -> [String: Any] { [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service, kSecAttrAccount as String: id] }
    func load(_ id: String) throws -> JSONValue? {
        var q = query(id); q[kSecReturnData as String] = true; q[kSecMatchLimit as String] = kSecMatchLimitOne
        var result: CFTypeRef?; let status = SecItemCopyMatching(q as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess, let data = result as? Data else { throw AppError.message("无法读取 Codexpress 钥匙串凭据（\(status)）。") }
        return try JSONDecoder().decode(JSONValue.self, from: data)
    }
    func save(_ secret: JSONValue, for id: String) throws {
        let data = try JSONEncoder().encode(secret)
        let q = query(id)
        var status = SecItemUpdate(q as CFDictionary, [kSecValueData as String: data] as CFDictionary)
        if status == errSecItemNotFound {
            var add = q; add[kSecValueData as String] = data; add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
            status = SecItemAdd(add as CFDictionary, nil)
        }
        guard status == errSecSuccess else { throw AppError.message("无法保存 Codexpress 钥匙串凭据（\(status)）。") }
    }
    func delete(_ id: String) throws {
        let status = SecItemDelete(query(id) as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else { throw AppError.message("无法删除此来源的钥匙串凭据（\(status)）。") }
    }
}
