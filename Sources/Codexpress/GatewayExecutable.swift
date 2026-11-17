import Darwin
import Foundation
import Security

struct GatewayExecutable {
    let directory: URL

    static func bundled() throws -> URL {
        guard let executable = Bundle.main.executableURL else { throw AppError.message("无法找到应用内的网关。") }
        return executable.deletingLastPathComponent().appendingPathComponent("codexpress-gateway")
    }

    static func installedVersion() throws -> String {
        let plist = Bundle.main.bundleURL.appendingPathComponent("Contents/Info.plist")
        let data = try Data(contentsOf: plist)
        let contents = try PropertyListSerialization.propertyList(from: data, options: [], format: nil) as? [String: Any]
        guard contents?["CFBundleIdentifier"] as? String == Bundle.main.bundleIdentifier,
              let version = contents?["CFBundleShortVersionString"] as? String else {
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

    static func validate(_ executable: URL) throws {
        var code: SecStaticCode?
        guard SecStaticCodeCreateWithPath(executable as CFURL, [], &code) == errSecSuccess,
              let code, SecStaticCodeCheckValidity(code, [], nil) == errSecSuccess else {
            throw AppError.message("新版网关的代码签名无效，当前版本保持运行。")
        }
    }
}
