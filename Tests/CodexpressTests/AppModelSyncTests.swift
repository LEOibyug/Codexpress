import XCTest
import Darwin
@testable import Codexpress

final class AppModelSyncTests: XCTestCase {
    @MainActor func testSettingChangesReuseTheStatusRefreshFromSync() async throws {
        let directory = URL(fileURLWithPath: "/tmp/cx-sync-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let helper = directory.appendingPathComponent("gateway-fixture")
        try Self.helperScript.write(to: helper, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: helper.path)
        let previousHelper = ProcessInfo.processInfo.environment["CODEXPRESS_GATEWAY_PATH"]
        setenv("CODEXPRESS_GATEWAY_PATH", helper.path, 1)
        defer {
            if let previousHelper { setenv("CODEXPRESS_GATEWAY_PATH", previousHelper, 1) }
            else { unsetenv("CODEXPRESS_GATEWAY_PATH") }
        }
        var first = Source(); first.name = "First"; first.automaticModelDiscovery = false
        var second = Source(); second.name = "Second"; second.automaticModelDiscovery = false; second.priority = 1
        var settings = AppSettings(); settings.sources = [first, second]; settings.codexHome = directory.appendingPathComponent("codex").path
        try MetadataStore(directory: directory).save(settings)
        let model = AppModel(stateDirectory: directory)
        defer { model.stop() }
        await model.startHelper()
        XCTAssertTrue(model.running, model.error ?? "Fixture helper did not start")
        guard model.running else { return }
        let requests = directory.appendingPathComponent("requests")
        let syncPayload = directory.appendingPathComponent("last-sync.json")
        func syncedLoadBalance() throws -> JSONValue {
            try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: syncPayload))["load_balance"]
        }
        XCTAssertEqual(try syncedLoadBalance(), .bool(false))
        var retryPayload = try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: syncPayload))
        XCTAssertEqual(retryPayload["upstream_attempts"], .number(3))
        XCTAssertEqual(retryPayload["upstream_retry_delay_seconds"], .number(3))

        func expectOneRefresh(_ action: () -> Void) async throws {
            try Data().write(to: requests)
            action()
            let deadline = Date().addingTimeInterval(5)
            while model.busy && Date() < deadline { try await Task.sleep(for: .milliseconds(10)) }
            XCTAssertFalse(model.busy, "Fixture management operation timed out")
            XCTAssertNil(model.error)
            let recorded = try operations()
            XCTAssertEqual(recorded.filter { $0 == "sync" }.count, 1)
            XCTAssertEqual(recorded.filter { $0 == "status" }.count, 1)
        }
        func operations() throws -> [String] {
            try String(contentsOf: requests).split(separator: "\n").map(String.init)
        }
        try await expectOneRefresh { model.changeRouting(mode: "fixed", fixed: first.id) }
        XCTAssertEqual(model.settings.fixed_source_id, first.id)
        try await expectOneRefresh { model.changeLowQuotaThreshold(20) }
        try await expectOneRefresh { model.changeContextFallback(false) }
        try await expectOneRefresh { model.changeLoadBalance(true) }
        XCTAssertEqual(try syncedLoadBalance(), .bool(true))
        XCTAssertTrue(try MetadataStore(directory: directory).load().loadBalanceEnabled)
        try await expectOneRefresh { model.changeLoadBalance(false) }
        XCTAssertEqual(try syncedLoadBalance(), .bool(false))
        XCTAssertFalse(try MetadataStore(directory: directory).load().loadBalanceEnabled)
        try await expectOneRefresh { model.changeIdleReturnDelay(60) }
        try await expectOneRefresh { model.changeUpstreamRetry(attempts: 5, delaySeconds: 7) }
        retryPayload = try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: syncPayload))
        XCTAssertEqual(retryPayload["upstream_attempts"], .number(5))
        XCTAssertEqual(retryPayload["upstream_retry_delay_seconds"], .number(7))
        let persisted = try MetadataStore(directory: directory).load()
        XCTAssertEqual(persisted.upstreamAttemptLimit, 5)
        XCTAssertEqual(persisted.upstreamRetryDelay, 7)
        try await expectOneRefresh { model.move(IndexSet(integer: 0), to: 2) }
        XCTAssertEqual(model.sources.first?.id, second.id)

        try Data().write(to: requests)
        var edited = try XCTUnwrap(model.settings.sources.first { $0.id == first.id })
        edited.name = "Renamed"
        try await model.saveSource(edited, apiKey: "")
        // Saving first checks current identity, then sync performs the final refresh.
        let recorded = try operations()
        XCTAssertEqual(recorded.filter { $0 == "sync" }.count, 1)
        XCTAssertEqual(recorded.filter { $0 == "status" }.count, 2)
        XCTAssertEqual(model.settings.sources.first { $0.id == first.id }?.name, "Renamed")

        func expectManualOperation(_ operation: String, _ action: () -> Void) async throws {
            try Data().write(to: requests)
            action()
            let deadline = Date().addingTimeInterval(5)
            while model.busy && Date() < deadline { try await Task.sleep(for: .milliseconds(10)) }
            XCTAssertFalse(model.busy)
            XCTAssertNil(model.error)
            let recorded = try operations()
            XCTAssertEqual(recorded.filter { $0 == operation }.count, 1)
            XCTAssertEqual(recorded.filter { $0 == "status" }.count, 1)
            XCTAssertFalse(recorded.contains("sync"))
        }
        try await expectManualOperation("routing.recover") { model.recoverSource(first.id) }
        try await expectManualOperation("routing.try") { model.startSourceTakeover(second.id) }
        XCTAssertEqual(model.status["manual_route"]["source_id"].string, second.id)
        try await expectManualOperation("routing.cancel") { model.cancelSourceTakeover() }
        XCTAssertEqual(model.status["manual_route"], .null)
    }

    private static let helperScript = #"""
    #!/usr/bin/env python3
    import json, os, pathlib, socket, sys
    directory = pathlib.Path(sys.argv[sys.argv.index('--state-dir') + 1])
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(str(directory / 'control.sock'))
    server.listen()
    pending_trial = None
    while True:
        connection, _ = server.accept()
        with connection:
            stream = connection.makefile('rb')
            request = json.loads(stream.readline())
            operation = request['op']
            with (directory / 'requests').open('a') as record:
                record.write(operation + '\n')
            if operation == 'sync':
                (directory / 'last-sync.json').write_text(json.dumps(request['payload']))
            if operation == 'status':
                data = dict(process_id=os.getpid(), port=17863, sources=[], manual_route=pending_trial)
            elif operation == 'routing.try':
                pending_trial = dict(id='fixture-trial', source_id=request['payload']['source_id'])
                data = pending_trial
            elif operation == 'routing.cancel':
                data = dict(cancelled=pending_trial is not None and pending_trial['id'] == request['payload']['id'])
                if data['cancelled']:
                    pending_trial = None
            elif operation == 'routing.recover':
                data = dict(recovered=True)
            elif operation == 'credentials.storage.status':
                data = dict(mode='keychain')
            elif operation == 'credentials.read':
                data = dict(items=[dict(source_id=source, secret=dict(api_key='fixture-api-key')) for source in request['payload']['source_ids']])
            elif operation == 'credentials':
                data = dict(updated_secrets=[])
            elif operation == 'notifications.list':
                data = dict(events=[])
            elif operation in ('sync', 'history.configure'):
                data = {}
            else:
                connection.sendall((json.dumps(dict(id=request['id'], ok=False, error='Unexpected fixture operation')) + '\n').encode())
                continue
            connection.sendall((json.dumps(dict(id=request['id'], ok=True, data=data)) + '\n').encode())
    """#
}
