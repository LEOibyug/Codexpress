import CryptoKit
import Foundation

enum RemoteIngressPurpose: String, CaseIterable, Codable {
    case codex, chat, images, omp

    var title: String {
        switch self { case .codex: "Codex"; case .chat: "聊天客户端"; case .images: "图片生成"; case .omp: "OMP" }
    }

    func baseURL(endpoint: String) -> String {
        let path: String = switch self { case .codex: "/v1"; case .chat: "/chat/v1"; case .images: "/images/v1"; case .omp: "/omp/v1" }
        return endpoint + path // The management profile has a canonical HTTPS root with no trailing slash.
    }
}

struct RemoteIngressCredential: Codable, Equatable {
    let id: String
    let key: String

    init(_ result: JSONValue) throws {
        try self.init(id: result["id"].string, key: result["key"].string)
    }

    init(from decoder: Decoder) throws {
        let fields = try decoder.container(keyedBy: CodingKeys.self)
        try self.init(id: fields.decode(String.self, forKey: .id), key: fields.decode(String.self, forKey: .key))
    }

    private init(id: String?, key: String?) throws {
        guard let id, UUID(uuidString: id) != nil, let key, key.hasPrefix("cxr-"), key.utf8.count == 68,
              key.utf8.dropFirst(4).allSatisfy({ (48...57).contains($0) || (97...102).contains($0) }) else { throw RemoteConsoleError.invalid }
        self.id = id; self.key = key
    }
}

/// One manually managed device credential per gateway, regardless of inference path.
@MainActor struct RemoteIngressKeys {
    struct Record: Codable, Equatable {
        let name: String
        var credential: RemoteIngressCredential?
    }
    private struct Contents: Codable {
        let endpoint: String
        let instance: String
        var device: Record?
    }

    private let file: RemoteManagementFile
    private let endpoint: String
    private let instance: String

    init(directory: URL, endpoint: String, instance: String) {
        self.endpoint = endpoint; self.instance = instance
        let hash = SHA256.hash(data: Data((endpoint + "\0" + instance).utf8)).map { String(format: "%02x", $0) }.joined()
        file = RemoteManagementFile(directory: directory, name: "remote-client-device-\(hash).json")
    }

    private func read() throws -> Contents {
        do {
            let contents = try JSONDecoder().decode(Contents.self, from: file.read())
            guard contents.endpoint == endpoint, contents.instance == instance else { throw RemoteConsoleError.invalid }
            return contents
        } catch let e as CocoaError where e.code == .fileReadNoSuchFile {
            return Contents(endpoint: endpoint, instance: instance, device: nil)
        }
    }

    private func write(_ contents: Contents) throws { try file.replace(with: JSONEncoder().encode(contents)) }

    /// Call only with the IDs returned by a successful list for this target; a failed list must not erase the cache.
    func verified(activeIDs: Set<String>) throws -> Record? {
        var contents = try read()
        if let id = contents.device?.credential?.id, !activeIDs.contains(id) { contents.device = nil; try write(contents) }
        return contents.device
    }

    func reserve() throws -> String {
        var contents = try read()
        guard contents.device == nil else { throw RemoteConsoleError.invalid }
        let name = "Mac · \(UUID().uuidString.prefix(8))"
        contents.device = Record(name: name)
        try write(contents); return name
    }

    func finish(reservation: String, credential: RemoteIngressCredential) throws {
        var contents = try read()
        guard contents.device == Record(name: reservation) else { throw RemoteConsoleError.invalid }
        contents.device?.credential = credential
        try write(contents)
    }

    func discardUnconfirmed(reservation: String, listed: [RemoteConsoleDevice]) throws {
        var contents = try read()
        guard contents.device == Record(name: reservation), !listed.contains(where: { $0.name == reservation }) else { throw RemoteConsoleError.invalid }
        contents.device = nil
        try write(contents)
    }
}
