import XCTest
@testable import Codexpress

final class ReviewRegressionTests: XCTestCase {
    private func live(_ source: Source, account: String, stale: Bool = false) throws -> JSONValue {
        guard case .object(var metadata) = try JSONValue.from(source) else { throw AppError.message("invalid fixture") }
        metadata["account_id"] = .string(account)
        metadata["credits"] = .object(["stale": .bool(stale), "error": .null, "credits": .array([
            .object(["id": .string("card"), "title": .string("Test card"), "status": .string("available"), "expiry_known": .bool(true), "expires_at": .number(Date().timeIntervalSince1970 + 3600)])
        ])])
        return .object(metadata)
    }
    func testEditorCannotReplaceNewOfficialIdentityAndResetBindsDisplayedAccount() throws {
        var oldEditor = Source.newOfficial(); oldEditor.account_id = "account-A"
        var current = oldEditor; current.account_id = "account-B"
        let currentLive = try live(current, account: "account-B")
        oldEditor.name = "New display name"
        let saved = oldEditor.preservingOfficialIdentity(current: current, live: currentLive)
        XCTAssertEqual(saved.name, "New display name")
        XCTAssertEqual(saved.account_id, "account-B")
        XCTAssertThrowsError(try CreditConfirmation.make(source: oldEditor, live: currentLive, creditID: "card"))
        let confirmation = try CreditConfirmation.make(source: saved, live: currentLive, creditID: "card")
        XCTAssertEqual(confirmation.accountID, "account-B")
        XCTAssertEqual(confirmation.consumePayload(operationID: "operation")["expected_account_id"].string, "account-B")
        XCTAssertNoThrow(try confirmation.validate(live: currentLive))
        XCTAssertThrowsError(try confirmation.validate(live: live(current, account: "account-C")))
        XCTAssertThrowsError(try confirmation.validate(live: live(current, account: "account-B", stale: true)))
        XCTAssertThrowsError(try confirmation.validate(live: .null))
    }
    func testDurableOfficialCredentialsWinOverStaleIPCAndMissingDurableBlocksMirror() throws {
        var official = Source.newOfficial(); official.account_id = "account-A"
        var api = Source(); api.name = "API"
        let stale: JSONValue = .object(["access_token": .string("stale-generation"), "account_id": .string("account-A")])
        let durable: JSONValue = .object(["access_token": .string("new-generation"), "refresh_token": .string("rotated"), "account_id": .string("account-B")])
        let response: JSONValue = .object(["updated_secrets": .array([
            .object(["source_id": .string(official.id), "secret": stale]),
            .object(["source_id": .string(api.id), "secret": stale])
        ])])
        var reads: [String] = []
        let merged = try OfficialCredentialMirror.mergedSources([official, api], response: response) { id in reads.append(id); return durable }
        XCTAssertEqual(reads, [official.id])
        XCTAssertEqual(merged[0].account_id, "account-B")
        XCTAssertNil(merged[1].account_id)
        XCTAssertEqual(try OfficialCredentialMirror.requireDurable(durable)["access_token"].string, "new-generation")
        XCTAssertThrowsError(try OfficialCredentialMirror.mergedSources([official], response: response, loadDurable: { _ in nil }))
    }
    func testPendingPortCannotCreateConnectionPreviewOrAttachment() {
        XCTAssertNoThrow(try ConfigConnectionPolicy.requireCurrentPort(saved: 17863, live: 17863))
        XCTAssertThrowsError(try ConfigConnectionPolicy.requireCurrentPort(saved: 17864, live: 17863)) { error in
            XCTAssertTrue(error.localizedDescription.contains("重新启动"))
        }
        XCTAssertThrowsError(try ConfigConnectionPolicy.requireCurrentPort(saved: 17863, live: nil))
    }
    func testBrokenMetadataEntersReadOnlyWithoutOverwritingOriginal() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("cx-corrupt-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let file = directory.appendingPathComponent("sources.json")
        let original = Data("{broken source metadata".utf8)
        try original.write(to: file)
        try await MainActor.run {
            let model = AppModel(stateDirectory: directory)
            XCTAssertTrue(model.metadataLoadFailed)
            var replacement = model.settings; replacement.port = 19000
            XCTAssertThrowsError(try model.persistSettings(replacement))
            XCTAssertEqual(model.settings.port, 17863)
        }
        XCTAssertEqual(try Data(contentsOf: file), original)
    }
    func testMetadataWriteFailureDoesNotPublishDesiredRouting() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("cx-save-fail-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = MetadataStore(directory: directory)
        var original = AppSettings(); var source = Source(); source.name = "Source"; original.sources = [source]
        try store.save(original)
        let model = await MainActor.run { AppModel(stateDirectory: directory) }
        // A directory at the atomic-rename target reliably simulates save failure, even under admin.
        try FileManager.default.removeItem(at: store.file)
        try FileManager.default.createDirectory(at: store.file, withIntermediateDirectories: false)
        let sourceID = source.id
        try await MainActor.run {
            var replacement = model.settings; replacement.mode = "fixed"; replacement.fixed_source_id = sourceID
            XCTAssertThrowsError(try model.persistSettings(replacement))
            XCTAssertEqual(model.settings.mode, "auto")
            XCTAssertNil(model.settings.fixed_source_id)
        }
    }
}
