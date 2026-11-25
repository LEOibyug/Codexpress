import XCTest
@testable import Codexpress

@MainActor
final class GatewayIntegrationFilesTests: XCTestCase {
    func testDeviceIdentityAndSidebarDataDoNotCrossTargetsOrConsumers() async throws {
        let project = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent().resolvingSymlinksInPath()
        let dir = project.appendingPathComponent(".runtime/tmp/integrations-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let bridge = GatewayIntegrationFiles(directory: dir)
        let a = "https://a.example", b = "https://b.example", keyA = "cxr-" + String(repeating: "a", count: 64)
        let keyB = "cxr-" + String(repeating: "b", count: 64)
        func record(_ filename: String) throws -> [String: Any] {
            let file = dir.appendingPathComponent(filename)
            let attributes = try FileManager.default.attributesOfItem(atPath: file.path)
            XCTAssertEqual((attributes[.posixPermissions] as? NSNumber)?.intValue, 0o600)
            return try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: file)) as? [String: Any])
        }
        let omp = "gateway-integrations.json", cmux = "cmux-gateway.json"
        try bridge.selectRemote(endpoint: a, instanceID: "one")
        let issued = await bridge.prepareOMP(endpoint: a, instanceID: "one") { ("device-a", keyA) }
        XCTAssertNil(issued)
        XCTAssertEqual(try record(omp)["device_key"] as? String, keyA)
        let pool = RemotePoolSnapshot(instanceID: "one", revision: "r1", mode: "auto", sources: [.init(id: "src", name: "A", kind: "api", enabled: true, priority: 0)])
        let runtime = Data(#"{"inflight":2,"sources":[{"id":"src","active_requests":2,"api_key":"private-upstream","balance":{"amount":0.5,"raw":"private-upstream"}}]}"#.utf8)
        try bridge.publish(pool: pool, runtime: runtime, fetchedAt: Date())
        let json = try Data(contentsOf: dir.appendingPathComponent(cmux)); let payload = String(decoding: json, as: UTF8.self)
        XCTAssertFalse(payload.contains(keyA)); XCTAssertFalse(payload.contains("private-upstream"))
        let snapshot = try XCTUnwrap(try record(cmux)["snapshot"] as? [String: Any]); let rows = try XCTUnwrap(snapshot["sources"] as? [[String: Any]])
        XCTAssertEqual(rows.first?["active_requests"] as? Int, 2)
        try bridge.selectRemote(endpoint: b, instanceID: "two")
        XCTAssertNil(try record(omp)["device_key"]); XCTAssertNil(try record(cmux)["snapshot"])
        // A delayed mint must never publish B's key after switching back to A.
        _ = await bridge.prepareOMP(endpoint: b, instanceID: "two") {
            try bridge.selectRemote(endpoint: a, instanceID: "one")
            return ("device-b", keyB)
        }
        XCTAssertNil(try record(omp)["device_key"])
        let reused = await bridge.prepareOMP(endpoint: a, instanceID: "one") { XCTFail("unexpected second mint"); return ("", "") }
        XCTAssertNil(reused); XCTAssertEqual(try record(omp)["device_key"] as? String, keyA)
        try bridge.selectLocal()
        XCTAssertNil(try record(omp)["device_key"]); XCTAssertEqual(try record(cmux)["mode"] as? String, "local")
    }
}
