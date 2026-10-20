import Foundation
import UserNotifications

@MainActor final class LocalNotifications: NSObject, UNUserNotificationCenterDelegate {
    static let shared = LocalNotifications()
    private var delivered = Set<String>()
    override init() { super.init(); UNUserNotificationCenter.current().delegate = self }
    func deliver(_ events: [JSONValue]) async -> (ids: [String], undelivered: String?) {
        guard !events.isEmpty else { return ([], nil) }
        let center = UNUserNotificationCenter.current()
        let allowed: Bool
        do { allowed = try await center.requestAuthorization(options: [.alert]) }
        catch { return ([], "无法发送系统通知，请检查 macOS 的 Codexpress 通知设置。") }
        var ids = [String]()
        var message: String?
        for event in events {
            guard let id = event["id"].string else { continue }
            if delivered.contains(id) { ids.append(id); continue }
            let title = event["title"].string ?? "Codexpress"
            let body = event["body"].string ?? ""
            if allowed {
                let content = UNMutableNotificationContent(); content.title = title; content.body = body
                do { try await center.add(UNNotificationRequest(identifier: id, content: content, trigger: nil)) }
                catch { message = [message, title + "：" + body].compactMap { $0 }.joined(separator: "\n"); continue }
            } else { message = [message, title + "：" + body + " 请在系统设置中允许 Codexpress 通知。"].compactMap { $0 }.joined(separator: "\n") }
            delivered.insert(id); ids.append(id)
        }
        return (ids, message)
    }
    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification) async -> UNNotificationPresentationOptions { [.banner, .list] }
}
