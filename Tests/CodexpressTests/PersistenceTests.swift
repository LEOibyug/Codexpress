import XCTest
@testable import Codexpress

final class PersistenceTests: XCTestCase {
    func testLoadBalancingDefaultsToPriorityAndPersistsOptIn() throws {
        let legacy = Data(#"{"sources":[],"mode":"auto","port":17863,"codexHome":"/temporary/codex"}"#.utf8)
        var settings = try JSONDecoder().decode(AppSettings.self, from: legacy)
        XCTAssertFalse(settings.loadBalanceEnabled)
        XCTAssertEqual(settings.idleReturnDelay, 5)
        let prior = Data(#"{"sources":[],"mode":"auto","port":17863,"codexHome":"/temporary/codex","idleReturnMinutes":30}"#.utf8)
        XCTAssertEqual(try JSONDecoder().decode(AppSettings.self, from: prior).idleReturnDelay, 5)
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("cx-load-balance-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = MetadataStore(directory: directory)
        settings.loadBalance = true
        settings.conversationStickyMinutes = 10
        try store.save(settings)
        XCTAssertTrue(try store.load().loadBalanceEnabled)
        XCTAssertEqual(try store.load().idleReturnDelay, 10)
        settings.loadBalance = false
        try store.save(settings)
        XCTAssertFalse(try store.load().loadBalanceEnabled)
    }

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
        XCTAssertNil(legacy.balanceWarningThreshold)
        source.is_subscription = true
        source.balanceWarningThreshold = 12.5
        source.automaticModelDiscovery = true
        let cached = try JSONDecoder().decode(Source.self, from: JSONEncoder().encode(source))
        XCTAssertTrue(cached.discoversModels)
        XCTAssertEqual(cached.is_subscription, true)
        XCTAssertEqual(cached.balanceWarningThreshold, 12.5)
        XCTAssertEqual(cached.models, ["manual-model"])
    }
    func testModelCompactionMigrationAndValidation() throws {
        let legacy = Data(#"{"sources":[],"mode":"auto","port":17863,"codexHome":"/temporary/codex"}"#.utf8)
        var settings = try JSONDecoder().decode(AppSettings.self, from: legacy)
        XCTAssertEqual(settings.lowQuotaPercent,10)
        settings.officialLowQuotaPercent=10
        XCTAssertNil(settings.modelCompaction)
        XCTAssertNil(settings.modelPolicies)
        let first = try ModelCompactionRule.parse(model: "model-a", threshold: "180000", window: "300000")
        let second = try ModelCompactionRule.parse(model: "model-b", threshold: "45000", window: "")
        settings.modelCompaction = [first, second]
        let policy = ModelPolicy(model: first.model, sourceIDs: [], clients: [])
        settings.modelPolicies = [policy]
        let restored = try JSONDecoder().decode(AppSettings.self, from: JSONEncoder().encode(settings))
        XCTAssertEqual(restored.modelCompaction, [first, second])
        XCTAssertEqual(restored.modelPolicies, [policy], "Explicit empty sources and clients must remain disabled, not inherit defaults")
        let inherited = try JSONDecoder().decode(ModelPolicy.self, from: Data(#"{"model":"legacy-model"}"#.utf8))
        XCTAssertNil(inherited.sourceIDs)
        XCTAssertEqual(inherited.clients, ModelClient.allCases)
        XCTAssertThrowsError(try ModelCompactionRule.validate([first, first]))
        XCTAssertThrowsError(try ModelCompactionRule.parse(model: "m", threshold: "90001", window: "100000"))
        XCTAssertThrowsError(try ModelCompactionRule.parse(model: "m", threshold: "1.5", window: ""))
        XCTAssertThrowsError(try ModelCompactionRule.parse(model: "m", threshold: "0", window: ""))
        let windowOnly = try ModelCompactionRule.parse(model: "window-only", threshold: " ", window: "100000", defaultWindow: 200000)
        XCTAssertNil(windowOnly.auto_compact_token_limit)
        XCTAssertEqual(windowOnly.context_window, 100000)
        let defaults = try ModelCompactionRule.parse(model: "defaults", threshold: "", window: "")
        XCTAssertNil(defaults.auto_compact_token_limit)
        XCTAssertNil(defaults.context_window)
        XCTAssertThrowsError(try ModelCompactionRule.parse(model: "m", threshold: "", window: "200001", defaultWindow: 200000))
        XCTAssertThrowsError(try ModelCompactionRule.parse(model: "m", threshold: "180001", window: "", defaultWindow: 200000))
        let numeric = try JSONDecoder().decode(ModelCompactionRule.self, from: Data(#"{"model":"legacy","auto_compact_token_limit":45000}"#.utf8))
        XCTAssertEqual(numeric.auto_compact_token_limit, 45000)
        XCTAssertEqual(try JSONDecoder().decode(ModelCompactionRule.self, from: JSONEncoder().encode(windowOnly)), windowOnly)
    }
    func testModelSettingsRevisionIgnoresSetOrderingButPreservesSourcePriority() throws {
        var first = AppSettings()
        XCTAssertEqual(try ModelSettingsRevision.value(for: first), ModelSettingsRevision.empty)
        first.modelCompaction = [try .parse(model: "b", threshold: "100", window: ""), try .parse(model: "a", threshold: "200", window: "")]
        first.modelPolicies = [ModelPolicy(model: "b", sourceIDs: ["one", "two"], clients: [.omp, .codex]), ModelPolicy(model: "a", clients: [])]
        let revision = try ModelSettingsRevision.value(for: first)
        var reordered = first
        reordered.modelCompaction?.reverse()
        reordered.modelPolicies?.reverse()
        reordered.modelPolicies?[1].clients.reverse()
        XCTAssertEqual(try ModelSettingsRevision.value(for: reordered), revision)
        reordered.modelPolicies?[1].sourceIDs?.reverse()
        XCTAssertNotEqual(try ModelSettingsRevision.value(for: reordered), revision)
        reordered = first
        reordered.modelCompaction?[0].auto_compact_token_limit = 101
        XCTAssertNotEqual(try ModelSettingsRevision.value(for: reordered), revision)
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

    func testChatbotEnvironmentExportDefaultsOffAndPersistsOptIn() throws {
        let legacy = Data(#"{"sources":[],"mode":"auto","port":17863,"codexHome":"/temporary/codex"}"#.utf8)
        var settings = try JSONDecoder().decode(AppSettings.self, from: legacy)
        XCTAssertFalse(settings.autoExportChatbotEnvironmentEnabled)
        settings.autoExportChatbotEnvironment = true
        let restored = try JSONDecoder().decode(AppSettings.self, from: JSONEncoder().encode(settings))
        XCTAssertTrue(restored.autoExportChatbotEnvironmentEnabled)
    }

    @MainActor func testGoogleAndOpenAISourcesDefaultPoliciesAndProviderToggles() throws {
        let google = Source.newGoogle()
        XCTAssertTrue(google.isGoogle)
        XCTAssertFalse(google.supportsBalanceQuery)
        XCTAssertTrue(google.defaultsDiscoveredModelsDisabled)
        XCTAssertEqual(google.kindLabel, "Google AI Studio")
        XCTAssertEqual(google.base_url, "https://generativelanguage.googleapis.com/v1beta")
        XCTAssertNoThrow(try google.validate())

        let openai = Source.newOpenAI()
        XCTAssertTrue(openai.isOpenAI)
        XCTAssertFalse(openai.supportsBalanceQuery)
        XCTAssertTrue(openai.defaultsDiscoveredModelsDisabled)
        XCTAssertEqual(openai.kindLabel, "OpenAI API Platform")
        XCTAssertEqual(openai.base_url, "https://api.openai.com/v1")
        XCTAssertNoThrow(try openai.validate())

        var customAPI = Source()
        customAPI.id = "custom-api"
        customAPI.name = "Custom Relay"
        customAPI.priority = 0
        customAPI.models = ["gpt-5.4", "claude-sonnet"]
        XCTAssertTrue(customAPI.supportsBalanceQuery)
        XCTAssertFalse(customAPI.defaultsDiscoveredModelsDisabled)

        var googleSource = google
        googleSource.id = "google-src"
        googleSource.priority = 1
        googleSource.models = ["gemini-2.5-pro", "gemini-2.5-flash"]

        var openaiSource = openai
        openaiSource.id = "openai-src"
        openaiSource.priority = 2
        openaiSource.models = ["gpt-5.4", "o3"]

        var settings = AppSettings()
        settings.sources = [customAPI, googleSource, openaiSource]
        // Existing model policy with sourceIDs == nil (e.g., customized clients) must keep existing non-default-disabled sources only.
        settings.modelPolicies = [ModelPolicy(model: "gpt-5.4", sourceIDs: nil, clients: [.codex, .omp])]

        AppModel.applyDefaultDisabledPolicies(for: googleSource, in: &settings)
        AppModel.applyDefaultDisabledPolicies(for: openaiSource, in: &settings)

        let geminiProPolicy = settings.modelPolicies?.first { $0.model == "gemini-2.5-pro" }
        XCTAssertEqual(geminiProPolicy?.sourceIDs, [])
        XCTAssertFalse(ModelPolicy.isEnabled("gemini-2.5-pro", in: settings))

        let gptPolicy = settings.modelPolicies?.first { $0.model == "gpt-5.4" }
        XCTAssertEqual(gptPolicy?.sourceIDs, ["custom-api"])
        XCTAssertEqual(gptPolicy?.clients, [.codex, .omp])
        XCTAssertTrue(ModelPolicy.isEnabled("gpt-5.4", on: customAPI, in: settings))
        XCTAssertFalse(ModelPolicy.isEnabled("gpt-5.4", on: openaiSource, in: settings))

        let o3Policy = settings.modelPolicies?.first { $0.model == "o3" }
        XCTAssertEqual(o3Policy?.sourceIDs, [])
        XCTAssertFalse(ModelPolicy.isEnabled("o3", in: settings))

        // Enabling a model on one provider does not re-disable already known models on refresh.
        try ModelPolicy.setProviderModelsEnabled(sourceID: googleSource.id, modelIDs: ["gemini-2.5-pro"], enabled: true, in: &settings)
        XCTAssertTrue(ModelPolicy.isEnabled("gemini-2.5-pro", on: googleSource, in: settings))
        XCTAssertFalse(ModelPolicy.isEnabled("gemini-2.5-flash", on: googleSource, in: settings))

        googleSource.models.append("gemini-3-flash-preview")
        AppModel.applyDefaultDisabledPolicies(for: googleSource, previousModels: ["gemini-2.5-pro", "gemini-2.5-flash"], in: &settings)
        XCTAssertTrue(ModelPolicy.isEnabled("gemini-2.5-pro", on: googleSource, in: settings))
        XCTAssertFalse(ModelPolicy.isEnabled("gemini-3-flash-preview", on: googleSource, in: settings))

        try ModelPolicy.setModelsEnabled(["gemini-2.5-flash"], enabled: true, in: &settings)
        XCTAssertTrue(ModelPolicy.isEnabled("gemini-2.5-flash", in: settings))
        try ModelPolicy.setModelsEnabled(["gemini-2.5-flash"], enabled: false, in: &settings)
        XCTAssertFalse(ModelPolicy.isEnabled("gemini-2.5-flash", in: settings))
    }

    func testShowInCmuxDefaultsTrueAndPersistsFalse() throws {
        let legacyData = Data(#"{"id":"legacy-src","name":"Legacy","kind":"api","base_url":"https://api.example.com/v1","enabled":true,"priority":0,"proxy":{"mode":"direct","url":null},"models":[]}"#.utf8)
        let decoded = try JSONDecoder().decode(Source.self, from: legacyData)
        XCTAssertNil(decoded.show_in_cmux)
        XCTAssertTrue(decoded.showInCmux)

        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("codexpress-cmux-toggle-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: directory) }

        var hidden = decoded
        hidden.show_in_cmux = false
        XCTAssertFalse(hidden.showInCmux)

        let store = MetadataStore(directory: directory)
        var settings = AppSettings()
        settings.sources = [hidden]
        try store.save(settings)
        let loaded = try store.load()
        XCTAssertEqual(loaded.sources.first?.show_in_cmux, false)
        XCTAssertEqual(loaded.sources.first?.showInCmux, false)
    }

    func testSystemProxyModeValidatesAndPersistsForAllSourceKinds() throws {
        var api = Source(); api.name = "API System"
        var settings = AppSettings()
        settings.sources = [.newOfficial(), api, .newGoogle(), .newOpenAI()]
        for index in settings.sources.indices {
            settings.sources[index].proxy = ProxyPolicy(mode: "system")
            XCTAssertEqual(settings.sources[index].policyLabel, "系统代理")
            XCTAssertNoThrow(try settings.sources[index].validate())
        }

        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("codexpress-sysproxy-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = MetadataStore(directory: directory)
        try store.save(settings)
        let loaded = try store.load()
        XCTAssertEqual(loaded.sources.map(\.proxy.mode), ["system", "system", "system", "system"])
        XCTAssertEqual(loaded.sources.map(\.proxy.url), [nil, nil, nil, nil])
    }
}
