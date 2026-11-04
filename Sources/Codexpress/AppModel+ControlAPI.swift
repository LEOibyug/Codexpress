import AppKit
import Foundation

extension AppModel {
    private var canPresentControlReset: Bool {
        running && !busy && creditResetRequest == nil && loginSource == nil && consuming.isEmpty
            && NSApp?.modalWindow == nil
            && !(NSApp?.windows.contains(where: { $0.attachedSheet != nil }) ?? false)
    }

    func scheduleControlResetPoll() {
        guard canPresentControlReset, controlResetPollTask == nil else { return }
        controlResetPollTask = Task { [weak self] in
            guard let self else { return }
            defer { controlResetPollTask = nil }
            do {
                let pending = try await client.call("control.reset.pending")
                guard canPresentControlReset, !Task.isCancelled else { return }
                for item in pending["requests"].array {
                    guard let id = item["id"].string,
                          let sourceID = item["source_id"].string,
                          let accountID = item["account_id"].string else { continue }
                    guard let source = settings.sources.first(where: {
                        $0.id == sourceID && $0.official && $0.account_id == accountID
                            && self.state($0.id)["account_id"].string == accountID
                    }) else {
                        _ = try await client.call("control.reset.cancel", .object(["id": .string(id)]))
                        continue
                    }
                    let presented = try await client.call("control.reset.present", .object(["id": .string(id)]))
                    guard presented["claimed"].bool == true else { continue }
                    guard canPresentControlReset, !Task.isCancelled,
                          settings.sources.contains(source), state(sourceID)["account_id"].string == accountID else {
                        _ = try await client.call("control.reset.cancel", .object(["id": .string(id)]))
                        return
                    }
                    AppDelegate.current?.showSettings(destination: .sources)
                    creditResetRequest = CreditResetRequest(source: source, quickReset: true, managementRequestID: id, creditID: item["credit_id"].string)
                    presentedControlResetRequest = creditResetRequest
                    break
                }
                controlResetPollingError = nil
            } catch {
                let message = "无法读取管理 API 重置请求：\(error.localizedDescription)"
                if controlResetPollingError != message { self.error = message }
                controlResetPollingError = message
            }
        }
    }

    func cancelControlReset(_ request: CreditResetRequest) {
        guard let id = request.managementRequestID, presentedControlResetRequest?.managementRequestID == id else { return }
        presentedControlResetRequest = nil
        Task {
            do { _ = try await client.call("control.reset.cancel", .object(["id": .string(id)])) }
            catch { self.error = "无法取消管理 API 重置请求：\(error.localizedDescription)" }
        }
    }

    func controlResetDismissed() {
        if let request = presentedControlResetRequest { cancelControlReset(request) }
    }
}
