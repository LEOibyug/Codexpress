import XCTest
@testable import Codexpress

final class PersistenceTests: XCTestCase {
    func testModelCompactionMigrationAndValidation() throws {
        let legacy = Data(#"{"sources":[],"mode":"auto","port":17863,"codexHome":"/temporary/codex"}"#.utf8)
        var settings = try JSONDecoder().decode(AppSettings.self, from: legacy)
        XCTAssertNil(settings.modelCompaction)
        let first = try ModelCompactionRule.parse(model: "model-a", threshold: "180000", window: "300000")
        let second = try ModelCompactionRule.parse(model: "model-b", threshold: "45000", window: "")
        settings.modelCompaction = [first, second]
        let restored = try JSONDecoder().decode(AppSettings.self, from: JSONEncoder().encode(settings))
        XCTAssertEqual(restored.modelCompaction, [first, second])
        XCTAssertThrowsError(try ModelCompactionRule.validate([first, first]))
        XCTAssertThrowsError(try ModelCompactionRule.parse(model: "m", threshold: "90001", window: "100000"))
        XCTAssertThrowsError(try ModelCompactionRule.parse(model: "m", threshold: "1.5", window: ""))
        XCTAssertThrowsError(try ModelCompactionRule.parse(model: "m", threshold: "0", window: ""))
    }
    func testMetadataPersistsWithoutSecretAndRestrictsPermissions() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("codexpress-test-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = MetadataStore(directory: directory)
        var settings = AppSettings()
        var source = Source.newOfficial(); source.proxy.url = "http://127.0.0.1:19876"; source.account_id = "test-account"
        settings.sources = [source]; settings.mode = "fixed"; settings.fixed_source_id = source.id
        try store.save(settings)
        let loaded = try store.load()
        XCTAssertEqual(loaded.sources, [source]); XCTAssertEqual(loaded.fixed_source_id, source.id)
        let raw = try String(contentsOf: store.file)
        XCTAssertFalse(raw.contains("secret")); XCTAssertFalse(raw.contains("access_token")); XCTAssertFalse(raw.contains("api_key"))
        let attrs = try FileManager.default.attributesOfItem(atPath: store.file.path)
        XCTAssertEqual((attrs[.posixPermissions] as? NSNumber)?.intValue, 0o600)
        let directoryAttrs = try FileManager.default.attributesOfItem(atPath: directory.path)
        XCTAssertEqual((directoryAttrs[.posixPermissions] as? NSNumber)?.intValue, 0o700)
        settings.sources = []; try store.save(settings)
        XCTAssertTrue(try store.load().sources.isEmpty)
    }
    func testOfficialRequiresExplicitValidProxyAndRejectsSecretsInURLs() throws {
        var source = Source.newOfficial()
        XCTAssertThrowsError(try source.validate())
        source.proxy.url = "socks5h://127.0.0.1:19999"; XCTAssertNoThrow(try source.validate())
        source.proxy.mode = "direct"; XCTAssertThrowsError(try source.validate())
        source = Source(); source.name = "Local test"; source.base_url = "http://127.0.0.1:19999/v1"; XCTAssertNoThrow(try source.validate())
        source.base_url = "https://example.com/v1?api_key=private"; XCTAssertThrowsError(try source.validate())
        source.base_url = "https://private@example.com/v1"; XCTAssertThrowsError(try source.validate())
    }
}
