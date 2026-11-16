import XCTest
import Darwin
@testable import Codexpress

final class AppModelSyncTests: XCTestCase {
    @MainActor func testBackupRefreshKeepsVisibleDataAndRejectsPreviousDirectoryResults() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("b-\(UUID().uuidString.prefix(8))", isDirectory: true)
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
        let first = directory.appendingPathComponent("first")
        let second = directory.appendingPathComponent("second")
        for (home, id) in [(first, "snapshot-first"), (second, "snapshot-second")] {
            try FileManager.default.createDirectory(at: home, withIntermediateDirectories: true)
            try JSONEncoder().encode(JSONValue.object(["backups": .array([.object(["id": .string(id)])])])).write(to: home.appendingPathComponent("backups.json"))
        }
        var settings = AppSettings(); settings.codexHome = first.path
        try MetadataStore(directory: directory).save(settings)
        let model = AppModel(stateDirectory: directory)
        defer { model.stop() }
        await model.startHelper()
        XCTAssertTrue(model.running, model.error ?? "Fixture helper did not start")
        await model.refreshConfigurationBackups()
        XCTAssertEqual(model.configurationBackups.first?["id"].string, "snapshot-first")
        try Data().write(to: directory.appendingPathComponent("hold-backups"))
        let pending = Task { await model.refreshConfigurationBackups() }
        let deadline = Date().addingTimeInterval(3)
        while !FileManager.default.fileExists(atPath: directory.appendingPathComponent("backups-started").path), Date() < deadline {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertEqual(model.configurationBackups.first?["id"].string, "snapshot-first", "Refreshing must not hide existing backups")
        var changed = model.settings; changed.codexHome = second.path
        try model.persistSettings(changed)
        try FileManager.default.removeItem(at: directory.appendingPathComponent("hold-backups"))
        await pending.value
        XCTAssertTrue(model.configurationBackups.isEmpty, "Previous-directory backups must not appear under a new directory")
        await model.refreshConfigurationBackups()
        XCTAssertEqual(model.configurationBackups.first?["id"].string, "snapshot-second")
        try FileManager.default.removeItem(at: second.appendingPathComponent("backups.json"))
        await model.refreshConfigurationBackups()
        XCTAssertEqual(model.configurationBackups.first?["id"].string, "snapshot-second", "A failed refresh must keep the last successful list")
        XCTAssertNotNil(model.error)
    }

    @MainActor func testSettingChangePersistsWhenSyncFailsAndCanBeRetried() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("s-\(UUID().uuidString.prefix(8))", isDirectory: true)
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
        var settings = AppSettings(); settings.codexHome = directory.appendingPathComponent("codex").path
        let metadata = MetadataStore(directory: directory)
        try metadata.save(settings)
        try "official".write(to: directory.appendingPathComponent("connection-mode"), atomically: true, encoding: .utf8)
        let localFailure = directory.appendingPathComponent("fail-local-models")
        try Data().write(to: localFailure)
        let model = AppModel(stateDirectory: directory)
        defer { model.stop() }
        await model.startHelper()
        XCTAssertTrue(model.running, model.error ?? "Fixture helper did not start")
        XCTAssertNil(model.localModelSettingsSyncError)
        XCTAssertTrue(model.localModelSettingsNeedsSync)
        let applied = directory.appendingPathComponent("applied-settings.json")
        let initial = try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: applied))
        XCTAssertEqual(initial["auto_reset_weekly_quota"].bool, false)

        let failure = directory.appendingPathComponent("fail-sync")
        try Data().write(to: failure)
        model.changeAutoResetWeeklyQuota(true)
        let deadline = Date().addingTimeInterval(3)
        while model.busy, Date() < deadline { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertFalse(model.busy)
        XCTAssertNotNil(model.error, "An unsuccessful immediate sync must remain visible")
        XCTAssertTrue(model.pendingSync)
        XCTAssertTrue(try metadata.load().autoResetWeeklyQuotaEnabled, "Sync failure must not discard the persisted user choice")
        let unchanged = try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: applied))
        XCTAssertEqual(unchanged["auto_reset_weekly_quota"].bool, false)

        try FileManager.default.removeItem(at: failure)
        try await model.sync()
        XCTAssertFalse(model.pendingSync)
        let retried = try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: applied))
        XCTAssertEqual(retried["auto_reset_weekly_quota"].bool, true)

        try await model.updateStatus()
        let helperPID = model.status["process_id"]
        let policy = ModelPolicy(model: "custom-model", sourceIDs: [], clients: [.omp])
        let rule = try ModelCompactionRule.parse(model: "custom-model", threshold: "90000", window: "100000")
        try Data().write(to: failure)
        do {
            try await model.saveModelSettings(model: policy.model, policy: policy, compaction: rule)
            XCTFail("Failed sync must be reported to the model editor")
        } catch {
            XCTAssertTrue(error.localizedDescription.contains("已保存在本机"))
        }
        XCTAssertFalse(model.busy)
        XCTAssertTrue(model.pendingSync)
        let saved = try metadata.load()
        XCTAssertEqual(saved.modelPolicies, [policy])
        XCTAssertEqual(saved.modelCompaction, [rule], "Policy and compression must persist together even when sync fails")
        let beforeRetry = try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: applied))
        XCTAssertEqual(beforeRetry["model_policies"].array, [])
        try FileManager.default.removeItem(at: failure)
        try await model.saveModelSettings(model: policy.model, policy: policy, compaction: rule)
        XCTAssertFalse(model.pendingSync)
        let modelRetry = try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: applied))
        XCTAssertEqual(modelRetry["model_policies"], try JSONValue.from([policy]))
        XCTAssertEqual(modelRetry["model_compaction"], try JSONValue.from([rule]))
        XCTAssertEqual(model.configuredModelIDs, [policy.model])
        XCTAssertNil(try metadata.load().localCodexAppliedModelSettingsRevision)
        XCTAssertTrue(model.localModelSettingsNeedsSync)
        XCTAssertNil(model.localModelSettingsSyncError, "Saving gateway settings must not attempt a local Codex update")
        model.retryLocalModelSettingsSync()
        let failureDeadline = Date().addingTimeInterval(3)
        while model.busy, Date() < failureDeadline { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertNotNil(model.localModelSettingsSyncError)
        XCTAssertTrue(model.localModelSettingsNeedsSync)
        XCTAssertFalse(model.pendingSync)
        try FileManager.default.removeItem(at: localFailure)
        try Data().write(to: failure)
        model.retryLocalModelSettingsSync()
        let localDeadline = Date().addingTimeInterval(3)
        while model.busy, Date() < localDeadline { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertFalse(model.busy)
        XCTAssertNil(model.localModelSettingsSyncError)
        XCTAssertFalse(model.localModelSettingsNeedsSync)
        let manuallyApplied = try metadata.load().localCodexAppliedModelSettingsRevision
        XCTAssertEqual(manuallyApplied, model.modelSettingsRevision)
        try FileManager.default.removeItem(at: failure)
        try Data().write(to: localFailure)
        let revisedRule = try ModelCompactionRule.parse(model: policy.model, threshold: "80000", window: "100000")
        try await model.saveModelSettings(model: policy.model, policy: policy, compaction: revisedRule)
        XCTAssertFalse(model.pendingSync)
        XCTAssertTrue(model.localModelSettingsNeedsSync)
        XCTAssertNil(model.localModelSettingsSyncError)
        XCTAssertEqual(try metadata.load().localCodexAppliedModelSettingsRevision, manuallyApplied)
        XCTAssertNotEqual(manuallyApplied, model.modelSettingsRevision)
        try await model.updateStatus()
        XCTAssertEqual(model.status["process_id"], helperPID)
        XCTAssertEqual(model.connectionMode, "official")
        let invalid = ModelCompactionRule(model: policy.model, auto_compact_token_limit: 0, context_window: nil)
        do {
            try await model.saveModelSettings(model: policy.model, policy: ModelPolicy(model: policy.model), compaction: invalid)
            XCTFail("Invalid compression must reject the entire model edit")
        } catch {}
        let unchangedModel = try metadata.load()
        XCTAssertEqual(unchangedModel.modelPolicies, [policy])
        XCTAssertEqual(unchangedModel.modelCompaction, [revisedRule])
    }

    @MainActor func testUsageAnalyticsSharesRequestsAndKeepsSnapshotsAcrossPageCancellationAndRefreshFailure() async throws {
        try await withUsageAnalyticsModel { model, directory in
            let query = UsageQuery.last(days: 30)
            let hold = directory.appendingPathComponent("hold-analytics")
            defer { try? FileManager.default.removeItem(at: hold) }
            try Data().write(to: hold)
            let first = Task { try await model.usageAnalytics(query) }
            try await Self.waitForAnalyticsCalls(1, directory: directory)
            var secondStarted = false
            let second = Task {
                secondStarted = true
                return try await model.usageAnalytics(query)
            }
            while !secondStarted { await Task.yield() }
            first.cancel()
            try FileManager.default.removeItem(at: hold)
            let initial = try await second.value
            _ = try? await first.value
            XCTAssertEqual(initial["groups"].array.first?["requests"].number, 1)
            let reopened = try await model.usageAnalytics(query)
            XCTAssertEqual(reopened, initial)
            XCTAssertEqual(try Self.analyticsCalls(directory), 1, "Concurrent and reopened pages must reuse the same result")

            var closedQuery = query; closedQuery.sourceID = "closed-page"
            try Data().write(to: hold)
            let closedPage = Task { try await model.usageAnalytics(closedQuery) }
            try await Self.waitForAnalyticsCalls(2, directory: directory)
            closedPage.cancel()
            try FileManager.default.removeItem(at: hold)
            let deadline = Date().addingTimeInterval(3)
            while model.cachedUsageAnalytics(closedQuery) == nil, Date() < deadline {
                try await Task.sleep(for: .milliseconds(10))
            }
            let closedSnapshot = try XCTUnwrap(model.cachedUsageAnalytics(closedQuery), "A page with no remaining waiter must still populate the cache")
            _ = try? await closedPage.value
            let reopenedClosedPage = try await model.usageAnalytics(closedQuery)
            XCTAssertEqual(reopenedClosedPage, closedSnapshot.value)
            XCTAssertEqual(try Self.analyticsCalls(directory), 2)

            let refreshed = try await model.usageAnalytics(query, refresh: true)
            XCTAssertEqual(refreshed["groups"].array.first?["requests"].number, 3)
            let failure = directory.appendingPathComponent("fail-analytics")
            try Data().write(to: failure)
            do {
                _ = try await model.usageAnalytics(query, refresh: true)
                XCTFail("A failed refresh must report its error")
            } catch {}
            XCTAssertEqual(model.cachedUsageAnalytics(query)?.value, refreshed)
            let afterFailure = try await model.usageAnalytics(query)
            XCTAssertEqual(afterFailure, refreshed)
            XCTAssertEqual(try Self.analyticsCalls(directory), 4)
            try FileManager.default.removeItem(at: failure)

            try await Task.sleep(for: .milliseconds(5100))
            XCTAssertEqual(model.cachedUsageAnalytics(query)?.value, refreshed, "Expired snapshots remain available for the first frame")
            let expiredRefresh = try await model.usageAnalytics(query)
            XCTAssertEqual(expiredRefresh["groups"].array.first?["requests"].number, 5)
        }
    }

    @MainActor func testUsageAnalyticsSeparatesFiltersAndCalendarWindowsButReusesRollingWindows() async throws {
        try await withUsageAnalyticsModel { model, directory in
            var calendar = Calendar(identifier: .gregorian)
            calendar.timeZone = TimeZone(secondsFromGMT: 0)!
            let now = Date(timeIntervalSince1970: 1_800_000_123)
            let fixed = try UsageQuery(start: now, end: now, calendar: calendar)
            var source = fixed; source.sourceID = "source"
            var modelFilter = fixed; modelFilter.modelID = "model"
            var ingress = fixed; ingress.ingress = "omp"
            let tomorrow = calendar.date(byAdding: .day, value: 1, to: now)!
            let nextDate = try UsageQuery(start: tomorrow, end: tomorrow, calendar: calendar)
            var otherCalendar = calendar
            otherCalendar.timeZone = TimeZone(secondsFromGMT: 8 * 3600)!
            let otherDayBoundary = try UsageQuery(start: now, end: now, calendar: otherCalendar)
            let rolling = UsageQuery(lastHours: 1, now: now)
            let fiveHours = UsageQuery(lastHours: 5, now: now)
            for (index, query) in [fixed, source, modelFilter, ingress, nextDate, otherDayBoundary, rolling, fiveHours].enumerated() {
                let value = try await model.usageAnalytics(query)
                XCTAssertEqual(value["groups"].array.first?["requests"].number, Double(index + 1), "Distinct filters and time windows must not share analytics")
            }
            let laterRolling = UsageQuery(lastHours: 1, now: now.addingTimeInterval(60))
            let reused = try await model.usageAnalytics(laterRolling)
            XCTAssertEqual(reused["groups"].array.first?["requests"].number, 7)
            let snapshot = try XCTUnwrap(model.cachedUsageAnalytics(laterRolling))
            XCTAssertEqual(snapshot.query, rolling, "A rolling cache hit must expose its actual requested window")
            XCTAssertNotEqual(snapshot.query.from, laterRolling.from)
            XCTAssertEqual(snapshot.daily.first?.requests, 7)

            _ = try await model.usageAnalytics(fixed)
            var extraSource = fixed; extraSource.sourceID = "another-source"
            let additional = try await model.usageAnalytics(extraSource)
            XCTAssertEqual(additional["groups"].array.first?["requests"].number, 9)
            XCTAssertNotNil(model.cachedUsageAnalytics(fixed), "Recently reused snapshots must survive eviction")
            XCTAssertNil(model.cachedUsageAnalytics(source), "Only eight recently used snapshots are retained")
            XCTAssertEqual(try Self.analyticsCalls(directory), 9)
        }
    }

    @MainActor private func withUsageAnalyticsModel(_ body: @MainActor (AppModel, URL) async throws -> Void) async throws {
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let directory = root.appendingPathComponent(".runtime/u-\(UUID().uuidString.prefix(8))", isDirectory: true)
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
        var settings = AppSettings(); settings.codexHome = directory.appendingPathComponent("codex").path
        try MetadataStore(directory: directory).save(settings)
        let model = AppModel(stateDirectory: directory)
        defer { model.stop() }
        await model.startHelper()
        XCTAssertTrue(model.running, model.error ?? "Fixture helper did not start")
        try await body(model, directory)
    }

    private static func analyticsCalls(_ directory: URL) throws -> Int {
        Int(try String(contentsOf: directory.appendingPathComponent("analytics-count"))) ?? -1
    }

    private static func waitForAnalyticsCalls(_ count: Int, directory: URL) async throws {
        let deadline = Date().addingTimeInterval(3)
        while (try? analyticsCalls(directory)) != count, Date() < deadline {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertEqual(try analyticsCalls(directory), count)
    }

    private static let helperScript = #"""
    #!/usr/bin/env python3
    import json, os, pathlib, socket, sys, time
    directory = pathlib.Path(sys.argv[sys.argv.index('--state-dir') + 1])
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(str(directory / 'control.sock'))
    server.listen()
    while True:
        connection, _ = server.accept()
        with connection:
            request = json.loads(connection.makefile('rb').readline())
            operation = request['op']
            try:
                if operation == 'status':
                    mode = directory / 'connection-mode'
                    data = dict(process_id=os.getpid(), port=17863, sources=[], connection_mode=mode.read_text() if mode.exists() else 'unmanaged')
                elif operation == 'credentials.storage.status':
                    data = dict(mode='file')
                elif operation == 'usage.analytics':
                    counter = directory / 'analytics-count'
                    count = int(counter.read_text()) + 1 if counter.exists() else 1
                    counter.write_text(str(count))
                    while (directory / 'hold-analytics').exists(): time.sleep(0.01)
                    if (directory / 'fail-analytics').exists():
                        raise ValueError('Fixture analytics failed')
                    data = dict(groups=[dict(requests=count)], daily=[dict(start_at=request['payload']['from'], requests=count)])
                elif operation == 'config.backups':
                    if (directory / 'hold-backups').exists():
                        (directory / 'backups-started').touch()
                        while (directory / 'hold-backups').exists(): time.sleep(0.01)
                    data = json.loads((pathlib.Path(request['payload']['codex_home']) / 'backups.json').read_text())
                elif operation == 'credentials':
                    data = dict(updated_secrets=[])
                elif operation == 'sync':
                    if (directory / 'fail-sync').exists():
                        raise ValueError('Fixture sync failed')
                    (directory / 'applied-settings.json').write_text(json.dumps(request['payload']))
                    data = {}
                elif operation == 'config.models.apply':
                    if (directory / 'fail-local-models').exists():
                        raise ValueError('Fixture model apply failed')
                    data = dict(applied=(directory / 'connection-mode').exists(), reason='unmanaged')
                elif operation == 'history.configure':
                    data = {}
                else:
                    raise ValueError('Unexpected fixture operation')
                response = dict(id=request['id'], ok=True, data=data)
            except Exception:
                response = dict(id=request['id'], ok=False, error='Fixture read failed')
            connection.sendall((json.dumps(response) + '\n').encode())
    """#
}
