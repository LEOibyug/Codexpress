import XCTest
import Darwin
@testable import Codexpress

final class GatewayClientTests: XCTestCase {
    func testUnixIPCFramingAndRequestIdentity() async throws {
        let path = "/tmp/cx-test-\(UUID().uuidString).sock"
        let server = socket(AF_UNIX, SOCK_STREAM, 0)
        XCTAssertGreaterThanOrEqual(server, 0)
        defer { close(server); unlink(path) }
        var address = sockaddr_un(); address.sun_family = sa_family_t(AF_UNIX)
        let capacity = MemoryLayout.size(ofValue: address.sun_path)
        withUnsafeMutablePointer(to: &address.sun_path) { ptr in ptr.withMemoryRebound(to: CChar.self, capacity: capacity) { chars in path.withCString { _ = strlcpy(chars, $0, capacity) } } }
        let bound = withUnsafePointer(to: &address) { ptr in ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { Darwin.bind(server, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) } }
        XCTAssertEqual(bound, 0); XCTAssertEqual(listen(server, 1), 0)
        let responseTask = Task.detached { () throws -> JSONValue in
            let connection = accept(server, nil, nil)
            guard connection >= 0 else { throw AppError.message("test accept failed") }
            defer { close(connection) }
            var received = Data(); var byte: UInt8 = 0
            while Darwin.read(connection, &byte, 1) == 1 { if byte == 10 { break }; received.append(byte) }
            let request = try JSONDecoder().decode(JSONValue.self, from: received)
            let reply: JSONValue = .object(["id": request["id"], "ok": .bool(true), "data": .object(["message": .string("本机响应"), "running": .bool(true)]), "error": .null])
            var encoded = try JSONEncoder().encode(reply); encoded.append(10)
            // Multiple writes exercise line framing without assuming a recv boundary.
            encoded.withUnsafeBytes { bytes in
                _ = Darwin.write(connection, bytes.baseAddress!, 7)
                _ = Darwin.write(connection, bytes.baseAddress!.advanced(by: 7), bytes.count - 7)
            }
            return request
        }
        let client = GatewayClient(socketPath: path)
        let result = try await client.call("status", .object(["probe": .bool(true)]))
        XCTAssertEqual(result["message"].string, "本机响应")
        let request = try await responseTask.value
        XCTAssertEqual(request["op"].string, "status")
        XCTAssertEqual(request["payload"]["probe"].bool, true)
        XCTAssertFalse(request["id"].string?.isEmpty ?? true)
    }
    func testRejectsOversizedRequestBeforeConnecting() async {
        let client = GatewayClient(socketPath: "/tmp/codexpress-intentionally-absent.sock")
        do {
            _ = try await client.call("sync", .string(String(repeating: "a", count: GatewayClient.limit)))
            XCTFail("oversized request accepted")
        } catch { XCTAssertTrue(error.localizedDescription.contains("8 MiB")) }
    }
    func testResetOperationIntentSurvivesRestart() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("cx-operations-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let store = ResetOperationStore(directory: directory)
        let operation = ResetOperation(sourceID: "source", creditID: "card", operationID: UUID().uuidString, createdAt: 1, result: "结果待核实")
        try store.save(["source:card": operation])
        let loaded = try store.load()
        XCTAssertEqual(loaded["source:card"]?.operationID, operation.operationID)
        XCTAssertEqual(loaded["source:card"]?.result, "结果待核实")
        XCTAssertEqual((try FileManager.default.attributesOfItem(atPath: store.file.path)[.posixPermissions] as? NSNumber)?.intValue, 0o600)
    }
}
