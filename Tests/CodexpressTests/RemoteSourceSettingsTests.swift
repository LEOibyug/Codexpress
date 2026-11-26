import XCTest
@testable import Codexpress

@MainActor
final class RemoteSourceSettingsTests: XCTestCase {
    private var empty: JSONValue { .object(["sources": .array([]), "settings": .object(["mode": .string("auto"), "upstream_attempts": .number(7)])]) }

    func testCompactionDefaultsFollowAdapterWithoutMakingThemExplicitInTheDraft() throws {
        let cases: [(Source, Bool?)] = [(.newOfficial(), true), (Source(), false), (.newOpenAI(), false), (.newGoogle(), nil)]
        for (source, expected) in cases {
            var local = source; local.name = "来源"
            var config = try RemoteConsoleConfig(empty)
            try config.importSource(local, includeKey: false, credential: nil)
            let draft = try RemoteConsoleConfig.source(XCTUnwrap(config.sources.first))
            XCTAssertNil(draft.remote_compaction_v2, draft.kind)
            XCTAssertEqual(draft.effectiveRemoteCompactionV2, expected, draft.kind)
        }
    }

    func testImportCopiesOnlySelectedMetadataAndNeverRequestsOfficialCredentials() throws {
        var config = try RemoteConsoleConfig(empty)
        var local = Source.newOfficial(); local.account_id = "private-account"; local.proxy.url = "http://localhost:1234"
        func forbidden() -> JSONValue? { XCTFail("credential mirror was read"); return nil }
        XCTAssertThrowsError(try config.importSource(local, includeKey: true, credential: forbidden()))
        try config.importSource(local, includeKey: false, credential: forbidden())
        let row = try XCTUnwrap(config.sources.first)
        XCTAssertNotEqual(row["id"].string, local.id); XCTAssertEqual(row["enabled"], .bool(false))
        XCTAssertEqual(row["proxy"], .object(["mode": .string("system")]))
        XCTAssertEqual(row["account_id"], .null); XCTAssertEqual(row["api_key"], .null)
    }

    func testAPIKeyRequiresOptInAndOnlyKeyFieldCanCrossBoundary() throws {
        var config = try RemoteConsoleConfig(empty)
        var local = Source.newGoogle(); local.proxy = ProxyPolicy(mode: "explicit", url: "http://localhost:7890")
        local.remote_compaction_v2 = true // A stale preference cannot enable the native Google adapter.
        XCTAssertNil(local.effectiveRemoteCompactionV2)
        let fixture: JSONValue = .object(["api_key": .string("fixture-key"), "access_token": .string("fixture-oauth")])
        try config.importSource(local, includeKey: false, credential: fixture)
        try config.importSource(local, includeKey: true, credential: fixture)
        let rows = config.sources
        XCTAssertEqual(rows.count, 2); XCTAssertEqual(rows[0]["api_key"], .null); XCTAssertEqual(rows[1]["api_key"], .string("fixture-key")); XCTAssertEqual(rows[1]["access_token"], .null)
        XCTAssertNotEqual(rows[0]["id"], rows[1]["id"]); XCTAssertEqual(rows[1]["proxy"], .object(["mode": .string("direct")]))
        XCTAssertEqual(rows[1]["remote_compaction_v2"], .null)
        XCTAssertEqual(config.settings["upstream_attempts"], .number(7))
    }

    func testRemovingFixedSourceDoesNotLeaveRoutingOrResetConsentBehind() throws {
        var config = try RemoteConsoleConfig(empty), source = Source.newOfficial(); source.proxy = ProxyPolicy(mode: "system")
        try config.stage(source, key: "", saved: nil)
        config.settings["mode"] = .string("fixed"); config.settings["fixed_source_id"] = .string(source.id)
        config.settings["auto_reset_weekly_accounts"] = .array([.string(source.id)])
        try config.remove(source.id)
        XCTAssertTrue(config.sources.isEmpty); XCTAssertEqual(config.settings["mode"], .string("auto")); XCTAssertEqual(config.settings["fixed_source_id"], .null); XCTAssertEqual(config.settings["auto_reset_weekly_accounts"], .array([]))
    }

    func testStagedKeySurvivesUnrelatedEditsButCannotFollowAnAddressChange() throws {
        var source = Source(); source.name = "A"
        guard case .object(var fields) = try JSONValue.from(source) else { return XCTFail() }
        fields["future_field"] = .string("preserve")
        let saved = JSONValue.object(fields)
        var config = RemoteConsoleConfig(sources: [saved], settings: [:])
        try config.stage(source, key: "test-original-destination", saved: saved)
        source.name = "B"; try config.stage(source, key: "", saved: saved)
        XCTAssertEqual(config.sources[0]["api_key"], .string("test-original-destination"))
        source.base_url = "https://other.example/v1"
        XCTAssertThrowsError(try config.stage(source, key: "", saved: saved))
        XCTAssertEqual(config.sources[0]["base_url"], saved["base_url"])
        try config.stage(source, key: "test-new-destination", saved: saved)
        XCTAssertEqual(config.sources[0]["api_key"], .string("test-new-destination"))
        XCTAssertEqual(config.sources[0]["future_field"], .string("preserve"))
    }

    func testBrowserLoginAndCallbackAreBoundToProviderClientLoopbackAndState() throws {
        let callback = "http://localhost:1455/auth/callback"
        let url = "https://auth.openai.com/oauth/authorize?client_id=app_EMoamEEZ73f0CkXaXp7hrann&response_type=code&code_challenge_method=S256&code_challenge=test&state=expected&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"
        let login = try XCTUnwrap(RemoteLoginLink(url)); XCTAssertTrue(login.accepts(callback + "?code=test&state=expected"))
        for (from, to) in [("auth.openai.com", "auth.openai.com.example"), ("localhost%3A1455", "example.com"), ("app_EMoamEEZ73f0CkXaXp7hrann", "other") ] { XCTAssertNil(RemoteLoginLink(url.replacingOccurrences(of: from, with: to))) }
        for raw in [callback + "?code=test&state=wrong", callback + "?state=expected&code=x&code=y", "http://localhost:1455@elsewhere.invalid/auth/callback?code=test&state=expected"] { XCTAssertFalse(login.accepts(raw)) }
    }
}
