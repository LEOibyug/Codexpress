import XCTest
@testable import Codexpress

@MainActor
final class RemoteGatewayMenuTests: XCTestCase {
    private let pool = RemotePoolSnapshot(instanceID: "menu-fixture", revision: "r1", mode: "fixed", fixedSourceID: "fixed", sources: [
        .init(id: "fixed", name: "归档服务", kind: "api", enabled: true, priority: 5),
        .init(id: "legacy", name: "兼容来源", kind: "api", enabled: true, priority: 3),
        .init(id: "account", name: "团队官方账户", kind: "official", enabled: true, priority: 1),
        .init(id: "standby", name: "备用服务", kind: "api", enabled: false, priority: 4),
        .init(id: "paid", name: "计费服务", kind: "api", enabled: true, priority: 2)
    ])
    private let runtime = Data(#"""
        {"sources":[
          {"id":"paid","health":"ready","active_requests":0,"balance":{"amount":12.5,"currency":"USD","estimated":true}},
          {"id":"standby","health":"cooldown"},
          {"id":"account","health":"ready","active_requests":2,"quota":{"windows":[{"name":"primary","window_seconds":18000,"used_percent":20},{"name":"secondary","window_seconds":604800,"used_percent":67}]}}
        ]}
        """#.utf8)

    func testPreviewJoinsByIDAndDoesNotInventMissingMeasurements() throws {
        let preview = RemoteMenuPool(pool, runtime: runtime)
        XCTAssertEqual(preview.rows.map(\.id), ["account", "paid", "legacy", "standby"])
        XCTAssertEqual(preview.count, 5)
        XCTAssertEqual(preview.routing, "固定来源 · 归档服务")
        XCTAssertEqual(preview.rows[0].quotas.compactMap(\.remaining), [80, 33])
        XCTAssertEqual(preview.rows[0].activeRequests, 2)
        XCTAssertEqual(preview.rows[1].activeRequests, 0)
        XCTAssertTrue(try XCTUnwrap(preview.rows[1].balance).hasPrefix("估算余额"))
        let legacy = preview.rows[2]
        XCTAssertNil(legacy.health)
        XCTAssertNil(legacy.activeRequests)
        XCTAssertNil(legacy.balance)
        XCTAssertFalse(preview.rows[3].source.enabled)
        XCTAssertEqual(preview.rows[3].health, "cooldown")
        let oldServer = RemoteMenuPool(pool, runtime: nil)
        XCTAssertTrue(oldServer.rows.allSatisfy { $0.quotas.isEmpty && $0.balance == nil && $0.activeRequests == nil })
    }
}
