import XCTest
@testable import Codexpress

final class RemoteIngressKeysTests: XCTestCase {
    private func directory() throws -> URL {
        let project = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent().resolvingSymlinksInPath()
        let directory = project.appendingPathComponent(".runtime/tmp/ingress-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        return directory
    }

    @MainActor func testOnlyVerifiedKeysFromTheOriginalEndpointAndInstanceSurviveRelaunch() throws {
        let dir = try directory(), fm = FileManager.default
        defer { try? fm.removeItem(at: dir) }
        let endpoint = "https://alpha.invalid", instance = "instance-a"
        let first = RemoteIngressKeys(directory: dir, endpoint: endpoint, instance: instance)
        let key = "cxr-" + String(repeating: "a", count: 64), device = UUID().uuidString
        let issued = try RemoteIngressCredential(.object(["id": .string(device), "key": .string(key)]))
        let name = try first.reserve()
        try first.finish(reservation: name, credential: issued)
        let files = try fm.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil)
        XCTAssertEqual(files.count, 1)
        let file = try XCTUnwrap(files.first)
        XCTAssertEqual(try fm.attributesOfItem(atPath: file.path)[.posixPermissions] as? NSNumber, 0o600)

        let reopened = RemoteIngressKeys(directory: dir, endpoint: endpoint, instance: instance)
        let verified = try reopened.verified(activeIDs: [device, UUID().uuidString])
        XCTAssertEqual(verified?.credential?.key, key)
        XCTAssertThrowsError(try reopened.reserve())
        for other in [RemoteIngressKeys(directory: dir, endpoint: "https://beta.invalid", instance: instance),
                      RemoteIngressKeys(directory: dir, endpoint: endpoint, instance: "instance-b")] {
            XCTAssertNil(try other.verified(activeIDs: [device]))
        }
        // Removing a revoked key is durable; an unrelated later device list cannot resurrect it.
        XCTAssertNil(try reopened.verified(activeIDs: []))
        XCTAssertNil(try first.verified(activeIDs: [device]))
    }

    @MainActor func testLostIssueReplyCannotBeRetriedOrRecoveredByMatchingADeviceName() throws {
        let dir = try directory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let first = RemoteIngressKeys(directory: dir, endpoint: "https://alpha.invalid", instance: "a")
        let name = try first.reserve()
        let reopened = RemoteIngressKeys(directory: dir, endpoint: "https://alpha.invalid", instance: "a")
        let existing = RemoteConsoleDevice(id: UUID().uuidString, name: name, host: nil, port: nil)
        XCTAssertThrowsError(try reopened.reserve())
        let pending = try XCTUnwrap(reopened.verified(activeIDs: [existing.id]))
        XCTAssertNil(pending.credential)
        XCTAssertThrowsError(try reopened.discardUnconfirmed(reservation: name, listed: [existing]))
        try reopened.discardUnconfirmed(reservation: name, listed: [])
        XCTAssertNotEqual(try reopened.reserve(), name)
    }
}
