import CryptoKit
import Foundation

/// Local IPC by atomic file replacement. cmux never opens the file that carries the OMP device key.
@MainActor
final class GatewayIntegrationFiles {
    private struct Target: Codable, Equatable {
        let endpoint: String
        let instanceID: String
    }
    private struct Device: Codable {
        let target: Target
        let id: String
        let key: String
    }
    private let directory: URL
    private var target: Target?
    private var generation = UUID()
    private var issuing: UUID?

    init(directory: URL) { self.directory = directory }

    private func file(_ name: String) -> RemoteManagementFile {
        RemoteManagementFile(directory: directory, name: name)
    }

    private func write(_ fields: [String: Any], to name: String) throws {
        var document = fields
        document["version"] = 1
        try file(name).replace(with: JSONSerialization.data(withJSONObject: document))
    }

    private static let ompFile = "gateway-integrations.json"
    private static let cmuxFile = "cmux-gateway.json"

    private func reset() {
        target = nil
        generation = UUID()
        issuing = nil
    }

    func invalidate() throws {
        reset()
        // Attempt both even if one consumer has a damaged file.
        let omp = Result { try write(["mode": "unavailable"], to: Self.ompFile) }
        try write(["mode": "unavailable"], to: Self.cmuxFile)
        try omp.get()
    }

    func selectLocal() throws {
        try select(["mode": "local"], target: nil)
    }

    func selectRemote(endpoint: String, instanceID: String) throws {
        let next = Target(endpoint: endpoint, instanceID: instanceID)
        guard next != target else { return }
        try select(fields(next), target: next)
    }

    private func select(_ fields: [String: Any], target next: Target?) throws {
        do {
            try invalidate()
            try write(fields, to: Self.ompFile)
            try write(fields, to: Self.cmuxFile)
            target = next
        } catch {
            try? invalidate()
            throw error
        }
    }

    private func fields(_ target: Target) -> [String: Any] {
        ["mode": "remote", "endpoint": target.endpoint, "instance_id": target.instanceID]
    }

    private func deviceFile(_ target: Target) -> RemoteManagementFile {
        let identity = Data((target.endpoint + "\0" + target.instanceID).utf8)
        let hash = SHA256.hash(data: identity).map { String(format: "%02x", $0) }.joined()
        return file("omp-device-\(hash).json")
    }

    private func validKey(_ key: String) -> Bool {
        key.hasPrefix("cxr-") && key.utf8.count == 68 && key.utf8.dropFirst(4).allSatisfy { (48...57).contains($0) || (97...102).contains($0) || (65...70).contains($0) }
    }

    private func device(for target: Target) throws -> Device? {
        do {
            let device = try JSONDecoder().decode(Device.self, from: deviceFile(target).read())
            guard device.target == target, !device.id.isEmpty, validKey(device.key) else { throw CocoaError(.fileReadCorruptFile) }
            return device
        } catch let error as CocoaError where error.code == .fileReadNoSuchFile { return nil }
    }

    /// The only non-idempotent request here is `issue`. Never retry it in this operation.
    func prepareOMP(endpoint: String, instanceID: String, issue: () async throws -> (id: String, key: String)) async -> String? {
        let requested = Target(endpoint: endpoint, instanceID: instanceID)
        guard requested == target, issuing == nil else { return nil }
        let ticket = generation
        issuing = ticket
        defer { if issuing == ticket { issuing = nil } }
        do {
            let credential: Device
            if let saved = try device(for: requested) { credential = saved }
            else {
                let response = try await issue()
                guard generation == ticket, target == requested else { return nil }
                guard !response.id.isEmpty, validKey(response.key) else { throw CocoaError(.fileReadCorruptFile) }
                credential = Device(target: requested, id: response.id, key: response.key)
                try deviceFile(requested).replace(with: JSONEncoder().encode(credential))
            }
            var omp = fields(requested)
            omp["device_key"] = credential.key
            try write(omp, to: Self.ompFile)
            return nil
        } catch {
            guard generation == ticket else { return nil }
            return "OMP 设备凭证未就绪。若签发结果未知，请先核对远端设备再重试。"
        }
    }

    func publish(pool: RemotePoolSnapshot, runtime: Data?, fetchedAt: Date) throws {
        guard let target, target.instanceID == pool.instanceID else { return }
        var snapshot: [String: Any] = ["running": true, "mode": pool.mode]
        snapshot["fixed_source_id"] = pool.fixedSourceID
        var byID: [String: [String: Any]] = [:]
        if let runtime, let value = try JSONSerialization.jsonObject(with: runtime) as? [String: Any] {
            snapshot["inflight"] = value["inflight"] as? Int
            for row in value["sources"] as? [[String: Any]] ?? [] {
                if let id = row["id"] as? String { byID[id] = row }
            }
        }
        // Use the editable pool as the authority for membership. Never forward arbitrary JSON from a remote server.
        snapshot["sources"] = pool.sources.map { source -> [String: Any] in
            var row: [String: Any] = ["id": source.id, "name": source.name, "kind": source.kind,
                                      "enabled": source.enabled, "priority": source.priority]
            guard let state = byID[source.id] else { return row }
            for key in ["health", "active_requests", "show_in_cmux", "cooldown_until", "capacity_available", "max_active_conversations", "capacity_active_conversations"] {
                if let v = state[key], v is NSNumber || (key == "health" && v is String) { row[key] = v }
            }
            for (key, allowed) in Self.metricFields {
                if let metric = state[key] as? [String: Any] {
                    var filtered = metric.filter { allowed.contains($0.key) && ($0.value is NSNumber || $0.value is String) }
                    if key == "quota", let windows = metric["windows"] as? [[String: Any]] {
                        filtered["windows"] = windows.map { $0.filter { ["name", "used_percent", "reset_at", "window_seconds", "ref_cost"].contains($0.key) && ($0.value is NSNumber || $0.value is String) } }
                    }
                    row[key] = filtered
                }
            }
            return row
        }
        var envelope = fields(target)
        envelope["snapshot"] = snapshot
        envelope["fetched_at"] = fetchedAt.timeIntervalSince1970
        try write(envelope, to: Self.cmuxFile)
    }

    private static let metricFields: [String: Set<String>] = [
        "quota": ["fetched_at", "stale", "failure_count", "next_retry_at"],
        "credits": ["available_count", "stale", "fetched_at", "failure_count", "next_retry_at"],
        "balance": ["status", "amount", "currency", "scope", "stale", "checked_at", "updated_at", "failure_count", "next_retry_at", "estimated", "unknown_spend_requests"],
        "admission": ["remaining_percent", "accepting_new_conversations", "active_conversations", "account_active_conversations", "capacity_available", "max_active_conversations", "capacity_active_conversations"]
    ]
}
