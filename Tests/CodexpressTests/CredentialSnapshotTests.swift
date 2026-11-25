import XCTest
@testable import Codexpress

final class CredentialSnapshotTests: XCTestCase {
    func testDurableSnapshotReplacesRotatedTokensAndRejectsIncompleteUpdatesAtomically() throws {
        var snapshot = CredentialSnapshot()
        let old: JSONValue = .object(["access_token": .string("fixture-old"), "account_id": .string("account")])
        let fresh: JSONValue = .object(["access_token": .string("fixture-new"), "refresh_token": .string("fixture-rotated"), "account_id": .string("account")])
        func item(_ id: String, _ secret: JSONValue) -> JSONValue { .object(["source_id": .string(id), "secret": secret]) }
        func response(_ secret: JSONValue) -> JSONValue { .object(["items": .array([item("official", secret)])]) }
        try snapshot.apply(response(old), requested: ["official"])
        try snapshot.apply(response(fresh), requested: ["official"])
        XCTAssertEqual(snapshot.load("official"), fresh)
        XCTAssertThrowsError(try snapshot.apply(response(.null), requested: ["official", "missing"]))
        XCTAssertEqual(snapshot.load("official"), fresh)
        for invalid in [item("missing", .string("invalid")), item("official", .null), item("unexpected", .null)] {
            let partial: JSONValue = .object(["items": .array([item("official", .null), invalid])])
            XCTAssertThrowsError(try snapshot.apply(partial, requested: ["official", "missing"]))
            XCTAssertEqual(snapshot.load("official"), fresh, "The valid first item must not be applied on failure")
        }
        try snapshot.apply(response(.null), requested: ["official"])
        XCTAssertNil(snapshot.load("official"))
    }
}
