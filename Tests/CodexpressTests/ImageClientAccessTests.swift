import XCTest
@testable import Codexpress

final class ImageClientAccessTests: XCTestCase {
    func testPrivateConnectionExportRotationAndPromptContainNoKey() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("cx-image-client-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        let key = "PRIVATE-IMAGE-CLIENT-KEY"
        let file = try ImageClientAccess.export(baseURL: "http://127.0.0.1:17866/v1", key: key, directory: directory)
        let stored = try JSONDecoder().decode([String:String].self, from: Data(contentsOf: file))
        XCTAssertEqual(stored["api_key"], key)
        XCTAssertEqual((try FileManager.default.attributesOfItem(atPath: file.path)[.posixPermissions] as? NSNumber)?.intValue, 0o600)
        XCTAssertEqual((try FileManager.default.attributesOfItem(atPath: directory.path)[.posixPermissions] as? NSNumber)?.intValue, 0o700)
        let prompt = ImageClientAccess.prompt(bundle: URL(fileURLWithPath: "/Applications/My Codexpress.app"), config: file)
        XCTAssertFalse(prompt.contains(key))
        XCTAssertTrue(prompt.contains("/Applications/My Codexpress.app/Contents/Resources/skills/codexpress-image/SKILL.md"))
        XCTAssertTrue(prompt.contains(file.path))
        _ = try ImageClientAccess.export(baseURL: "http://localhost:17867/v1", key: "ROTATED-IMAGE-KEY", directory: directory)
        let updated = try JSONDecoder().decode([String:String].self, from: Data(contentsOf: file))
        XCTAssertEqual(updated["api_key"], "ROTATED-IMAGE-KEY")
        XCTAssertEqual(updated["base_url"], "http://localhost:17867/v1")
    }
    func testExportRejectsRemoteAddressesAndSymlinksWithoutOverwritingTargets() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("cx-image-path-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        for url in ["http://example.com:123/v1", "https://127.0.0.1:123/v1", "http://key@127.0.0.1:123/v1", "http://127.0.0.1:123/v1?key=secret", "http://127.0.0.1:123/other"] {
            XCTAssertThrowsError(try ImageClientAccess.export(baseURL: url, key: "LOCAL-IMAGE-KEY-123", directory: directory))
        }
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let target = directory.appendingPathComponent("untouched.json")
        try Data("unchanged".utf8).write(to: target)
        try FileManager.default.createSymbolicLink(at: directory.appendingPathComponent("image-client.json"), withDestinationURL: target)
        XCTAssertThrowsError(try ImageClientAccess.export(baseURL: "http://127.0.0.1:123/v1", key: "LOCAL-IMAGE-KEY-123", directory: directory))
        XCTAssertEqual(try String(contentsOf: target), "unchanged")
    }
}
