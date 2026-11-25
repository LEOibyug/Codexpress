import XCTest
@testable import Codexpress

final class RemoteGatewayConsoleTests: XCTestCase {
    private func json(_ raw: String) throws -> JSONValue { try JSONDecoder().decode(JSONValue.self, from: Data(raw.utf8)) }

    @MainActor func testUncertainResetAfterRelaunchCanOnlyLookUpTheOriginalReceipt() throws {
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent().resolvingSymlinksInPath()
        let dir = root.appendingPathComponent(".runtime/tmp/console-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        defer { try? FileManager.default.removeItem(at: dir) }
        let book = RemoteConsoleReceiptBook(directory: dir, endpoint: "https://first.invalid", instance: "same-instance")
        let attempted = try book.reserve(source: "one", binding: "account-fingerprint", card: "card-fingerprint")
        let reopened = RemoteConsoleReceiptBook(directory: dir, endpoint: "https://first.invalid", instance: "same-instance")
        let pending = try XCTUnwrap(reopened.read().first)
        XCTAssertEqual(pending.id, attempted.id)
        XCTAssertEqual(pending.resultFields, ["source_id": .string("one"), "account_binding": .string("account-fingerprint"), "operation_id": .string(attempted.id.uuidString)])
        try reopened.update(pending, result: .object(["code": .string("not_found")]))
        XCTAssertThrowsError(try reopened.reserve(source: "one", binding: pending.binding, card: pending.card))
        let unrelated = RemoteConsoleReceiptBook(directory: dir, endpoint: "https://second.invalid", instance: "same-instance")
        XCTAssertTrue(try unrelated.read().isEmpty)
        try reopened.update(pending, result: .object(["code": .string("reset"), "windows_reset": .number(2)]))
        XCTAssertEqual(try book.read().first?.windows, 2)
    }

    func testCatalogUploadRejectsNonMetadataBeforeSendingAnything() throws {
        let valid = #"{"models":[{"slug":"sample","max_context_window":4000,"auto_compact_token_limit":2000}]}"#
        let catalog = try RemoteConsoleMetadataFile(data: Data(valid.utf8))
        XCTAssertEqual(try json(catalog.json), try json(valid))
        for invalid in [#"{"models":[],"token":"sensitive"}"#, #"{"models":[{"slug":"sample","max_context_window":4000,"headers":{}}]}"#, #"{"models":[{"slug":"sample","max_context_window":4000,"auto_compact_token_limit":3700}]}"#] {
            XCTAssertThrowsError(try RemoteConsoleMetadataFile(data: Data(invalid.utf8)))
        }
        XCTAssertThrowsError(try RemoteConsoleMetadataFile(data: Data(repeating: 32, count: 262145)))
    }
}
