import XCTest
import Darwin
@testable import Codexpress

final class OfficialResetStateTests: XCTestCase {
    @MainActor func testRejectedInitialSyncKeepsOwnedHelperAvailableToRemoveDuplicateSources() async throws {
        let owner = Self.source("account-A")
        let duplicate = Self.source("account-A")
        let fixture = try OfficialResetFixture(sources: [owner, duplicate], rejectDuplicateSync: true)
        defer { fixture.stop() }
        try await fixture.start()
        XCTAssertTrue(fixture.model.running)
        XCTAssertTrue(fixture.model.pendingSync)
        XCTAssertTrue(fixture.model.error?.contains("重复官方账号") == true)
        let unrelated = AppModel(stateDirectory: fixture.directory)
        await unrelated.startHelper()
        XCTAssertFalse(unrelated.running)
        XCTAssertFalse(unrelated.canManageCredentials)
        XCTAssertTrue(unrelated.error?.contains("拒绝接管") == true)
        XCTAssertEqual(try fixture.operations().filter { $0 == "sync" }.count, 1)
        fixture.model.removeSource(duplicate.id)
        try await fixture.waitUntil { !fixture.model.busy }
        XCTAssertNil(fixture.model.error)
        XCTAssertTrue(fixture.model.running)
        XCTAssertFalse(fixture.model.pendingSync)
        XCTAssertEqual(fixture.model.settings.sources.map(\.id), [owner.id])
        XCTAssertEqual(try fixture.operations().filter { $0 == "sync" }.count, 2)
    }

    @MainActor func testSavingKnownDuplicateAccountRejectsDisabledOwnerBeforeSyncAndAllowsOtherAccounts() async throws {
        var owner = Self.source("account-A"); owner.name = "已连接的来源"; owner.enabled = false
        var duplicate = Self.source("account-A"); duplicate.name = "重复来源"
        var other = Self.source("account-B"); other.name = "不同账号"
        let fixture = try OfficialResetFixture(sources: [owner, duplicate, other])
        defer { fixture.stop() }
        try await fixture.start()
        let initialSyncCount = try fixture.operations().filter { $0 == "sync" }.count
        duplicate.name = "不应保存"
        do {
            try await fixture.model.saveSource(duplicate, apiKey: "")
            XCTFail("Duplicate official account was saved")
        } catch {
            XCTAssertTrue(error.localizedDescription.contains(owner.name))
        }
        XCTAssertEqual(try fixture.operations().filter { $0 == "sync" }.count, initialSyncCount)
        XCTAssertEqual(fixture.model.settings.sources.first { $0.id == duplicate.id }?.name, "重复来源")
        XCTAssertEqual(try MetadataStore(directory: fixture.directory).load().sources.first { $0.id == duplicate.id }?.name, "重复来源")
        other.name = "允许保存"
        try await fixture.model.saveSource(other, apiKey: "")
        XCTAssertEqual(try fixture.operations().filter { $0 == "sync" }.count, initialSyncCount + 1)
        XCTAssertEqual(fixture.model.settings.sources.first { $0.id == other.id }?.name, "允许保存")
        for _ in 0..<2 {
            var draft = Self.source("unused"); draft.account_id = nil
            try await fixture.model.saveSource(draft, apiKey: "")
        }
        XCTAssertEqual(fixture.model.settings.sources.filter { $0.account_id == nil }.count, 2)
    }

    @MainActor func testCreditRefreshWaitsForQuotaOnlyRefreshAndCoalescesOtherCreditWaiters() async throws {
        let source = Self.source("account-A")
        let fixture = try OfficialResetFixture(sources: [source])
        defer { fixture.stop() }
        try await fixture.start()
        try Data().write(to: fixture.directory.appendingPathComponent("hold-refresh"))
        let quotaOnly = Task { try await fixture.model.queryOfficial(source.id, includeCredits: false) }
        try await fixture.waitUntil { try fixture.refreshRequests().count == 1 }
        let credits = Task { try await fixture.model.queryOfficial(source.id, includeCredits: true) }
        let otherCredits = Task { try await fixture.model.queryOfficial(source.id, includeCredits: true) }
        await Task.yield()
        try Data().write(to: fixture.directory.appendingPathComponent("release-refresh-false"))
        try await fixture.waitUntil { try fixture.refreshRequests().count == 2 }
        XCTAssertTrue(fixture.model.refreshingQuotas.contains(source.id))
        try Data().write(to: fixture.directory.appendingPathComponent("release-refresh-true"))
        try await quotaOnly.value
        try await credits.value
        try await otherCredits.value
        XCTAssertEqual(try fixture.refreshRequests().compactMap { $0["include_credits"].bool }, [false, true])
        XCTAssertFalse(fixture.model.refreshingQuotas.contains(source.id))
    }

    @MainActor func testStoredResultsFollowAccountsAndLegacyRecordsProtectTheirOriginalSource() throws {
        let directory = URL(fileURLWithPath: "/tmp/cx-reset-history-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        var first = Self.source("account-A")
        let alias = Self.source("account-A")
        let other = Self.source("account-B")
        var settings = AppSettings(); settings.sources = [first, alias, other]
        settings.codexHome = directory.appendingPathComponent("codex").path
        try MetadataStore(directory: directory).save(settings)
        try ResetOperationStore(directory: directory).save([
            // An account-bound record may still have an old source-based key.
            first.id + ":card": ResetOperation(sourceID: first.id, creditID: "card", operationID: "bound", createdAt: 2, result: "account-A result", accountID: "account-A"),
            first.id + ":legacy": ResetOperation(sourceID: first.id, creditID: "legacy", operationID: "legacy", createdAt: 1, result: "legacy result")
        ])
        let model = AppModel(stateDirectory: directory)
        XCTAssertEqual(model.resetResult(for: alias, creditID: "card"), "account-A result")
        XCTAssertFalse(model.canSubmitCredit(for: alias, creditID: "card"))
        XCTAssertNil(model.resetResult(for: other, creditID: "card"))
        XCTAssertTrue(model.canSubmitCredit(for: other, creditID: "card"))
        XCTAssertNil(model.resetResult(for: alias, creditID: "legacy"))
        first.account_id = "account-B"
        XCTAssertNil(model.resetResult(for: first, creditID: "card"))
        XCTAssertTrue(model.canSubmitCredit(for: first, creditID: "card"))
        XCTAssertEqual(model.resetResult(for: first, creditID: "legacy"), "legacy result")
        XCTAssertFalse(model.canSubmitCredit(for: first, creditID: "legacy"))
    }

    @MainActor func testInFlightCardsSerializePerAccountAcrossSourcesAndSurviveAnAccountSwitch() async throws {
        var first = Self.source("account-A")
        let alias = Self.source("account-A")
        let other = Self.source("account-B")
        let fixture = try OfficialResetFixture(sources: [first, alias, other])
        defer { fixture.stop() }
        try await fixture.start()
        let model = fixture.model
        let firstConfirmation = try fixture.confirmation(first, card: "card")
        let duplicate = try fixture.confirmation(alias, card: "card")
        let otherCard = try fixture.confirmation(alias, card: "other-card")
        model.consume(firstConfirmation)
        XCTAssertTrue(model.isConsumingCredit(for: first, creditID: "card"))
        XCTAssertTrue(model.isConsumingCredit(for: alias, creditID: "card"))
        XCTAssertFalse(model.canSubmitCredit(for: alias, creditID: "other-card"))
        model.consume(duplicate)
        model.consume(otherCard)
        XCTAssertTrue(model.canSubmitCredit(for: other, creditID: "card"))
        model.consume(try fixture.confirmation(other, card: "card"))
        XCTAssertTrue(model.isConsumingCredit(for: other, creditID: "card"))
        XCTAssertFalse(model.busy)

        // A login changes the account behind an existing source while its old
        // account's request is still pending. Its alias must stay protected.
        first.account_id = "account-C"
        var changed = model.settings; changed.sources[0] = first
        try fixture.setSources(changed.sources)
        try model.persistSettings(changed)
        try await model.queryOfficial(first.id, includeCredits: true)
        XCTAssertNil(model.resetResult(for: first, creditID: "card"))
        XCTAssertFalse(model.isConsumingCredit(for: first, creditID: "card"))
        XCTAssertTrue(model.canSubmitCredit(for: first, creditID: "card"))
        XCTAssertTrue(model.isConsumingCredit(for: alias, creditID: "card"))
        model.consume(try fixture.confirmation(first, card: "card"))
        try await fixture.waitUntil { try fixture.requests().count == 3 }
        let requests = try fixture.requests()
        XCTAssertEqual(Set(requests.compactMap { $0["expected_account_id"].string }), ["account-A", "account-B", "account-C"])
        XCTAssertEqual(requests.compactMap { $0["credit_id"].string }, ["card", "card", "card"])

        try fixture.release()
        try await fixture.waitUntil { model.consuming.isEmpty }
        XCTAssertNotNil(model.resetResult(for: alias, creditID: "card"))
        XCTAssertNotNil(model.resetResult(for: first, creditID: "card"))
        XCTAssertFalse(model.canSubmitCredit(for: alias, creditID: "card"))
        XCTAssertFalse(model.canSubmitCredit(for: first, creditID: "card"))
        first.account_id = "account-D"
        XCTAssertNil(model.resetResult(for: first, creditID: "card"))
        XCTAssertTrue(model.canSubmitCredit(for: first, creditID: "card"))
        let stored = try ResetOperationStore(directory: fixture.directory).load()
        XCTAssertEqual(stored.count, 3)
        XCTAssertEqual(Set(stored.values.compactMap(\.accountID)), ["account-A", "account-B", "account-C"])
        let reloaded = AppModel(stateDirectory: fixture.directory)
        XCTAssertNotNil(reloaded.resetResult(for: alias, creditID: "card"))
        XCTAssertFalse(reloaded.canSubmitCredit(for: alias, creditID: "card"))
    }

    @MainActor func testFailedIntentWriteLeavesNoStateAndAnUncertainSentRequestCannotBeRepeated() async throws {
        let source = Self.source("account-A")
        let fixture = try OfficialResetFixture(sources: [source], disconnectConsume: true)
        defer { fixture.stop() }
        try await fixture.start()
        let model = fixture.model
        let confirmation = try fixture.confirmation(source, card: "card")
        let store = ResetOperationStore(directory: fixture.directory)
        // Atomic rename onto a directory fails reliably, including under admin.
        try FileManager.default.createDirectory(at: store.file, withIntermediateDirectories: false)
        model.consume(confirmation)
        XCTAssertNotNil(model.error)
        XCTAssertNil(model.resetResult(for: source, creditID: "card"))
        XCTAssertFalse(model.isConsumingCredit(for: source, creditID: "card"))
        XCTAssertTrue(model.canSubmitCredit(for: source, creditID: "card"))
        XCTAssertTrue(try fixture.requests().isEmpty)
        try FileManager.default.removeItem(at: store.file)
        model.error = nil
        model.consume(confirmation)
        try await fixture.waitUntil { try fixture.requests().count == 1 }
        XCTAssertFalse(model.canSubmitCredit(for: source, creditID: "card"))
        XCTAssertEqual(try store.load().count, 1, "The intent must exist before the fixture receives the request")
        try fixture.release()
        try await fixture.waitUntil { model.consuming.isEmpty }
        XCTAssertNotNil(model.error)
        XCTAssertTrue(model.resetResult(for: source, creditID: "card")?.contains("待核实") == true)
        model.consume(confirmation)
        XCTAssertEqual(try fixture.requests().count, 1)
        let reloaded = AppModel(stateDirectory: fixture.directory)
        XCTAssertFalse(reloaded.canSubmitCredit(for: source, creditID: "card"))
        XCTAssertTrue(reloaded.resetResult(for: source, creditID: "card")?.contains("待核实") == true)
    }

    @MainActor func testUnreadableOperationHistoryBlocksEveryAccount() throws {
        let directory = URL(fileURLWithPath: "/tmp/cx-reset-corrupt-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        try Data("{invalid".utf8).write(to: ResetOperationStore(directory: directory).file)
        let model = AppModel(stateDirectory: directory)
        XCTAssertFalse(model.canSubmitCredit(for: Self.source("account-A"), creditID: "card"))
        XCTAssertFalse(model.canSubmitCredit(for: Self.source("account-B"), creditID: "card"))
    }

    private static func source(_ account: String) -> Source {
        var source = Source.newOfficial(); source.account_id = account
        source.proxy.url = "http://127.0.0.1:9999"
        source.automaticModelDiscovery = false
        return source
    }
}

@MainActor private final class OfficialResetFixture {
    let directory: URL
    let model: AppModel
    private let disconnectConsume: Bool
    private let rejectDuplicateSync: Bool

    init(sources: [Source], disconnectConsume: Bool = false, rejectDuplicateSync: Bool = false) throws {
        directory = URL(fileURLWithPath: "/tmp/cx-reset-\(UUID().uuidString)", isDirectory: true)
        self.disconnectConsume = disconnectConsume
        self.rejectDuplicateSync = rejectDuplicateSync
        var settings = AppSettings(); settings.sources = sources
        settings.codexHome = directory.appendingPathComponent("codex").path
        try MetadataStore(directory: directory).save(settings)
        model = AppModel(stateDirectory: directory)
        try setSources(sources)
        try Self.script.write(to: directory.appendingPathComponent("gateway-fixture"), atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: directory.appendingPathComponent("gateway-fixture").path)
    }

    func start() async throws {
        let previous = ProcessInfo.processInfo.environment["CODEXPRESS_GATEWAY_PATH"]
        setenv("CODEXPRESS_GATEWAY_PATH", directory.appendingPathComponent("gateway-fixture").path, 1)
        defer {
            if let previous { setenv("CODEXPRESS_GATEWAY_PATH", previous, 1) }
            else { unsetenv("CODEXPRESS_GATEWAY_PATH") }
        }
        await model.startHelper()
        XCTAssertTrue(model.running, model.error ?? "Fixture did not start")
        guard model.running else { throw AppError.message("Fixture did not start") }
    }

    func stop() {
        model.stop()
        try? FileManager.default.removeItem(at: directory)
    }

    func setSources(_ sources: [Source]) throws {
        let cards: [JSONValue] = ["card", "other-card"].map { id in
            .object(["id": .string(id), "status": .string("available"), "expiry_known": .bool(true), "expires_at": .number(4_000_000_000)])
        }
        let live: [JSONValue] = try sources.map { source in
            guard case .object(var value) = try JSONValue.from(source) else { throw AppError.message("Invalid fixture source") }
            value["credits"] = .object(["stale": .bool(false), "error": .null, "credits": .array(cards)])
            return .object(value)
        }
        try JSONEncoder().encode(JSONValue.object(["sources": .array(live), "disconnect_consume": .bool(disconnectConsume), "reject_duplicate_sync": .bool(rejectDuplicateSync)])).write(to: directory.appendingPathComponent("fixture-state.json"), options: .atomic)
    }

    func confirmation(_ source: Source, card: String) throws -> CreditConfirmation {
        try CreditConfirmation.make(source: source, live: model.state(source.id), creditID: card)
    }

    func requests() throws -> [JSONValue] {
        try recordedRequests("consume-requests")
    }

    func refreshRequests() throws -> [JSONValue] {
        try recordedRequests("refresh-requests")
    }

    func operations() throws -> [String] {
        try String(contentsOf: directory.appendingPathComponent("operations")).split(separator: "\n").map(String.init)
    }

    private func recordedRequests(_ name: String) throws -> [JSONValue] {
        let file = directory.appendingPathComponent(name)
        guard FileManager.default.fileExists(atPath: file.path) else { return [] }
        return try String(contentsOf: file).split(separator: "\n").map { try JSONDecoder().decode(JSONValue.self, from: Data($0.utf8)) }
    }

    func release() throws { try Data().write(to: directory.appendingPathComponent("release-consume")) }

    func waitUntil(_ condition: () throws -> Bool) async throws {
        let deadline = Date().addingTimeInterval(5)
        while try !condition(), Date() < deadline { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertTrue(try condition(), "Fixture condition timed out")
    }

    private static let script = #"""
    #!/usr/bin/env python3
    import json, os, pathlib, socket, sys, threading, time
    directory = pathlib.Path(sys.argv[sys.argv.index('--state-dir') + 1])
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(str(directory / 'control.sock'))
    server.listen()
    record_lock = threading.Lock()
    def handle(connection):
        with connection:
            request = json.loads(connection.makefile('rb').readline())
            operation = request['op']
            with record_lock:
                with (directory / 'operations').open('a') as record:
                    record.write(operation + '\n')
            state = json.loads((directory / 'fixture-state.json').read_text())
            sources = state['sources']
            def secret(source):
                return dict(account_id=source['account_id'], access_token='fixture-token')
            if operation == 'status':
                data = dict(process_id=os.getpid(), port=17863, sources=sources)
            elif operation == 'credentials.storage.status':
                data = dict(mode='keychain')
            elif operation == 'credentials.read':
                by_id = {source['id']: source for source in sources}
                data = dict(items=[dict(source_id=source_id, secret=secret(by_id[source_id]) if source_id in by_id else None) for source_id in request['payload']['source_ids']])
            elif operation == 'credentials.delete':
                state['sources'] = [source for source in sources if source['id'] != request['payload']['source_id']]
                temporary = directory / 'fixture-state-next.json'
                temporary.write_text(json.dumps(state))
                temporary.replace(directory / 'fixture-state.json')
                data = {}
            elif operation == 'credentials':
                data = dict(updated_secrets=[dict(source_id=source['id'], secret=secret(source)) for source in sources])
            elif operation == 'notifications.list':
                data = dict(events=[])
            elif operation == 'official.refresh':
                with record_lock:
                    with (directory / 'refresh-requests').open('a') as record:
                        record.write(json.dumps(request['payload']) + '\n')
                if (directory / 'hold-refresh').exists():
                    release = 'release-refresh-' + str(request['payload']['include_credits']).lower()
                    deadline = time.monotonic() + 8
                    while not (directory / release).exists() and time.monotonic() < deadline:
                        time.sleep(0.01)
                data = {}
            elif operation == 'official.credits.consume':
                with record_lock:
                    with (directory / 'consume-requests').open('a') as record:
                        record.write(json.dumps(request['payload']) + '\n')
                deadline = time.monotonic() + 8
                while not (directory / 'release-consume').exists() and time.monotonic() < deadline:
                    time.sleep(0.01)
                if state['disconnect_consume']:
                    return
                data = dict(code='reset')
            elif operation == 'sync':
                accounts = [source['account_id'] for source in request['payload']['sources'] if source.get('kind') == 'official' and source.get('account_id')]
                if state['reject_duplicate_sync'] and len(accounts) != len(set(accounts)):
                    connection.sendall((json.dumps(dict(id=request['id'], ok=False, error='重复官方账号，请删除重复来源')) + '\n').encode())
                    return
                data = {}
            elif operation == 'history.configure':
                data = {}
            else:
                connection.sendall((json.dumps(dict(id=request['id'], ok=False, error='Unexpected fixture operation: ' + operation)) + '\n').encode())
                return
            connection.sendall((json.dumps(dict(id=request['id'], ok=True, data=data)) + '\n').encode())
    while True:
        connection, _ = server.accept()
        threading.Thread(target=handle, args=(connection,), daemon=True).start()
    """#
}
