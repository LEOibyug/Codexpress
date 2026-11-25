import XCTest
import Darwin
@testable import Codexpress

@MainActor
final class RemoteGatewayManagementTests: XCTestCase {
    private struct Reply {
        let code: Int
        let json: String
        init(_ json: String, code: Int = 200) { self.code = code; self.json = json }
    }

    private static let status = Reply(#"{"instance_id":"server-one","protocol":1,"version":"1.2","capabilities":["pool.edit"]}"#)
    private static let pool = Reply(#"{"instance_id":"server-one","revision":"r1","mode":"auto","fixed_source_id":null,"sources":[{"id":"a","kind":"api","name":"A","enabled":true,"priority":10},{"id":"b","kind":"official","name":"B","enabled":false,"priority":20}]}"#)

    @MainActor private final class Peer {
        var replies: [Reply] = []
        var requests: [URLRequest] = []
        var beforeReply: ((URLRequest) async -> Void)?

        func manager(_ directory: URL) -> RemoteGatewayManagement {
            RemoteGatewayManagement(directory: directory) { request in
                self.requests.append(request)
                guard !self.replies.isEmpty else { XCTFail("意外的管理请求：\(request.url?.path ?? "")"); throw URLError(.badServerResponse) }
                let reply = self.replies.removeFirst()
                if let hook = self.beforeReply { await hook(request) }
                let url = try XCTUnwrap(request.url)
                let response = try XCTUnwrap(HTTPURLResponse(url: url, statusCode: reply.code, httpVersion: "HTTP/1.1", headerFields: nil))
                return (Data(reply.json.utf8), response)
            }
        }
    }

    private func directory() throws -> URL {
        let project = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent().resolvingSymlinksInPath()
        let dir = project.appendingPathComponent(".runtime/tmp/remote-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        addTeardownBlock { try FileManager.default.removeItem(at: dir) }
        return dir
    }

    func testVerifiedProfileRotationAndOfflineSelectionSurviveRestart() async throws {
        let dir = try directory(), peer = Peer(), fm = FileManager.default
        let local = dir.appendingPathComponent("sources.json"), profile = dir.appendingPathComponent("remote-management.json")
        let sentinel = try JSONEncoder().encode(AppSettings())
        try sentinel.write(to: local)
        let manager = peer.manager(dir)
        peer.replies = [Self.status, Self.pool]
        let first = await manager.connect(url: "HTTPS://Alpha.Example:443/", token: "test-first-credential")
        XCTAssertTrue(first)
        XCTAssertEqual(manager.endpoint, "https://alpha.example")
        XCTAssertEqual(manager.version, "1.2")
        let original = try Data(contentsOf: profile)
        for (url, mode) in [(dir, 0o700), (profile, 0o600)] {
            let attrs = try fm.attributesOfItem(atPath: url.path)
            XCTAssertEqual((attrs[.posixPermissions] as? NSNumber)?.intValue, mode)
            XCTAssertEqual((attrs[.ownerAccountID] as? NSNumber)?.uint32Value, geteuid())
        }
        peer.replies = [Reply("test-sensitive-server-error", code: 401)]
        let rejected = await manager.connect(url: "https://beta.example", token: "test-invalid")
        XCTAssertFalse(rejected)
        XCTAssertFalse(manager.message?.contains("test-sensitive-server-error") ?? true)
        XCTAssertEqual(peer.requests.last?.value(forHTTPHeaderField: "Authorization"), "Bearer test-invalid")
        XCTAssertEqual(manager.endpoint, "https://alpha.example")
        XCTAssertTrue(manager.remoteSelected)
        XCTAssertNil(manager.snapshot)
        XCTAssertEqual(try Data(contentsOf: profile), original)

        peer.replies = [Self.status, Self.pool]
        let rotated = await manager.connect(url: "https://beta.example", token: "test-second-credential")
        XCTAssertTrue(rotated)
        XCTAssertNil(manager.message)
        let resumed = peer.manager(dir)
        XCTAssertTrue(resumed.remoteSelected)
        XCTAssertNil(resumed.snapshot)
        peer.replies = [Self.status, Self.pool]
        await resumed.refresh()
        XCTAssertNotNil(resumed.snapshot)
        XCTAssertEqual(peer.requests.last?.url?.host, "beta.example")
        XCTAssertEqual(peer.requests.last?.value(forHTTPHeaderField: "Authorization"), "Bearer test-second-credential")
        peer.replies = [Reply(Self.status.json.replacingOccurrences(of: "server-one", with: "replacement"))]
        await resumed.refresh()
        XCTAssertTrue(resumed.remoteSelected)
        XCTAssertNil(resumed.snapshot)
        XCTAssertEqual(peer.requests.last?.url?.path, "/admin/v1/status")
        resumed.selectLocal()
        let restarted = peer.manager(dir)
        let count = peer.requests.count
        XCTAssertFalse(restarted.remoteSelected)
        await restarted.refresh()
        XCTAssertEqual(peer.requests.count, count)
        XCTAssertEqual(restarted.endpoint, "https://beta.example")
        peer.replies = [Self.status, Self.pool]
        let selected = await restarted.selectRemote()
        XCTAssertTrue(selected)
        XCTAssertEqual(try Data(contentsOf: local), sentinel)
        XCTAssertEqual(Set(try fm.contentsOfDirectory(atPath: dir.path)), ["sources.json", "remote-management.json"])
        XCTAssertFalse(try String(contentsOf: profile, encoding: .utf8).contains("test-first-credential"))

        let model = AppModel(stateDirectory: dir)
        XCTAssertTrue(model.gatewayManagement.remoteSelected)
        await model.start()
        XCTAssertFalse(model.running)
        XCTAssertFalse(fm.fileExists(atPath: dir.appendingPathComponent("gateway-runtime").path))
        XCTAssertFalse(fm.fileExists(atPath: dir.appendingPathComponent("control.sock").path))
    }

    func testRejectsInvalidRootsAndIncompatibleServersBeforeCommitting() async throws {
        let dir = try directory(), peer = Peer()
        let manager = peer.manager(dir)
        for url in ["http://gateway.example", "https://u:p@gateway.example", "https://gateway.example/admin", "https://gateway.example?", "https://gateway.example/#fragment", "https://gateway.example/%2e", "https://gateway.example:0", "https://gateway.example:65536", "https://gateway.example:999999999999999999999", "https://gateway.example:", "https://gateway.example\\@other.example"] {
            let ok = await manager.connect(url: url, token: "test-value")
            XCTAssertFalse(ok, url)
        }
        let invalidToken = await manager.connect(url: "https://gateway.example", token: "a\r\nb")
        XCTAssertFalse(invalidToken)
        XCTAssertTrue(peer.requests.isEmpty)
        for response in [Reply(#"{"instance_id":"one","protocol":2,"version":"1","capabilities":["pool.edit"]}"#),
                         Reply(#"{"instance_id":"one","protocol":1,"version":"1"}"#), Reply("redacted", code: 307)] {
            peer.replies = [response]
            let ok = await manager.connect(url: "https://gateway.example", token: "test-value")
            XCTAssertFalse(ok)
            XCTAssertEqual(peer.requests.last?.url?.path, "/admin/v1/status")
        }
        peer.replies = [Self.status, Reply(Self.pool.json.replacingOccurrences(of: "server-one", with: "server-two"))]
        let mismatch = await manager.connect(url: "https://gateway.example", token: "test-value")
        XCTAssertFalse(mismatch)
        XCTAssertNil(manager.snapshot)
        XCTAssertEqual(manager.endpoint, "")
        XCTAssertTrue(try FileManager.default.contentsOfDirectory(atPath: dir.path).isEmpty)
    }

    func testConflictInvalidatesDraftAndRefreshUsesNewRevision() async throws {
        let dir = try directory(), peer = Peer()
        let manager = peer.manager(dir)
        peer.replies = [Self.status, Self.pool]
        await manager.connect(url: "https://gateway.example", token: "test-admin")
        var draft = try XCTUnwrap(manager.snapshot)
        draft.sources.reverse()
        let newSource = RemotePoolAddition(id: "new", name: "新增", baseURL: "https://upstream.example/v1", apiKey: "test-upstream", models: ["model-1"])
        peer.replies = [Self.status, Reply("test-upstream", code: 409)]
        let saved = await manager.save(draft, additions: [newSource])
        XCTAssertFalse(saved)
        XCTAssertTrue(manager.remoteSelected)
        XCTAssertNil(manager.snapshot)
        XCTAssertFalse(manager.message?.contains("test-upstream") ?? true)
        let put = try XCTUnwrap(peer.requests.last)
        XCTAssertEqual(put.httpMethod, "PUT")
        XCTAssertEqual(put.url?.absoluteString, "https://gateway.example/admin/v1/pool")
        XCTAssertFalse(put.httpShouldHandleCookies)
        let body = try JSONDecoder().decode(JSONValue.self, from: XCTUnwrap(put.httpBody))
        XCTAssertEqual(body["revision"].string, "r1")
        XCTAssertEqual(body["instance_id"].string, "server-one")
        XCTAssertEqual(body["sources"].array.map { $0["id"].string }, ["b", "a"])
        XCTAssertEqual(body["sources"].array.map { $0["priority"].number }, [0, 1])
        XCTAssertEqual(body["additions"].array.first?["api_key"].string, "test-upstream")
        let count = peer.requests.count
        let stale = await manager.save(draft, additions: [])
        XCTAssertFalse(stale)
        XCTAssertEqual(peer.requests.count, count)
        let r2 = Reply(Self.pool.json.replacingOccurrences(of: "r1", with: "r2"))
        peer.replies = [Self.status, r2]
        await manager.refresh()
        XCTAssertNil(manager.message)
        var next = try XCTUnwrap(manager.snapshot)
        next.sources.removeLast()
        let incomplete = await manager.save(next, additions: [])
        XCTAssertFalse(incomplete)
        next = try XCTUnwrap(manager.snapshot)
        next.sources[0].name = "修改"
        peer.replies = [Self.status, Reply(r2.json.replacingOccurrences(of: "r2", with: "r3"))]
        let success = await manager.save(next, additions: [])
        XCTAssertTrue(success)
        XCTAssertEqual(manager.snapshot?.revision, "r3")
        XCTAssertNil(manager.message)
    }

    func testTargetSwitchDuringSaveNeverSendsEditsOrAdoptsOldResponse() async throws {
        let dir = try directory(), peer = Peer()
        let manager = peer.manager(dir)
        peer.replies = [Self.status, Self.pool]
        await manager.connect(url: "https://alpha.example", token: "alpha-admin")
        let draft = try XCTUnwrap(manager.snapshot)
        let started = expectation(description: "保存前正在验证原来的远端")
        var resume: CheckedContinuation<Void, Never>?
        peer.beforeReply = { request in
            if request.url?.host == "alpha.example" {
                await withCheckedContinuation { continuation in resume = continuation; started.fulfill() }
            }
        }
        peer.replies = [Self.status, Self.status, Self.pool]
        let pending = Task { await manager.save(draft, additions: []) }
        await fulfillment(of: [started], timeout: 2)
        manager.selectLocal()
        // 两个地址甚至可以声称相同的实例和修订，仍不能复用草稿。
        let switched = await manager.connect(url: "https://beta.example", token: "beta-admin")
        XCTAssertTrue(switched)
        resume?.resume()
        let superseded = await pending.value
        XCTAssertFalse(superseded)
        XCTAssertFalse(manager.busy)
        XCTAssertNil(manager.message)
        XCTAssertEqual(manager.endpoint, "https://beta.example")
        let oldDraft = await manager.save(draft, additions: [])
        XCTAssertFalse(oldDraft)
        XCTAssertEqual(peer.requests.filter { $0.httpMethod == "PUT" }.count, 0)
        XCTAssertEqual(peer.requests.filter { $0.url?.host == "beta.example" }.compactMap { $0.value(forHTTPHeaderField: "Authorization") }, ["Bearer beta-admin", "Bearer beta-admin"])
    }

    func testChangedBindingAndLinksCannotRestoreOrReplaceCredentials() async throws {
        let dir = try directory(), peer = Peer(), fm = FileManager.default
        let manager = peer.manager(dir)
        peer.replies = [Self.status, Self.pool]
        await manager.connect(url: "https://alpha.example", token: "fixture-secret")
        let file = dir.appendingPathComponent("remote-management.json"), saved = try Data(contentsOf: file)
        let count = peer.requests.count
        let altered = try XCTUnwrap(String(data: saved, encoding: .utf8)).replacingOccurrences(of: "alpha.example", with: "other.example")
        try Data(altered.utf8).write(to: file)
        try fm.setAttributes([.posixPermissions: 0o600], ofItemAtPath: file.path)
        let changed = peer.manager(dir)
        XCTAssertTrue(changed.remoteSelected)
        await changed.refresh()
        XCTAssertEqual(changed.endpoint, "")
        XCTAssertNil(changed.snapshot)
        XCTAssertEqual(peer.requests.count, count)

        try saved.write(to: file)
        try fm.setAttributes([.posixPermissions: 0o600], ofItemAtPath: file.path)
        let alias = dir.appendingPathComponent("alias", isDirectory: true)
        try fm.createSymbolicLink(at: alias, withDestinationURL: dir)
        let linkedDirectory = peer.manager(alias)
        linkedDirectory.selectLocal()
        XCTAssertNotNil(linkedDirectory.message)
        XCTAssertEqual(try Data(contentsOf: file), saved)
        let backup = dir.appendingPathComponent("original")
        try fm.moveItem(at: file, to: backup)
        for hardLink in [false, true] {
            if hardLink { try fm.linkItem(at: backup, to: file) }
            else { try fm.createSymbolicLink(at: file, withDestinationURL: backup) }
            let linked = peer.manager(dir)
            XCTAssertNil(linked.snapshot)
            XCTAssertEqual(linked.endpoint, "")
            linked.selectLocal()
            XCTAssertNotNil(linked.message)
            XCTAssertEqual(try Data(contentsOf: backup), saved)
            try fm.removeItem(at: file)
        }
        try fm.moveItem(at: backup, to: file)
        try fm.setAttributes([.posixPermissions: 0o644], ofItemAtPath: file.path)
        let exposed = peer.manager(dir)
        await exposed.refresh()
        XCTAssertNil(exposed.snapshot)
        XCTAssertEqual(peer.requests.count, count)
    }
}
