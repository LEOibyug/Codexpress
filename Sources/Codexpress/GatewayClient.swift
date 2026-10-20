import Foundation
import Darwin

struct GatewayClient: Sendable {
    let socketPath: String
    static let limit = 8 * 1024 * 1024
    func call(_ op: String, _ payload: JSONValue = .object([:])) async throws -> JSONValue {
        let path = socketPath
        return try await Task.detached(priority: .utility) {
            let id = UUID().uuidString
            let request: JSONValue = .object(["id": .string(id), "op": .string(op), "payload": payload])
            var data = try JSONEncoder().encode(request); data.append(10)
            guard data.count <= Self.limit else { throw AppError.message("管理请求超过 8 MiB 限制。") }
            let fd = socket(AF_UNIX, SOCK_STREAM, 0)
            guard fd >= 0 else { throw AppError.message("无法建立本机管理连接。") }
            defer { close(fd) }
            var noSig: Int32 = 1
            setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &noSig, socklen_t(MemoryLayout.size(ofValue: noSig)))
            let receiveSeconds: Int = op == "official.credits.consume" || op.hasPrefix("credentials.") ? 180 : ["official.refresh", "models.list"].contains(op) || op.hasPrefix("config.") ? 90 : op.hasPrefix("official.login.") ? 40 : 12
            var receiveTimeout = timeval(tv_sec: receiveSeconds, tv_usec: 0)
            var sendTimeout = timeval(tv_sec: 12, tv_usec: 0)
            setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &receiveTimeout, socklen_t(MemoryLayout.size(ofValue: receiveTimeout)))
            setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &sendTimeout, socklen_t(MemoryLayout.size(ofValue: sendTimeout)))
            var address = sockaddr_un(); address.sun_family = sa_family_t(AF_UNIX)
            let capacity = MemoryLayout.size(ofValue: address.sun_path)
            guard path.utf8.count < capacity else { throw AppError.message("状态目录路径过长，无法创建 Unix socket。") }
            withUnsafeMutablePointer(to: &address.sun_path) { pointer in
                pointer.withMemoryRebound(to: CChar.self, capacity: capacity) { chars in
                    path.withCString { _ = strlcpy(chars, $0, capacity) }
                }
            }
            let connected = withUnsafePointer(to: &address) { ptr in ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size)) } }
            guard connected == 0 else { throw AppError.message("无法连接网关。请检查网关是否已启动及本地端口是否被占用。") }
            try data.withUnsafeBytes { bytes in
                var offset = 0
                while offset < bytes.count {
                    let sent = Darwin.write(fd, bytes.baseAddress!.advanced(by: offset), bytes.count - offset)
                    guard sent > 0 else { throw AppError.message("向网关发送管理请求失败。") }; offset += sent
                }
            }
            var response = Data(); var buffer = [UInt8](repeating: 0, count: 8192)
            while response.count <= Self.limit {
                let n = Darwin.read(fd, &buffer, buffer.count)
                guard n > 0 else { throw AppError.message("网关管理请求超时或连接中断；涉及重置卡时请先核实结果。") }
                if let end = buffer[..<n].firstIndex(of: 10) { response.append(contentsOf: buffer[..<end]); break }
                response.append(contentsOf: buffer[..<n])
            }
            guard response.count <= Self.limit else { throw AppError.message("网关管理响应超过 8 MiB 限制。") }
            let result = try JSONDecoder().decode(JSONValue.self, from: response)
            guard result["id"].string == id else { throw AppError.message("网关响应标识不匹配。") }
            guard result["ok"].bool == true else { throw AppError.message(result["error"].string ?? "网关操作失败。") }
            return result["data"]
        }.value
    }
}
