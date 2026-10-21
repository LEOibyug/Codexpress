import XCTest
@testable import Codexpress

final class BalanceWarningsTests: XCTestCase {
    private func source() -> Source {
        var source = Source()
        source.name = "测试 API"
        source.base_url = "https://api.example.com/v1"
        source.balanceWarningThreshold = 10
        return source
    }
    private func balance(_ amount: Double, stale: Bool = false, currency: String = "USD") -> JSONValue {
        .object(["status": .string("available"), "amount": .number(amount), "stale": .bool(stale), "currency": .string(currency)])
    }

    func testAlertPersistsRetriesDeliveryAndOnlyRearmsAfterFreshRecovery() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("cx-balance-alert-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        let source = source(), key = "PRIVATE-TEST-KEY"
        var monitor = try BalanceWarnings(directory: directory)
        let first = try XCTUnwrap(monitor.evaluate(source: source, balance: balance(10), apiKey: key))
        let id = try XCTUnwrap(first["id"].string)
        XCTAssertTrue(first["body"].string?.contains("测试 API 余额不足") == true)
        XCTAssertTrue(first["body"].string?.contains("10.00") == true)
        XCTAssertEqual(try monitor.evaluate(source: source, balance: balance(9), apiKey: key)?["id"].string, id)
        monitor = try BalanceWarnings(directory: directory)
        XCTAssertEqual(try monitor.evaluate(source: source, balance: balance(9), apiKey: key)?["id"].string, id)
        try monitor.acknowledge([id])
        monitor = try BalanceWarnings(directory: directory)
        XCTAssertNil(try monitor.evaluate(source: source, balance: balance(8), apiKey: key))
        XCTAssertNil(try monitor.evaluate(source: source, balance: balance(100, stale: true), apiKey: key))
        XCTAssertNil(try monitor.evaluate(source: source, balance: .object(["status":.string("unavailable")]), apiKey: key))
        XCTAssertNil(try monitor.evaluate(source: source, balance: balance(7), apiKey: key))
        XCTAssertNil(try monitor.evaluate(source: source, balance: balance(11), apiKey: key))
        let renewed = try XCTUnwrap(monitor.evaluate(source: source, balance: balance(9), apiKey: key)?["id"].string)
        XCTAssertNotEqual(renewed, id)
        let file = directory.appendingPathComponent("balance-warnings.json")
        XCTAssertFalse(try String(contentsOf: file).contains(key))
        XCTAssertEqual((try FileManager.default.attributesOfItem(atPath: file.path)[.posixPermissions] as? NSNumber)?.intValue, 0o600)
    }

    func testUnusableBalancesDisabledSourcesAndOfficialAccountsNeverWarn() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("cx-balance-gates-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        var monitor = try BalanceWarnings(directory: directory)
        var source = source()
        for invalid in [JSONValue.null, balance(0, stale: true), balance(0, currency: ""), balance(.nan), .object(["status":.string("unavailable"),"amount":.number(0),"stale":.bool(false),"currency":.string("USD")])] {
            XCTAssertNil(try monitor.evaluate(source: source, balance: invalid, apiKey: "key"))
        }
        source.enabled = false
        XCTAssertNil(try monitor.evaluate(source: source, balance: balance(0), apiKey: "key"))
        source.enabled = true; source.kind = "official"
        XCTAssertNil(try monitor.evaluate(source: source, balance: balance(0), apiKey: "key"))
        source.kind = "api"; source.balanceWarningThreshold = nil
        XCTAssertNil(try monitor.evaluate(source: source, balance: balance(0), apiKey: "key"))
        for invalid in [-1.0, .infinity, .nan] {
            source.balanceWarningThreshold = invalid
            XCTAssertThrowsError(try source.validate())
            XCTAssertNil(try monitor.evaluate(source: source, balance: balance(0), apiKey: "key"))
        }
        source.balanceWarningThreshold = 0
        XCTAssertNoThrow(try source.validate())
        XCTAssertNotNil(try monitor.evaluate(source: source, balance: balance(0), apiKey: "key"))
    }

    func testIdentityThresholdCurrencyAndWarningSwitchHaveIndependentEpisodes() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("cx-balance-identity-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: directory) }
        var monitor = try BalanceWarnings(directory: directory)
        var source = source()
        var id = try XCTUnwrap(monitor.evaluate(source: source, balance: balance(5), apiKey: "key-A")?["id"].string)
        try monitor.acknowledge([id])
        source.name = "重命名"; source.priority = 5
        XCTAssertNil(try monitor.evaluate(source: source, balance: balance(5), apiKey: "key-A"))
        for (threshold, currency, key, endpoint) in [(20.0, "USD", "key-A", source.base_url), (20.0, "USD", "key-B", source.base_url), (20.0, "CNY", "key-B", source.base_url), (20.0, "CNY", "key-B", "https://other.example.com/v1")] {
            source.balanceWarningThreshold = threshold; source.base_url = endpoint
            let next = try XCTUnwrap(monitor.evaluate(source: source, balance: balance(5, currency: currency), apiKey: key)?["id"].string)
            XCTAssertNotEqual(next, id)
            id = next
            try monitor.acknowledge([id])
        }
        source.balanceWarningThreshold = nil
        try monitor.retain(sources: [source])
        source.balanceWarningThreshold = 20
        let reenabled = try XCTUnwrap(monitor.evaluate(source: source, balance: balance(5, currency: "CNY"), apiKey: "key-B")?["id"].string)
        try monitor.acknowledge([reenabled])
        var other = source; other.id = UUID().uuidString; other.balanceWarningThreshold = 2
        XCTAssertNil(try monitor.evaluate(source: other, balance: balance(5, currency: "CNY"), apiKey: "key-B"))
        XCTAssertNotNil(try monitor.evaluate(source: other, balance: balance(2, currency: "CNY"), apiKey: "key-B"))
        XCTAssertNil(try monitor.evaluate(source: source, balance: balance(5, currency: "CNY"), apiKey: "key-B"))
    }
}
