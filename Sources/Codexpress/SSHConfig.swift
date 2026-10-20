import Foundation
import Darwin

/// Discovers explicit aliases only. OpenSSH still resolves login options and Match rules.
struct SSHConfigCatalog: Sendable {
    let hosts: [String]
    let exists: Bool
    let incomplete: Bool

    static func load(home: URL = FileManager.default.homeDirectoryForCurrentUser) -> SSHConfigCatalog {
        var reader = Reader(home: home)
        reader.read(home.appendingPathComponent(".ssh/config"), depth: 0)
        return SSHConfigCatalog(hosts: reader.hosts.sorted { $0.localizedStandardCompare($1) == .orderedAscending }, exists: reader.exists, incomplete: reader.incomplete)
    }

    private struct Reader {
        let home: URL
        var hosts: [String] = []
        var hostNames: Set<String> = []
        var visited: Set<String> = []
        var bytes = 0
        var exists = false
        var incomplete = false

        mutating func read(_ url: URL, depth: Int) {
            let path = url.resolvingSymlinksInPath().path
            guard !visited.contains(path) else { return }
            guard depth <= 16, visited.count < 128 else { incomplete = true; return }
            visited.insert(path)
            let fd = Darwin.open(path, O_RDONLY | O_NONBLOCK | O_CLOEXEC)
            guard fd >= 0 else {
                if errno != ENOENT { incomplete = true }
                return
            }
            if depth == 0 { exists = true }
            let file = FileHandle(fileDescriptor: fd, closeOnDealloc: true)
            defer { try? file.close() }
            var info = stat()
            guard fstat(fd, &info) == 0, (info.st_mode & S_IFMT) == S_IFREG,
                  info.st_size <= 1024 * 1024, bytes + Int(info.st_size) <= 4 * 1024 * 1024 else { incomplete = true; return }
            let data: Data
            do { data = try file.read(upToCount: 1024 * 1024 + 1) ?? Data() }
            catch { incomplete = true; return }
            guard data.count <= 1024 * 1024, bytes + data.count <= 4 * 1024 * 1024,
                  let text = String(data: data, encoding: .utf8), !text.contains("\0") else { incomplete = true; return }
            bytes += data.count
            for line in text.split(whereSeparator: \.isNewline) {
                guard let (keyword, values) = Self.directive(String(line)) else { continue }
                if keyword == "host" {
                    for name in values where Self.isAlias(name) && hostNames.insert(name.lowercased()).inserted {
                        guard hosts.count < 5000 else { incomplete = true; return }
                        hosts.append(name)
                    }
                } else if keyword == "include" {
                    for pattern in values { include(pattern, depth: depth + 1) }
                }
            }
        }

        mutating func include(_ pattern: String, depth: Int) {
            let absolute: String
            if pattern.hasPrefix("~/") { absolute = home.appendingPathComponent(String(pattern.dropFirst(2))).path }
            else if pattern.hasPrefix("/") { absolute = pattern }
            else if pattern.hasPrefix("~") || pattern.contains("${") || pattern.contains("%") {
                // Dynamic host/user expansion is left to OpenSSH at connection time.
                incomplete = true; return
            } else { absolute = home.appendingPathComponent(".ssh").appendingPathComponent(pattern).path }
            var matches = glob_t()
            let result = absolute.withCString { Darwin.glob($0, GLOB_ERR | GLOB_LIMIT, nil, &matches) }
            defer { globfree(&matches) }
            guard result == 0 else { if result != GLOB_NOMATCH { incomplete = true }; return }
            guard matches.gl_pathc <= 128 else { incomplete = true; return }
            for index in 0..<Int(matches.gl_pathc) {
                if let name = matches.gl_pathv[index] { read(URL(fileURLWithPath: String(cString: name)), depth: depth) }
            }
        }

        static func isAlias(_ name: String) -> Bool {
            let allowed = CharacterSet(charactersIn: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.-_:")
            return !name.isEmpty && !name.hasPrefix("-") && name.utf8.count <= 253 && name.unicodeScalars.allSatisfy(allowed.contains)
        }

        static func directive(_ raw: String) -> (String, [String])? {
            var line = raw.drop(while: \.isWhitespace)
            let keyword = line.prefix { !$0.isWhitespace && $0 != "=" }.lowercased()
            guard keyword == "host" || keyword == "include" else { return nil }
            line = line.dropFirst(keyword.count).drop(while: \.isWhitespace)
            if line.first == "=" { line = line.dropFirst().drop(while: \.isWhitespace) }
            var words: [String] = [], word = ""
            var quote: Character?, escaped = false
            for character in line {
                if escaped { word.append(character); escaped = false; continue }
                if character == "\\" { escaped = true; continue }
                if let current = quote {
                    if character == current { quote = nil } else { word.append(character) }
                } else if character == "\"" || character == "'" { quote = character }
                else if character == "#" { break }
                else if character.isWhitespace {
                    if !word.isEmpty { words.append(word); word = "" }
                } else { word.append(character) }
            }
            guard quote == nil, !escaped else { return nil }
            if !word.isEmpty { words.append(word) }
            return (keyword, words)
        }
    }
}
