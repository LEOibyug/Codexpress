import Foundation

struct ImageClientAccess {
    static var directory: URL { FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".codexpress", isDirectory: true) }
    static var file: URL { directory.appendingPathComponent("image-client.json") }

    static func export(baseURL: String, key: String, directory: URL = directory) throws -> URL {
        guard let url = URLComponents(string: baseURL), url.scheme == "http", ["127.0.0.1", "localhost"].contains(url.host ?? ""),
              let port = url.port, (1...65535).contains(port), url.path == "/v1", url.user == nil, url.password == nil, url.query == nil, url.fragment == nil,
              (16...256).contains(key.utf8.count), key.utf8.allSatisfy({ (33...126).contains($0) }) else { throw AppError.message("图片生成接入信息无效。") }
        let manager = FileManager.default
        if let attributes = try? manager.attributesOfItem(atPath: directory.path), attributes[.type] as? FileAttributeType != .typeDirectory { throw AppError.message("图片接入配置目录不是普通目录。") }
        try manager.createDirectory(at: directory, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        try manager.setAttributes([.posixPermissions: 0o700], ofItemAtPath: directory.path)
        let file = directory.appendingPathComponent("image-client.json")
        if let attributes = try? manager.attributesOfItem(atPath: file.path), attributes[.type] as? FileAttributeType != .typeRegular { throw AppError.message("图片接入配置不是普通文件。") }
        let data = try JSONEncoder().encode(["base_url": baseURL, "api_key": key])
        let temp = directory.appendingPathComponent(".image-client-\(UUID().uuidString)")
        defer { try? manager.removeItem(at: temp) }
        guard manager.createFile(atPath: temp.path, contents: data, attributes: [.posixPermissions: 0o600]), rename(temp.path, file.path) == 0 else { throw AppError.message("无法保存图片接入配置。") }
        return file
    }

    static func prompt(bundle: URL, config: URL) -> String {
        let skill = bundle.appendingPathComponent("Contents/Resources/skills/codexpress-image/SKILL.md").path
        return """
        请接入本机 Codexpress 的图片生成功能，供我之后提出生图或改图需求时使用。
        先阅读应用内的 skill：\(skill)
        按该 skill 使用同目录 scripts/image.py，调用本机独立 Images API。连接配置位于：\(config.path)
        密钥仅从本机配置读取，不要打印、发到对话或写入项目。不要改动其他供应商的密钥、Codex 配置或系统代理。
        此功能使用 Codexpress 中已登录官方账号的套餐额度，默认模型 gpt-image-2，支持生成和编辑 PNG 图片。即使当前文字对话使用第三方 API，也使用这个独立入口生图。
        本次只学习接入方式；可以运行脚本的 check 命令，但不要生成测试图片。之后根据我的实际需求操作，超时或结果未知时不要自动重试。图片保存到指定项目目录，并展示或告知完整路径。
        """
    }
}
