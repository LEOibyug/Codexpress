import XCTest
@testable import Codexpress

final class RemoteSourceSettingsTests: XCTestCase {
    private var empty: JSONValue { .object(["sources": .array([]), "settings": .object(["mode": .string("auto"), "upstream_attempts": .number(7)])]) }

    func testImportCopiesOnlySelectedMetadataAndNeverRequestsOfficialCredentials() throws {
        var config = try RemoteSourceConfiguration(empty)
        var local = Source.newOfficial(); local.account_id = "private-account"; local.proxy.url = "http://localhost:1234"
        func forbidden() -> JSONValue? { XCTFail("credential mirror was read"); return nil }
        XCTAssertThrowsError(try config.importSource(local, includeKey: true, credential: forbidden()))
        try config.importSource(local, includeKey: false, credential: forbidden())
        let row = try XCTUnwrap(config.payload()["sources"].array.first)
        XCTAssertNotEqual(row["id"].string, local.id); XCTAssertEqual(row["enabled"], .bool(false))
        XCTAssertEqual(row["proxy"], .object(["mode": .string("system")]))
        XCTAssertEqual(row["account_id"], .null); XCTAssertEqual(row["api_key"], .null)
    }

    func testAPIKeyRequiresOptInAndOnlyKeyFieldCanCrossBoundary() throws {
        var config = try RemoteSourceConfiguration(empty)
        var local = Source.newGoogle(); local.proxy = ProxyPolicy(mode: "explicit", url: "http://localhost:7890")
        let fixture: JSONValue = .object(["api_key": .string("fixture-key"), "access_token": .string("fixture-oauth")])
        try config.importSource(local, includeKey: false, credential: fixture)
        try config.importSource(local, includeKey: true, credential: fixture)
        let wire = try config.payload(), rows = wire["sources"].array
        XCTAssertEqual(rows.count, 2); XCTAssertEqual(rows[0]["api_key"], .null); XCTAssertEqual(rows[1]["api_key"], .string("fixture-key")); XCTAssertEqual(rows[1]["access_token"], .null)
        XCTAssertNotEqual(rows[0]["id"], rows[1]["id"]); XCTAssertEqual(rows[1]["proxy"], .object(["mode": .string("direct")]))
        XCTAssertEqual(wire["settings"]["upstream_attempts"], .number(7))
    }

    func testRemovingFixedSourceDoesNotLeaveRoutingOrResetConsentBehind() throws {
        var cfg = try RemoteSourceConfiguration(empty); var source = Source.newOfficial(); source.proxy = ProxyPolicy(mode: "system"); cfg.apply(source, key: "")
        let row = try XCTUnwrap(cfg.payload()["sources"].array.first)
        var draft = try RemoteSourceConfiguration(.object(["sources": .array([row]), "settings": .object(["mode": .string("fixed"), "fixed_source_id": .string(source.id), "auto_reset_weekly_accounts": .array([.string(source.id)])])]))
        draft.remove(source.id); let wire = try draft.payload()
        XCTAssertEqual(wire["sources"], .array([])); XCTAssertEqual(wire["settings"]["mode"], .string("auto")); XCTAssertEqual(wire["settings"]["fixed_source_id"], .null); XCTAssertEqual(wire["settings"]["auto_reset_weekly_accounts"], .array([]))
    }

    func testBrowserLoginAndCallbackAreBoundToProviderClientLoopbackAndState() throws {
        let callback = "http://localhost:1455/auth/callback"
        let url = "https://auth.openai.com/oauth/authorize?client_id=app_EMoamEEZ73f0CkXaXp7hrann&response_type=code&code_challenge_method=S256&code_challenge=test&state=expected&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"
        let login = try XCTUnwrap(RemoteLoginLink(url)); XCTAssertTrue(login.accepts(callback + "?code=test&state=expected"))
        for (from, to) in [("auth.openai.com", "auth.openai.com.example"), ("localhost%3A1455", "example.com"), ("app_EMoamEEZ73f0CkXaXp7hrann", "other") ] { XCTAssertNil(RemoteLoginLink(url.replacingOccurrences(of: from, with: to))) }
        for raw in [callback + "?code=test&state=wrong", callback + "?state=expected&code=x&code=y", "http://localhost:1455@elsewhere.invalid/auth/callback?code=test&state=expected"] { XCTAssertFalse(login.accepts(raw)) }
    }
}
