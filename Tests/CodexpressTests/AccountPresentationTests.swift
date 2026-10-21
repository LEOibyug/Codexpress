import XCTest
import AppKit
@testable import Codexpress

final class AccountPresentationTests: XCTestCase {
    func testBalanceAndQuotaColorBoundariesAndUnknownValues() {
        for (amount, expected): (Double, AccountLevel) in [(51, .healthy), (50, .caution), (21, .caution), (20, .caution), (11, .caution), (10, .low), (0, .low), (-1, .low)] {
            XCTAssertEqual(AccountLevel.balance(amount, threshold: 10), expected)
        }
        XCTAssertEqual(AccountLevel.balance(1, threshold: 0), .healthy)
        XCTAssertEqual(AccountLevel.balance(0, threshold: 0), .low)
        XCTAssertEqual(AccountLevel.balance(100, threshold: nil), .unknown)
        XCTAssertEqual(AccountLevel.balance(.nan, threshold: 10), .unknown)
        XCTAssertEqual(AccountLevel.balance(100, threshold: -1), .unknown)
        for (remaining, expected): (Double, AccountLevel) in [(100, .healthy), (31, .healthy), (30, .caution), (11, .caution), (10, .low), (0, .low)] {
            XCTAssertEqual(AccountLevel.quota(remaining), expected)
        }
        XCTAssertEqual(AccountLevel.quota(nil), .unknown)
        XCTAssertEqual(AccountLevel.quota(.infinity), .unknown)
        let invalid = QuotaWindow(.object(["used_percent": .number(.nan)]), index: 0)
        XCTAssertEqual(invalid.remainingLabel, "未知")
    }

    func testCurrencyUnitIsSeparateAndPrecisionIsPreserved() {
        let usd = BalanceAmount(1234.5, currency: "USD")
        XCTAssertEqual(usd.unit, "US$")
        XCTAssertEqual(usd.number, "1,234.50")
        XCTAssertEqual(balanceMoney(1234.5, currency: "USD"), "US$ 1,234.50")
        XCTAssertEqual(BalanceAmount(0.0012, currency: "CNY").number, "0.0012")
        XCTAssertFalse(BalanceAmount(1, currency: "CNY").unit.isEmpty)
    }

    func testMenuIconKeepsSizeAndDrawsGreenActivityInBothAppearances() throws {
        for dark in [false, true] {
            for active in [false, true] {
                let image = GatewayMenuIcon.image(running: true, active: active, dark: dark)
                XCTAssertEqual(image.size, NSSize(width: 18, height: 18))
                XCTAssertFalse(image.isTemplate)
                let representation = try XCTUnwrap(NSBitmapImageRep(data: XCTUnwrap(image.tiffRepresentation)))
                // Bitmap coordinates start at the top; the marker is bottom-right.
                let color = try XCTUnwrap(representation.colorAt(x: representation.pixelsWide * 14 / 18,
                                                                y: representation.pixelsHigh * 14 / 18)?.usingColorSpace(.deviceRGB))
                if active { XCTAssertGreaterThan(color.greenComponent, color.redComponent + 0.1) }
                else { XCTAssertLessThan(abs(color.greenComponent - color.redComponent), 0.05) }
            }
        }
    }
}
