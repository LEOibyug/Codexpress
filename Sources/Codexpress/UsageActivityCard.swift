import SwiftUI

enum UsageActivityPeriod: String, CaseIterable, Identifiable {
    case month, year
    var id: String { rawValue }
    var days: Int { self == .year ? 365 : 30 }
    var label: String { self == .year ? "年" : "月" }
}

private struct UsageActivityRefreshKey: Equatable {
    let query: UsageQuery?
    let reusesOverview: Bool
}

struct UsageActivityCard: View {
    let selection: UsageSelection
    @EnvironmentObject var model: AppModel
    private var period: UsageActivityPeriod { model.usageActivityPeriod }
    @State private var loadError: String?

    private var query: UsageQuery? {
        guard let selected = selection.query else { return nil }
        return UsageQuery.last(days: period.days, now: Date(timeIntervalSince1970: selected.to - 1), sourceID: selection.sourceID, modelID: selection.modelID)
    }
    private var reusesOverview: Bool { query != nil && query == selection.query }
    private var cachedSnapshot: UsageAnalyticsSnapshot? { model.cachedUsageAnalytics(query) }
    private var daily: [UsageDaily] {
        guard let query = cachedSnapshot?.query ?? query else { return [] }
        let existing = Dictionary((cachedSnapshot?.daily ?? []).map { ($0.date.timeIntervalSince1970, $0) }, uniquingKeysWith: { _, latest in latest })
        return query.dayStarts.dropLast().map { start in
            existing[start] ?? UsageDaily(.object(["start_at": .number(start), "input_tokens": .number(0), "output_tokens": .number(0), "cached_input_tokens": .number(0), "total_tokens": .number(0), "requests": .number(0), "total_usd": .number(0)]))
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            UsageActivityChart(days: daily, period: $model.usageActivityPeriod)
            if let loadError { InfoNote(text: loadError, color: .orange).padding(.horizontal, 2) }
        }
        .task(id: UsageRefreshKey(selection: UsageActivityRefreshKey(query: query, reusesOverview: reusesOverview), active: model.settingsWindowActive)) {
            let requestedKey = UsageActivityRefreshKey(query: query, reusesOverview: reusesOverview)
            loadError = nil
            guard model.settingsWindowActive, !reusesOverview else { return }
            var refresh = false
            while !Task.isCancelled {
                guard let requested = query else { break }
                do {
                    _ = try await model.usageAnalytics(requested, refresh: refresh)
                    guard !Task.isCancelled, requestedKey == UsageActivityRefreshKey(query: query, reusesOverview: reusesOverview) else { break }
                    loadError = nil
                } catch {
                    guard !Task.isCancelled, requestedKey == UsageActivityRefreshKey(query: query, reusesOverview: reusesOverview) else { break }
                    loadError = error.localizedDescription
                }
                refresh = true
                do { try await Task.sleep(for: .seconds(5)) } catch { break }
            }
        }
    }
}
