import Foundation

@MainActor final class TerminationGuard {
    private var pending = false

    // nil means another quit request is already awaiting its single reply.
    func request(check: () async throws -> Int,
                 confirm: (Int?) -> Bool,
                 shutdown: () async -> Void) async -> Bool? {
        guard !pending else { return nil }
        pending = true
        defer { pending = false }
        let active: Int?
        do { active = try await check() }
        catch { active = nil }
        if active != 0 && !confirm(active) { return false }
        await shutdown()
        return true
    }
}
