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
    let snapshot: JSONValue
    let overviewQuery: UsageQuery?
    @EnvironmentObject var model: AppModel
    @State private var period: UsageActivityPeriod = .month
    @State private var activitySnapshot: JSONValue = .null
    @State private var loadedQuery: UsageQuery?
    @State private var loadError: String?

    private var query: UsageQuery? {
        guard let selected = selection.query else { return nil }
        return UsageQuery.last(days: period.days, now: Date(timeIntervalSince1970: selected.to - 1), sourceID: selection.sourceID, modelID: selection.modelID)
    }
    private var reusesOverview: Bool { query != nil && query == selection.query && query == overviewQuery }
    private var daily: [UsageDaily] {
        guard let query else { return [] }
        let data = reusesOverview ? snapshot : loadedQuery == query ? activitySnapshot : .null
        let existing = Dictionary(data["daily"].array.compactMap { day -> (Double, JSONValue)? in
            guard let start = day["start_at"].number else { return nil }
            return (start, day)
        }, uniquingKeysWith: { _, latest in latest })
        return query.dayStarts.dropLast().map { start in
            UsageDaily(existing[start] ?? .object(["start_at": .number(start), "input_tokens": .number(0), "output_tokens": .number(0), "cached_input_tokens": .number(0), "total_tokens": .number(0), "requests": .number(0), "total_usd": .number(0)]))
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            UsageActivityChart(days: daily, period: $period)
            if let loadError { InfoNote(text: loadError, color: .orange).padding(.horizontal, 3) }
        }
        .task(id: UsageRefreshKey(selection: UsageActivityRefreshKey(query: query, reusesOverview: reusesOverview), active: model.settingsWindowActive)) {
            loadError = nil
            guard model.settingsWindowActive, !reusesOverview else { return }
            while !Task.isCancelled {
                guard let requested = query else { break }
                do {
                    let result = try await model.usageAnalytics(requested)
                    guard !Task.isCancelled, requested == query else { break }
                    activitySnapshot = result; loadedQuery = requested; loadError = nil
                } catch {
                    guard !Task.isCancelled else { break }
                    loadError = error.localizedDescription
                }
                do { try await Task.sleep(for: .seconds(5)) } catch { break }
            }
        }
    }
}
