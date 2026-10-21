import XCTest
@testable import Codexpress

final class PersistenceTests: XCTestCase {
    func testHistoryCacheCapacityMigrationAndValidation() throws {
        let old = Data(#"{"sources":[],"mode":"auto","port":17863,"codexHome":"/temporary/codex"}"#.utf8)
        var settings = try JSONDecoder().decode(AppSettings.self, from: old)
        XCTAssertTrue(settings.historyCacheSettings.enabled)
        XCTAssertEqual(settings.historyCacheSettings.maxBytes, 5_000_000_000)
        settings.historyCache = try .parse(enabled: false, gigabytes: "0.5")
        let restored = try JSONDecoder().decode(AppSettings.self, from: JSONEncoder().encode(settings))
        XCTAssertEqual(restored.historyCacheSettings.maxBytes, 500_000_000)
        XCTAssertFalse(restored.historyCacheSettings.enabled)
        for value in ["0", "-1", "NaN", "inf", "1001", "bad"] { XCTAssertThrowsError(try HistoryCacheSettings.parse(enabled: true, gigabytes: value)) }
    }
    func testLocalProxyPortAndLegacyModelLists() throws {
        let fresh = try ProxyPolicy().withLocalPort("7890")
        XCTAssertEqual(fresh.url, "http://127.0.0.1:7890")
        XCTAssertEqual(fresh.localPort, "7890")
        XCTAssertEqual(try ProxyPolicy(mode: "explicit", url: "socks5h://localhost:7891").withLocalPort("7892").url, "socks5h://127.0.0.1:7892")
        let remote = ProxyPolicy(mode: "explicit", url: "http://proxy.example:1234")
        XCTAssertNil(remote.localPort)
        XCTAssertEqual(try remote.withLocalPort("7890").url, fresh.url)
        for invalid in ["", "0", "65536", "-1", "7890.0", "http://127.0.0.1:7890"] {
            XCTAssertThrowsError(try fresh.withLocalPort(invalid))
        }
        var source = Source(); source.models = ["manual-model"]
        // An old manual allowlist must not become an automatically overwritten cache.
        let legacy = try JSONDecoder().decode(Source.self, from: JSONEncoder().encode(source))
        XCTAssertFalse(legacy.discoversModels)
        XCTAssertNil(legacy.is_subscription)
        source.is_subscription = true
        source.automaticModelDiscovery = true
        let cached = try JSONDecoder().decode(Source.self, from: JSONEncoder().encode(source))
        XCTAssertTrue(cached.discoversModels)
        XCTAssertEqual(cached.is_subscription, true)
        XCTAssertEqual(cached.models, ["manual-model"])
    }
    func testModelCompactionMigrationAndValidation() throws {
        let legacy = Data(#"{"sources":[],"mode":"auto","port":17863,"codexHome":"/temporary/codex"}"#.utf8)
        var settings = try JSONDecoder().decode(AppSettings.self, from: legacy)
        XCTAssertEqual(settings.lowQuotaPercent,10)
        settings.officialLowQuotaPercent=10
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
