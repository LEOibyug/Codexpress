import AppKit
import Foundation

extension AppModel {
    func requireUniqueOfficialAccount(_ source: Source) throws {
        guard source.official, let accountID = source.account_id, !accountID.isEmpty,
              let existing = settings.sources.first(where: { $0.id != source.id && $0.official && $0.account_id == accountID }) else { return }
        throw AppError.message("此官方账号已连接到来源“\(existing.name)”，请使用已有来源；如需更换代理，请编辑该来源。")
    }

    func login(_ source: Source) {
        perform {
            try await self.sync()
            let result = try await self.client.call("official.login.start", .object(["source_id": .string(source.id)]))
            guard let urlString = result["auth_url"].string, let url = URL(string: urlString), url.scheme == "https", url.host == "auth.openai.com", let flow = result["flow_id"].string else { throw AppError.message("网关返回了无效的登录地址。") }
            self.loginSource = source.id; self.loginFlow = flow; self.loginMessage = "请在浏览器中完成登录。"; self.callbackURL = ""
            NSWorkspace.shared.open(url)
        }
    }
    func cancelLogin() {
        guard let sourceID = loginSource, let flowID = loginFlow else {
            callbackURL = ""; loginSource = nil; loginFlow = nil; loginMessage = nil
            return
        }
        perform {
            _ = try await self.client.call("official.login.cancel", .object(["source_id": .string(sourceID), "flow_id": .string(flowID)]))
            guard self.loginSource == sourceID, self.loginFlow == flowID else { return }
            self.callbackURL = ""; self.loginSource = nil; self.loginFlow = nil; self.loginMessage = nil
        }
    }
    func completeLogin() {
        guard let id = loginSource, let flow = loginFlow else { return }
        perform {
            _ = try await self.client.call("official.login.complete", .object(["source_id": .string(id), "flow_id": .string(flow), "callback_url": .string(self.callbackURL)]))
            try await self.finishLogin(id)
        }
    }
    func finishLogin(_ id: String) async throws {
        // The helper durably installs official credentials before returning; never replay an IPC token snapshot.
        try await loadCredentialSnapshot([id])
        let durable = try OfficialCredentialMirror.requireDurable(credentialSnapshot.load(id))
        var next = settings
        if let i = next.sources.firstIndex(where: { $0.id == id }) { next.sources[i].account_id = durable["account_id"].string }
        try persistSettings(next); callbackURL = ""; loginSource = nil; loginFlow = nil; loginMessage = "官方账号已连接。"
        officialRefreshSchedule.reset(id)
        lastModelRefresh[id] = nil
        scheduleModelRefresh()
        Task { await refreshAccount(id, foreground: true) }
    }
    func queryOfficial(_ id: String, includeCredits: Bool) async throws {
        if let pending = officialQueries[id] {
            try await pending.task.value
            if includeCredits && !pending.includeCredits {
                try await queryOfficial(id, includeCredits: true)
            }
            return
        }
        refreshingQuotas.insert(id)
        let task = Task<Void, Error> {
            defer { officialQueries[id] = nil; refreshingQuotas.remove(id) }
            officialRefreshSchedule.record(id, includeCredits: includeCredits)
            _ = try await client.call("official.refresh", .object(["source_id": .string(id), "include_credits": .bool(includeCredits)]))
            try await refreshCredentials()
            try await updateStatus()
        }
        officialQueries[id] = (includeCredits, task)
        try await task.value
    }
    func refresh(_ id: String) { perform { try await self.queryOfficial(id, includeCredits: true) } }
    func prepareCredit(source: Source, credit: JSONValue) async throws -> CreditConfirmation {
        try requireWritableMetadata()
        guard let before = settings.sources.first(where: { $0.id == source.id }), before == source else { throw AppError.message("来源已删除或发生变化，请重新选择重置卡。") }
        try await queryOfficial(source.id, includeCredits: true)
        guard let current = settings.sources.first(where: { $0.id == source.id }), current.sameConfiguration(as: before) else { throw AppError.message("来源已删除或网络策略发生变化，请重新选择重置卡。") }
        return try CreditConfirmation.make(source: current, live: state(source.id), creditID: credit["id"].string)
    }

    func resetResult(for source: Source, creditID: String) -> String? {
        let accountID = source.account_id ?? ""
        // Look up record identities rather than their dictionary keys: older app
        // versions used source IDs, including for account-bound records.
        let match = operations.filter {
            $0.value.matches(source: source, accountID: accountID, creditID: creditID)
        }.max { $0.value.createdAt < $1.value.createdAt }
        return match?.value.result
    }

    func isConsumingCredit(for source: Source, creditID: String) -> Bool {
        consuming.contains { key in
            operations[key]?.matches(source: source, accountID: source.account_id ?? "", creditID: creditID) == true
        }
    }

    func canSubmitCredit(for source: Source, creditID: String) -> Bool {
        guard source.official, let accountID = source.account_id, !accountID.isEmpty, !creditID.isEmpty,
              !consuming.contains("all"), resetResult(for: source, creditID: creditID) == nil else { return false }
        // Resetting one card changes the account's windows, so serialize all
        // cards for this account even when it appears in several sources.
        return !consuming.contains { key in
            guard let operation = operations[key] else { return false }
            return operation.accountID.map { $0 == accountID } ?? (operation.sourceID == source.id)
        }
    }

    func consume(_ confirmation: CreditConfirmation) {
        let source = confirmation.source
        guard let creditID = confirmation.credit["id"].string,
              canSubmitCredit(for: source, creditID: creditID) else { return }
        do {
            try requireWritableMetadata()
            guard let currentSource = settings.sources.first(where: { $0.id == source.id }), currentSource == source else { throw AppError.message("来源或账号已变化，请重新确认。") }
            try confirmation.validate(live: state(source.id))
        } catch { self.error = error.localizedDescription; return }

        // Length-prefix the account so arbitrary account/card IDs cannot collide.
        let key = "account:\(confirmation.accountID.utf8.count):\(confirmation.accountID):\(creditID)"
        let operationID = UUID().uuidString
        let intent = ResetOperation(sourceID: source.id, creditID: creditID, operationID: operationID, createdAt: Date().timeIntervalSince1970, result: "结果待核实，请刷新额度和卡片。", accountID: confirmation.accountID)
        var next = operations
        next[key] = intent
        // A failed intent write must leave both memory and disk unchanged: no
        // request has been sent, and retrying after repairing storage is safe.
        do { try ResetOperationStore(directory: directory).save(next) }
        catch { self.error = error.localizedDescription; return }
        operations = next
        consuming.insert(key)
        Task {
            defer {
                consuming.remove(key)
                do { try ResetOperationStore(directory: directory).save(operations) }
                catch { self.error = error.localizedDescription }
            }
            do {
                let result = try await client.call("official.credits.consume", confirmation.consumePayload(operationID: operationID))
                let labels = ["reset": "已重置；以刷新后的窗口为准", "nothing_to_reset": "当前无需重置", "no_credit": "无可用重置卡", "already_redeemed": "卡片已使用", "unknown": "结果待核实，请刷新，勿重复提交", "pending_verification": "结果待核实，请刷新，勿重复提交"]
                operations[key]?.result = labels[result["code"].string ?? "unknown"] ?? "结果待核实，请刷新"
                try await updateStatus()
            } catch {
                // Once sent, a transport failure cannot establish whether the
                // card was consumed. Keep the durable intent blocking retries.
                operations[key]?.result = "结果待核实，请先刷新额度和卡片。"
                self.error = error.localizedDescription
            }
        }
    }
}
