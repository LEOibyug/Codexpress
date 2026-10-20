import XCTest
@testable import Codexpress

final class CredentialSnapshotTests: XCTestCase {
    func testDurableSnapshotReplacesRotatedTokensAndRejectsIncompleteUpdatesAtomically() throws {
        var snapshot = CredentialSnapshot()
        let old: JSONValue = .object(["access_token": .string("fixture-old"), "account_id": .string("account")])
        let fresh: JSONValue = .object(["access_token": .string("fixture-new"), "refresh_token": .string("fixture-rotated"), "account_id": .string("account")])
        func response(_ secret: JSONValue) -> JSONValue { .object(["items": .array([.object(["source_id": .string("official"), "secret": secret])])]) }
        try snapshot.apply(response(old), requested: ["official"])
        try snapshot.apply(response(fresh), requested: ["official"])
        XCTAssertEqual(try OfficialCredentialMirror.requireDurable(snapshot.load("official")), fresh)
        XCTAssertThrowsError(try snapshot.apply(response(.null), requested: ["official", "missing"]))
        XCTAssertEqual(try snapshot.load("official"), fresh)
        XCTAssertThrowsError(try snapshot.apply(response(.string("invalid")), requested: ["official"]))
        XCTAssertEqual(try snapshot.load("official"), fresh)
        try snapshot.apply(response(.null), requested: ["official"])
        XCTAssertNil(try snapshot.load("official"))
    }
}
