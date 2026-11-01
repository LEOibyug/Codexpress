import XCTest
@testable import Codexpress

final class OfficialCreditsTests: XCTestCase {
    private func card(_ id: String, expires: JSONValue = .null, known: Bool = true, status: String = "available") -> JSONValue {
        .object(["id": .string(id), "status": .string(status), "expiry_known": .bool(known), "expires_at": expires])
    }

    func testQuickResetUsesFreshInventoryAndEarliestUsableCard() {
        let later = card("later", expires: .number(253402300798))
        let earlier = card("earlier", expires: .number(253402300797))
        let invalid = [card("expired", expires: .number(1)), card("unknown", known: false), card("used", status: "redeemed"), card("")]
        var snapshot: [String: JSONValue] = ["available_count": .number(3), "credits": .array([later, card("no-expiry"), earlier] + invalid), "stale": .bool(false)]
        let inventory = CreditInventory(.object(snapshot))
        XCTAssertEqual(inventory.usableCards.compactMap { $0["id"].string }, ["earlier", "later", "no-expiry"])
        XCTAssertTrue(inventory.hasAvailableCredits)
        snapshot["stale"] = .bool(true)
        let stale = CreditInventory(.object(snapshot))
        XCTAssertTrue(stale.hasAvailableCredits, "The cached count still offers a refresh-and-confirm shortcut")
        XCTAssertTrue(stale.usableCards.isEmpty, "A stale inventory must never supply a submit-ready card")
        XCTAssertNil(CreditInventory(.null).availableCount)
        XCTAssertEqual(CreditInventory(.object(["available_count": .number(0)])).countLabel, "0")
    }

    func testExtraModelLimitDoesNotShowMainQuotaExhaustion() {
        let extra: JSONValue = .object(["name": .string("other:primary"), "used_percent": .number(100), "window_seconds": .number(18000)])
        let primary: JSONValue = .object(["name": .string("primary"), "used_percent": .number(10), "window_seconds": .number(18000)])
        let weekly: JSONValue = .object(["name": .string("secondary"), "used_percent": .number(100), "window_seconds": .number(604800)])
        XCTAssertFalse(QuotaOverview(.object(["windows": .array([extra, primary])])).isExhausted)
        XCTAssertTrue(QuotaOverview(.object(["windows": .array([primary, weekly])])).isExhausted)
        XCTAssertFalse(QuotaOverview(.null).isExhausted)
    }
}
