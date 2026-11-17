import Darwin
import Foundation
import Security

struct GatewayExecutable {
    let directory: URL

    struct Update: Equatable {
        let version: String
        let requiresRestart: Bool
    }

    static func installedUpdate(after currentVersion: String) throws -> Update? {
        let version = try installedVersion()
        guard version.compare(currentVersion, options: .numeric) == .orderedDescending else { return nil }
        let gateway = try bundled()
        guard FileManager.default.isExecutableFile(atPath: gateway.path) else {
            throw AppError.message("未找到随应用打包的网关，请确认 Codexpress.app 已完整复制。")
        }
        let sameSeries = version.split(separator: ".").prefix(2).elementsEqual(currentVersion.split(separator: ".").prefix(2))
        return Update(version: version, requiresRestart: !sameSeries)
    }

    static func validateInstalledBundle() throws {
        try validate(Bundle.main.bundleURL, failure: "磁盘上的 Codexpress.app 签名无效或尚未完整复制，当前网关保持运行。")
    }

    static func bundled() throws -> URL {
        guard let executable = Bundle.main.executableURL else { throw AppError.message("无法找到应用内的网关。") }
        return executable.deletingLastPathComponent().appendingPathComponent("codexpress-gateway")
    }

    private static func installedVersion() throws -> String {
        let plist = Bundle.main.bundleURL.appendingPathComponent("Contents/Info.plist")
        let data = try Data(contentsOf: plist)
        let contents = try PropertyListSerialization.propertyList(from: data, options: [], format: nil) as? [String: Any]
        guard contents?["CFBundleIdentifier"] as? String == Bundle.main.bundleIdentifier,
              let version = contents?["CFBundleShortVersionString"] as? String,
              version.range(of: #"^[0-9]+\.[0-9]+\.[0-9]+$"#, options: .regularExpression) != nil else {
            throw AppError.message("无法识别已安装的 Codexpress 版本。")
        }
        return version
    }

    func stage(_ source: URL, replacing slot: String) throws -> URL {
        let files = FileManager.default
        guard files.isExecutableFile(atPath: source.path) else { throw AppError.message("未找到随应用打包的网关。") }
        try files.createDirectory(at: directory, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        let target = directory.appendingPathComponent(slot)
        if files.contentsEqual(atPath: source.path, andPath: target.path) { return target }
        let temporary = directory.appendingPathComponent(".gateway-\(UUID().uuidString)")
        defer { try? files.removeItem(at: temporary) }
        try files.copyItem(at: source, to: temporary)
        guard rename(temporary.path, target.path) == 0 else { throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
        return target
    }

    static func validate(_ url: URL, failure: String = "新版网关的代码签名无效，当前版本保持运行。") throws {
        var code: SecStaticCode?
        guard SecStaticCodeCreateWithPath(url as CFURL, [], &code) == errSecSuccess,
              let code, SecStaticCodeCheckValidity(code, [], nil) == errSecSuccess else {
            throw AppError.message(failure)
        }
    }
}
