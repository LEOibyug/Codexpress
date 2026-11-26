import XCTest
import Darwin
@testable import Codexpress

final class AppModelSyncTests: XCTestCase {
    @MainActor func testUnverifiedCredentialStoreCannotReceiveCredentialsDuringStartupRepairOrShutdown() async throws {
        let directory = try gatewayFixtureDirectory()
        var source = Source(); source.name = "Existing API"
        var settings = AppSettings(); settings.sources = [source]
        let metadata = MetadataStore(directory: directory)
        try metadata.save(settings)
        let original = try Data(contentsOf: metadata.file)
        let log = directory.appendingPathComponent("operations")
        try Data().write(to: log)
        let model = AppModel(stateDirectory: directory)
        defer { model.stop() }
        XCTAssertEqual(model.localCredentialFile.path, directory.appendingPathComponent("credentials.json").path)

        for invalid in [#"{"mode":"file","file_path":"relative.json"}"#, #"{"mode":"keychain"}"#] {
            try Data(invalid.utf8).write(to: directory.appendingPathComponent("storage-status.json"))
            await model.startHelper()
            XCTAssertFalse(model.running)
            XCTAssertTrue(model.error?.contains("未读取任何凭据") == true)
            do { try await model.sync(); XCTFail("Unverified store accepted a sync") } catch {}
            do { try await model.saveSource(source, apiKey: "fixture-key"); XCTFail("Unverified store accepted a key") } catch {}
            await model.poll()
        }
        let active = try await model.activeRequestsBeforeTermination()
        XCTAssertEqual(active, 0)
        await model.shutdown()
        let operations = try String(contentsOf: log).split(separator: "\n")
        XCTAssertEqual(operations.filter { $0 == "credentials.storage.status" }.count, 2)
        XCTAssertTrue(operations.allSatisfy { ["status", "gateway.update.info", "credentials.storage.status"].contains($0) }, "Unexpected operations: \(operations)")
        XCTAssertEqual(try Data(contentsOf: metadata.file), original)
    }

    @MainActor func testRemoteStandbyPreservesBusyHelperAndRestartsOnlyOnReturn() async throws {
        let dir = try gatewayFixtureDirectory()
        var settings = AppSettings(); settings.codexHome = dir.appendingPathComponent("codex").path
        try MetadataStore(directory: dir).save(settings)
        let model = AppModel(stateDirectory: dir)
        defer { model.stop() }
        await model.start()
        let ipc = GatewayClient(socketPath: dir.appendingPathComponent("control.sock").path)
        let original = try await ipc.call("status")["process_id"]
        let later = ContinuousClock.now.advanced(by: .seconds(301))
        model.localGatewayTargetChanged(remote: true)
        await model.suspendLocalGatewayIfDue(at: .now.advanced(by: .seconds(299)))
        model.localGatewayTargetChanged(remote: false)
        await model.suspendLocalGatewayIfDue(at: later)
        let same = try await ipc.call("status")["process_id"]
        XCTAssertEqual(same, original, "Returning within the grace period must keep the same helper")

        let busy = dir.appendingPathComponent("active-stream")
        try Data().write(to: busy)
        model.localGatewayTargetChanged(remote: true)
        await model.suspendLocalGatewayIfDue(at: later)
        let stillServing = try await ipc.call("status")["process_id"]
        XCTAssertEqual(stillServing, original, "Only the gateway, not the last cached status, can admit shutdown")
        try FileManager.default.removeItem(at: busy)
        await model.suspendLocalGatewayIfDue(at: later)
        let deadline = ContinuousClock.now.advanced(by: .seconds(3))
        while model.running, .now < deadline { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertFalse(model.running)
        do { _ = try await ipc.call("status"); XCTFail("Local endpoint still accepts connections") } catch {}
        model.localGatewayTargetChanged(remote: false)
        let resumed = ContinuousClock.now.advanced(by: .seconds(4))
        while model.gatewayVersion == nil, .now < resumed { try await Task.sleep(for: .milliseconds(10)) }
        let next = try await ipc.call("status")["process_id"]
        XCTAssertNotEqual(next, original)
        XCTAssertTrue(model.running)

        // A return after admission but before exit must also bring back exactly one serving process.
        let hold = dir.appendingPathComponent("hold-exit")
        try Data().write(to: hold)
        model.localGatewayTargetChanged(remote: true)
        await model.suspendLocalGatewayIfDue(at: .now.advanced(by: .seconds(301)))
        model.localGatewayTargetChanged(remote: false)
        try FileManager.default.removeItem(at: hold)
        let handoff = ContinuousClock.now.advanced(by: .seconds(4))
        while (model.status["process_id"] == next || !model.running), .now < handoff { try await Task.sleep(for: .milliseconds(10)) }
        let final = try await ipc.call("status")["process_id"]
        XCTAssertNotEqual(final, next)
        XCTAssertTrue(model.running)
    }

    @MainActor func testAPIKeysCanBeRestoredIndividuallyWithoutChangingTheFixedRoute() async throws {
        let directory = try gatewayFixtureDirectory()
        var first = Source(); first.name = "First"; first.automaticModelDiscovery = false
        var fixed = Source(); fixed.name = "Fixed"; fixed.automaticModelDiscovery = false
        var settings = AppSettings(); settings.sources = [first, fixed]; settings.mode = "fixed"; settings.fixed_source_id = fixed.id
        let metadata = MetadataStore(directory: directory)
        try metadata.save(settings)
        let model = await startGateway(in: directory)
        defer { model.stop() }

        try await model.saveSource(first, apiKey: "fixture-first")
        XCTAssertTrue(model.isMissingCredential(fixed))
        let applied = try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: directory.appendingPathComponent("applied-settings.json")))
        XCTAssertEqual(applied["mode"].string, "fixed")
        XCTAssertEqual(applied["fixed_source_id"].string, fixed.id)
        XCTAssertEqual(applied["sources"].array.map { $0["id"].string }, [first.id, fixed.id])
        XCTAssertEqual(applied["sources"].array.first?["secret"]["api_key"].string, "fixture-first")
        try await model.saveSource(fixed, apiKey: "fixture-fixed")
        XCTAssertFalse(model.isMissingCredential(fixed))
        XCTAssertEqual(try metadata.load().fixed_source_id, fixed.id)
    }

    @MainActor func testBackupRefreshKeepsVisibleDataAndRejectsPreviousDirectoryResults() async throws {
        let directory = try gatewayFixtureDirectory()
        let first = directory.appendingPathComponent("first")
        let second = directory.appendingPathComponent("second")
        for (home, id) in [(first, "snapshot-first"), (second, "snapshot-second")] {
            try FileManager.default.createDirectory(at: home, withIntermediateDirectories: true)
            try JSONEncoder().encode(JSONValue.object(["backups": .array([.object(["id": .string(id)])])])).write(to: home.appendingPathComponent("backups.json"))
        }
        var settings = AppSettings(); settings.codexHome = first.path
        try MetadataStore(directory: directory).save(settings)
        let model = await startGateway(in: directory)
        defer { model.stop() }
        await model.refreshConfigurationBackups()
        XCTAssertEqual(model.configurationBackups.first?["id"].string, "snapshot-first")
        try Data().write(to: directory.appendingPathComponent("hold-backups"))
        let pending = Task { await model.refreshConfigurationBackups() }
        let deadline = Date().addingTimeInterval(3)
        while !FileManager.default.fileExists(atPath: directory.appendingPathComponent("backups-started").path), Date() < deadline {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertTrue(FileManager.default.fileExists(atPath: directory.appendingPathComponent("backups-started").path))
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
        let directory = try gatewayFixtureDirectory()
        var settings = AppSettings(); settings.codexHome = directory.appendingPathComponent("codex").path
        var first = Source.newOfficial(); first.id = "source-1"; first.account_id = "account-z"
        first.proxy.url = "http://127.0.0.1:7890"; first.automaticModelDiscovery = false
        var second = first; second.id = "source-2"; second.account_id = "account-a"
        settings.sources = [first, second]
        let metadata = MetadataStore(directory: directory)
        try metadata.save(settings)
        var legacy = try JSONDecoder().decode([String: JSONValue].self, from: Data(contentsOf: metadata.file))
        legacy["autoResetWeeklyQuota"] = .bool(true)
        try JSONEncoder().encode(legacy).write(to: metadata.file)
        try "official".write(to: directory.appendingPathComponent("connection-mode"), atomically: true, encoding: .utf8)
        let localFailure = directory.appendingPathComponent("fail-local-models")
        try Data().write(to: localFailure)
        let model = await startGateway(in: directory)
        defer { model.stop() }
        XCTAssertNil(model.localModelSettingsSyncError)
        XCTAssertTrue(model.localModelSettingsNeedsSync)
        XCTAssertFalse(model.showsLocalModelSettingsWarning)
        let applied = directory.appendingPathComponent("applied-settings.json")
        let initial = try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: applied))
        XCTAssertEqual(initial["auto_reset_weekly_accounts"], .array([]), "The old global opt-in must not enroll either account")
        XCTAssertEqual(initial["auto_reset_weekly_quota"], .null)

        let failure = directory.appendingPathComponent("fail-sync")
        try Data().write(to: failure)
        model.changeAutoResetWeeklyQuota(true, for: first)
        XCTAssertFalse(model.busy)
        let deadline = Date().addingTimeInterval(3)
        while (model.busy || model.hasPendingBackgroundSync), Date() < deadline { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertFalse(model.busy)
        XCTAssertNotNil(model.error, "An unsuccessful immediate sync must remain visible")
        XCTAssertTrue(model.pendingSync)
        XCTAssertTrue(model.showsPendingSyncWarning)
        let persisted = try metadata.load()
        XCTAssertTrue(persisted.weeklyResetEnabled(for: first), "Sync failure must not discard the persisted user choice")
        XCTAssertFalse(persisted.weeklyResetEnabled(for: second))
        let stored = try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: metadata.file))
        XCTAssertEqual(stored["autoResetWeeklyAccounts"]["account-z"]["proxy"]["url"].string, first.proxy.url)
        XCTAssertEqual(stored["autoResetWeeklyQuota"], .null)
        let unchanged = try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: applied))
        XCTAssertEqual(unchanged["auto_reset_weekly_accounts"], .array([]))

        try FileManager.default.removeItem(at: failure)
        let notification = directory.appendingPathComponent("updated-secrets.json")
        try Data(#"[{"source_id":"source-1","secret":{"account_id":"stale"}}]"#.utf8).write(to: notification)
        try await model.sync()
        XCTAssertFalse(model.pendingSync)
        let retried = try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: applied))
        XCTAssertEqual(retried["auto_reset_weekly_accounts"], .array([.string("account-z")]))
        XCTAssertEqual(retried["sources"].array.first?["secret"], .object([:]), "Never send tokens back for a helper-owned official account")
        try FileManager.default.removeItem(at: notification)

        model.changeAutoResetWeeklyQuota(true, for: second)
        XCTAssertFalse(model.busy)
        let secondDeadline = Date().addingTimeInterval(3)
        while (model.busy || model.hasPendingBackgroundSync), Date() < secondDeadline { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertFalse(model.busy)
        XCTAssertNil(model.error)
        let both = try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: applied))
        XCTAssertEqual(both["auto_reset_weekly_accounts"], .array([.string("account-a"), .string("account-z")]))
        XCTAssertEqual(both["sources"].array.first?["secret"]["access_token"].string, "fixture-token", "A background sync must read the current ownership notification")

        try await model.finishLogin(first.id)
        XCTAssertTrue(try metadata.load().weeklyResetEnabled(for: first), "Same-account relogin retains consent")
        var moved = first; moved.proxy.url = "http://127.0.0.1:7891"
        try await model.saveSource(moved, apiKey: "")
        try await model.saveSource(first, apiKey: "")
        let movedSync = try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: applied))
        XCTAssertEqual(movedSync["auto_reset_weekly_accounts"], .array([.string("account-a")]), "Changing the endpoint revokes consent even after switching back")

        let accountOverride = directory.appendingPathComponent("account-\(second.id)")
        try "account-b".write(to: accountOverride, atomically: true, encoding: .utf8)
        try await model.finishLogin(second.id)
        XCTAssertEqual(model.settings.sources.first(where: { $0.id == second.id })?.account_id, "account-b")
        let switched = try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: applied))
        XCTAssertEqual(switched["auto_reset_weekly_accounts"], .array([]), "A different account must not receive consent")

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
        XCTAssertFalse(model.showsLocalModelSettingsWarning)
        try "gateway".write(to: directory.appendingPathComponent("connection-mode"), atomically: true, encoding: .utf8)
        try await model.updateStatus()
        XCTAssertTrue(model.showsLocalModelSettingsWarning)
        let invalidRules = [
            ModelCompactionRule(model: policy.model, auto_compact_token_limit: 0, context_window: nil),
            ModelCompactionRule(model: policy.model, auto_compact_token_limit: nil, context_window: 200001)
        ]
        for invalid in invalidRules {
            do {
                try await model.saveModelSettings(model: policy.model, policy: ModelPolicy(model: policy.model), compaction: invalid)
                XCTFail("Invalid compression must reject the entire model edit")
            } catch {}
        }
        let unchangedModel = try metadata.load()
        XCTAssertEqual(unchangedModel.modelPolicies, [policy])
        XCTAssertEqual(unchangedModel.modelCompaction, [revisedRule])
        let extended = ModelCompactionRule(model: policy.model, auto_compact_token_limit: nil, context_window: 150000)
        try await model.saveModelSettings(model: policy.model, policy: policy, compaction: extended)
        XCTAssertEqual(try metadata.load().modelCompaction, [extended], "Maximum input may exceed the standard window, but not the official maximum")
    }

    @MainActor func testUsageAnalyticsSharesRequestsAndKeepsSnapshotsAcrossPageCancellationAndRefreshFailure() async throws {
        try await withGatewayModel { model, directory in
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
        try await withGatewayModel { model, directory in
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

    private func gatewayFixtureDirectory() throws -> URL {
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let temp = ProcessInfo.processInfo.environment["CODEXPRESS_TEST_TMPDIR"].map(URL.init(fileURLWithPath:)) ?? root.appendingPathComponent(".runtime")
        let directory = temp.appendingPathComponent("s-\(UUID().uuidString.prefix(8))", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        addTeardownBlock { try FileManager.default.removeItem(at: directory) }
        let helper = directory.appendingPathComponent("gateway-fixture")
        try Self.helperScript.write(to: helper, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: helper.path)
        let previousHelper = ProcessInfo.processInfo.environment["CODEXPRESS_GATEWAY_PATH"]
        setenv("CODEXPRESS_GATEWAY_PATH", helper.path, 1)
        addTeardownBlock {
            if let previousHelper { setenv("CODEXPRESS_GATEWAY_PATH", previousHelper, 1) }
            else { unsetenv("CODEXPRESS_GATEWAY_PATH") }
        }
        return directory
    }

    @MainActor private func startGateway(in directory: URL) async -> AppModel {
        let model = AppModel(stateDirectory: directory)
        await model.startHelper()
        XCTAssertTrue(model.running, model.error ?? "Fixture helper did not start")
        XCTAssertEqual(model.localCredentialFile.path, directory.appendingPathComponent("legacy-file.json").path)
        return model
    }

    @MainActor private func withGatewayModel(_ body: @MainActor (AppModel, URL) async throws -> Void) async throws {
        let directory = try gatewayFixtureDirectory()
        var settings = AppSettings(); settings.codexHome = directory.appendingPathComponent("codex").path
        try MetadataStore(directory: directory).save(settings)
        let model = await startGateway(in: directory)
        defer { model.stop() }
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

    @MainActor func testModelDiscoveryDiscardsResultAfterCredentialChange() async throws {
        try await withGatewayModel { model, directory in
            var source = Source(); source.name = "API"; source.models = ["original"]; source.automaticModelDiscovery = true
            var settings = model.settings; settings.sources = [source]
            try model.persistSettings(settings)
            try model.credentialSnapshot.apply(.object(["items": .array([.object(["source_id": .string(source.id), "secret": .object(["api_key": .string("old-key")])])])]), requested: [source.id])
            let hold = directory.appendingPathComponent("hold-models")
            try Data().write(to: hold)
            defer { try? FileManager.default.removeItem(at: hold) }
            let pending = Task { await model.refreshModels(source.id) }
            let started = directory.appendingPathComponent("models-started")
            let deadline = Date().addingTimeInterval(3)
            while !FileManager.default.fileExists(atPath: started.path), Date() < deadline {
                try await Task.sleep(for: .milliseconds(10))
            }
            XCTAssertTrue(FileManager.default.fileExists(atPath: started.path))
            try model.credentialSnapshot.apply(.object(["items": .array([.object(["source_id": .string(source.id), "secret": .object(["api_key": .string("new-key")])])])]), requested: [source.id])
            try FileManager.default.removeItem(at: hold)
            await pending.value
            XCTAssertEqual(model.settings.sources.first?.models, ["original"])
        }
    }

    @MainActor func testRapidSettingsEditsApplyImmediatelyAndCoalesceGatewaySync() async throws {
        try await withGatewayModel { model, directory in
            var first = Source()
            first.id = "api-1"; first.name = "API 1"; first.priority = 0; first.models = ["gpt-5.4", "gemini-2.5-pro"]
            var second = Source()
            second.id = "api-2"; second.name = "API 2"; second.priority = 1; second.models = ["gpt-5.4"]
            try model.credentialSnapshot.apply(.object(["items": .array([
                .object(["source_id": .string("api-1"), "secret": .object(["api_key": .string("k1")])]),
                .object(["source_id": .string("api-2"), "secret": .object(["api_key": .string("k2")])]),
            ])]), requested: ["api-1", "api-2"])
            try await model.updateStatus()
            try model.persistSettings({
                var s = model.settings
                s.sources = [first, second]
                return s
            }())
            try await model.sync()

            let syncCountFile = directory.appendingPathComponent("sync-count")
            let countBefore = Int(try String(contentsOf: syncCountFile)) ?? 0

            model.changeLoadBalance(true)
            model.changeLowQuotaThreshold(25)
            model.changeUpstreamRetry(attempts: 6, delaySeconds: 9)
            model.toggle(second)
            try model.setModelsEnabled(["gemini-2.5-pro"], enabled: false)

            // Every setting applies locally with zero UI lockout (`busy` remains false)
            // and without flashing the error/warning banner while coalescing.
            XCTAssertFalse(model.busy)
            XCTAssertFalse(model.showsPendingSyncWarning)
            XCTAssertTrue(model.hasPendingBackgroundSync)
            XCTAssertTrue(model.settings.loadBalanceEnabled)
            XCTAssertEqual(model.settings.lowQuotaPercent, 25)
            XCTAssertEqual(model.settings.upstreamAttemptLimit, 6)
            XCTAssertEqual(model.settings.upstreamRetryDelay, 9)
            XCTAssertEqual(model.settings.sources.first(where: { $0.id == "api-2" })?.enabled, false)
            XCTAssertFalse(ModelPolicy.isEnabled("gemini-2.5-pro", in: model.settings))

            let persisted = try MetadataStore(directory: directory).load()
            XCTAssertTrue(persisted.loadBalanceEnabled)
            XCTAssertEqual(persisted.lowQuotaPercent, 25)
            XCTAssertEqual(persisted.sources.first(where: { $0.id == "api-2" })?.enabled, false)

            let deadline = Date().addingTimeInterval(3)
            while model.hasPendingBackgroundSync, Date() < deadline {
                try await Task.sleep(for: .milliseconds(10))
            }
            XCTAssertFalse(model.hasPendingBackgroundSync)
            XCTAssertFalse(model.pendingSync)
            XCTAssertFalse(model.showsPendingSyncWarning)
            XCTAssertNil(model.error)

            let countAfter = Int(try String(contentsOf: syncCountFile)) ?? 0
            XCTAssertEqual(countAfter - countBefore, 1, "Rapid consecutive edits must coalesce into a single gateway sync")

            let applied = try JSONDecoder().decode(JSONValue.self, from: Data(contentsOf: directory.appendingPathComponent("applied-settings.json")))
            XCTAssertEqual(applied["load_balance"].bool, true)
            XCTAssertEqual(applied["official_low_quota_percent"].number, 25)
            XCTAssertEqual(applied["upstream_attempts"].number, 6)
            XCTAssertEqual(applied["upstream_retry_delay_seconds"].number, 9)
            XCTAssertEqual(applied["sources"].array.first(where: { $0["id"].string == "api-2" })?["enabled"].bool, false)
        }
    }

    private static let helperScript = #"""
    #!/usr/bin/env python3
    import json, os, pathlib, socket, sys, time
    directory = pathlib.Path(sys.argv[sys.argv.index('--state-dir') + 1])
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(str(directory / 'control.sock'))
    server.listen()
    api_keys = {}
    while True:
        connection, _ = server.accept()
        with connection:
            request = json.loads(connection.makefile('rb').readline())
            operation = request['op']
            if (directory / 'operations').exists():
                with (directory / 'operations').open('a') as log: log.write(operation + '\n')
            try:
                if operation == 'status':
                    mode = directory / 'connection-mode'
                    applied = directory / 'applied-settings.json'
                    sources = json.loads(applied.read_text())['sources'] if applied.exists() else []
                    for source in sources: source.pop('secret', None)
                    data = dict(process_id=os.getpid(), port=17863, inflight=0, sources=sources, connection_mode=mode.read_text() if mode.exists() else 'unmanaged')
                elif operation == 'gateway.update.info':
                    data = dict(protocol=1, version='0.8.26')
                elif operation == 'gateway.update.stop_when_idle':
                    data = dict(stopping=not (directory / 'active-stream').exists())
                elif operation == 'credentials.storage.status':
                    status = directory / 'storage-status.json'
                    data = json.loads(status.read_text()) if status.exists() else dict(mode='file', file_path=str(directory / 'legacy-file.json'))
                elif operation == 'models.list':
                    (directory / 'models-started').touch()
                    while (directory / 'hold-models').exists(): time.sleep(0.01)
                    data = dict(models=['old-account-model'])
                elif operation == 'models.info':
                    data = dict(models=[dict(id='custom-model',default_context_window=100000,default_max_context_window=200000,default_auto_compact_token_limit=None)])
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
                    notice = directory / 'updated-secrets.json'
                    data = dict(updated_secrets=json.loads(notice.read_text()) if notice.exists() else [])
                elif operation == 'credentials.save_api':
                    payload = request['payload']
                    api_keys[payload['source_id']] = dict(api_key=payload['api_key'])
                    data = {}
                elif operation == 'credentials.read':
                    sources = json.loads((directory / 'sources.json').read_text())['sources']
                    by_id = {source['id']: source for source in sources}
                    items = []
                    for source_id in request['payload']['source_ids']:
                        account = by_id.get(source_id, {}).get('account_id')
                        override = directory / ('account-' + source_id)
                        if override.exists(): account = override.read_text()
                        secret = dict(account_id=account, access_token='fixture-token') if account else api_keys.get(source_id)
                        items.append(dict(source_id=source_id, secret=secret))
                    data = dict(items=items)
                elif operation == 'official.refresh':
                    data = dict(quota=dict(stale=False, complete=True, fetched_at=int(time.time()), error=None))
                elif operation == 'sync':
                    counter = directory / 'sync-count'
                    counter.write_text(str((int(counter.read_text()) if counter.exists() else 0) + 1))
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
            if operation == 'gateway.update.stop_when_idle' and data['stopping']:
                while (directory / 'hold-exit').exists(): time.sleep(0.01)
                (directory / 'control.sock').unlink()
                break
    """#
}
