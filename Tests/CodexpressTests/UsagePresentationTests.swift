import XCTest
@testable import Codexpress

final class UsagePresentationTests: XCTestCase {
    func testSourceTotalsWeightThroughputAndExcludeInflightRequestsFromSuccessAndLatency() {
        let rows: [JSONValue] = [
            .object(["requests":.number(4),"active_requests":.number(2),"successes":.number(1),"input_tokens":.number(300),"output_tokens":.number(10),"total_latency_ms":.number(2000),"timed_output_tokens":.number(10),"total_generation_ms":.number(1000),"total_first_token_ms":.number(300),"first_token_samples":.number(1)]),
            .object(["requests":.number(1),"successes":.number(1),"input_tokens":.number(200),"output_tokens":.number(100),"total_latency_ms":.number(6000),"timed_output_tokens":.number(100),"total_generation_ms":.number(5000),"total_first_token_ms":.number(500),"first_token_samples":.number(1),"estimated_requests":.number(1)])
        ]
        let totals = UsageTotals(rows: rows)
        XCTAssertEqual(totals.total,610)
        XCTAssertEqual(totals.finished,3)
        XCTAssertEqual(totals.successRate ?? 0,200.0/3,accuracy:0.001)
        XCTAssertEqual(totals.tps ?? 0,110000.0/6000,accuracy:0.001)
        XCTAssertEqual(totals.averageLatency ?? 0,8000.0/3,accuracy:0.001)
        XCTAssertEqual(totals.firstToken,400)
        XCTAssertTrue(totals.estimated)
        XCTAssertNil(UsageTotals(rows: []).tps)
        XCTAssertNil(UsageTotals(rows: []).successRate)
    }
}
