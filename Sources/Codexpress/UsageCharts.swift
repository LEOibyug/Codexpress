import SwiftUI
import Charts

struct UsageTrendChart: View {
    let days: [UsageDaily]
    @State private var selectedDate: Date?
    private var selected: UsageDaily? {
        guard let selectedDate else { return nil }
        return days.min { abs($0.date.timeIntervalSince(selectedDate)) < abs($1.date.timeIntervalSince(selectedDate)) }
    }
    private var total: Double { days.reduce(0) { $0 + $1.total } }
    var body: some View {
        VStack(alignment: .leading, spacing: 15) {
            HStack(alignment: .firstTextBaseline) {
                Text("Token 趋势").font(.system(size: 13, weight: .semibold))
                Spacer()
                if let selected {
                    Text(selected.date.formatted(.dateTime.month().day()) + " · " + usageCount(selected.total, estimated: selected.estimated) + " Token")
                        .font(.system(size: 11, weight: .medium)).monospacedDigit()
                } else { Text("每日输入 + 输出").font(.system(size: 10)).foregroundStyle(.secondary) }
            }
            Chart {
                ForEach(days) { day in
                    AreaMark(x: .value("日期", day.date), y: .value("Token", day.total))
                        .foregroundStyle(LinearGradient(colors: [.accentColor.opacity(0.20), .accentColor.opacity(0.015)], startPoint: .top, endPoint: .bottom))
                        .interpolationMethod(.monotone)
                    LineMark(x: .value("日期", day.date), y: .value("Token", day.total))
                        .foregroundStyle(Color.accentColor).lineStyle(StrokeStyle(lineWidth: 2))
                        .interpolationMethod(.monotone)
                }
                if let selected {
                    RuleMark(x: .value("日期", selected.date)).foregroundStyle(Color.secondary.opacity(0.3)).lineStyle(StrokeStyle(dash: [3, 3]))
                    PointMark(x: .value("日期", selected.date), y: .value("Token", selected.total))
                        .foregroundStyle(Color.accentColor).symbolSize(38)
                }
            }
            .chartYScale(domain: 0...max(1, (days.map(\.total).max() ?? 0) * 1.12))
            .chartXAxis { AxisMarks(values: .automatic(desiredCount: 5)) { _ in AxisValueLabel(format: .dateTime.month().day()); AxisTick() } }
            .chartYAxis { AxisMarks(position: .leading, values: .automatic(desiredCount: 4)) { value in AxisGridLine(stroke: StrokeStyle(lineWidth: 0.5)).foregroundStyle(Color.secondary.opacity(0.16)); AxisValueLabel { if let number = value.as(Double.self) { Text(usageCount(number)).font(.system(size: 9)).monospacedDigit() } } } }
            .chartXSelection(value: $selectedDate)
            .chartOverlay { proxy in
                GeometryReader { geometry in
                    Rectangle().fill(.clear).contentShape(Rectangle()).onContinuousHover { phase in
                        switch phase {
                        case .active(let location):
                            if let frame = proxy.plotFrame { selectedDate = proxy.value(atX: location.x - geometry[frame].origin.x) }
                        case .ended: selectedDate = nil
                        }
                    }
                }
            }
            .frame(height: 165)
            HStack(spacing: 16) {
                tokenLabel("输入", value: days.reduce(0) { $0 + $1.input })
                tokenLabel("缓存读取", value: days.reduce(0) { $0 + $1.cached })
                tokenLabel("输出", value: days.reduce(0) { $0 + $1.output })
                Spacer()
                Text("总计 " + usageCount(total)).fontWeight(.medium)
            }.font(.system(size: 10)).monospacedDigit()
        }.padding(18).cardSurface()
    }
    private func tokenLabel(_ label: String, value: Double) -> some View {
        HStack(spacing: 5) { Text(label).foregroundStyle(.secondary); Text(usageCount(value)) }
    }
}

struct UsageActivityChart: View {
    let days: [UsageDaily]
    @Environment(\.colorScheme) private var colorScheme
    private var maximum: Double { days.map(\.total).max() ?? 0 }
    private var leading: Int { days.first.map { (Calendar.current.component(.weekday, from: $0.date) + 5) % 7 } ?? 0 }
    private var weeks: Int { max(1, Int(ceil(Double(days.count + leading) / 7))) }
    private var activeDays: Int { days.filter { $0.total > 0 }.count }
    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack {
                Text("每日活跃").font(.system(size: 13, weight: .semibold))
                Spacer()
                Text("\(activeDays) / \(days.count) 天有 Token 用量").font(.system(size: 10)).foregroundStyle(.secondary)
            }
            UsageActivityGridLayout(dayCount: days.count, leading: leading) {
                GeometryReader { geometry in
                let cell = UsageActivityLayout(width: geometry.size.width, days: days.count, leading: leading).cell
                HStack(alignment: .top, spacing: 5) {
                    VStack(spacing: 3) {
                        ForEach(0..<7, id: \.self) { weekday in
                            Text(["一", "", "三", "", "五", "", "日"][weekday]).font(.system(size: 9)).foregroundStyle(.tertiary).frame(width: 16, height: cell)
                        }
                    }
                    HStack(alignment: .top, spacing: 3) {
                        ForEach(0..<weeks, id: \.self) { week in
                            VStack(spacing: 3) {
                                ForEach(0..<7, id: \.self) { weekday in
                                    let index = week * 7 + weekday - leading
                                    if days.indices.contains(index) {
                                        let day = days[index]
                                        RoundedRectangle(cornerRadius: min(3, cell / 4)).fill(color(usageActivityLevel(day.total, maximum: maximum)))
                                            .overlay(RoundedRectangle(cornerRadius: min(3, cell / 4)).strokeBorder(Color.primary.opacity(0.045), lineWidth: 0.5))
                                            .frame(width: cell, height: cell)
                                            .help(day.date.formatted(.dateTime.year().month().day()) + "\n" + usageCount(day.total, estimated: day.estimated) + " Token · " + usageCount(day.requests) + " 次请求")
                                            .accessibilityLabel(day.date.formatted(.dateTime.month().day()) + "，" + usageCount(day.total) + " Token")
                                    } else { Color.clear.frame(width: cell, height: cell) }
                                }
                            }
                        }
                    }
                    Spacer(minLength: 0)
                    if weeks < 12 {
                        VStack(alignment: .trailing, spacing: 7) {
                            Text(usageCount(days.reduce(0) { $0 + $1.total })).font(.system(size: 24, weight: .medium, design: .rounded)).monospacedDigit()
                            Text("Token").font(.system(size: 10)).foregroundStyle(.secondary)
                            Spacer()
                            if let busiest = days.max(by: { $0.total < $1.total }), busiest.total > 0 {
                                Text("最高用量日").font(.system(size: 10)).foregroundStyle(.secondary)
                                Text(busiest.date.formatted(.dateTime.month().day())).font(.system(size: 12, weight: .medium))
                                Text(usageCount(busiest.total) + " Token").font(.system(size: 10)).foregroundStyle(.secondary)
                            }
                        }.frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .trailing)
                    }
                }
                }
            }
            HStack(spacing: 4) {
                Text(days.first?.date.formatted(.dateTime.month().day()) ?? "").foregroundStyle(.secondary)
                Text("–").foregroundStyle(.tertiary)
                Text(days.last?.date.formatted(.dateTime.month().day()) ?? "").foregroundStyle(.secondary)
                Spacer(); Text("少").foregroundStyle(.secondary)
                ForEach(0...4, id: \.self) { level in RoundedRectangle(cornerRadius: 2).fill(color(level)).frame(width: 10, height: 10) }
                Text("多").foregroundStyle(.secondary)
            }.font(.system(size: 9))
        }.padding(18).cardSurface()
    }
    private func color(_ level: Int) -> Color {
        let light: [UInt32] = [0xEBF0ED, 0xA9DFB2, 0x6EC481, 0x38A35A, 0x177B3A]
        let dark: [UInt32] = [0x303A33, 0x225B35, 0x287A42, 0x39A456, 0x66CF7F]
        let hex = (colorScheme == .dark ? dark : light)[min(4, max(0, level))]
        return Color(red: Double((hex >> 16) & 255) / 255, green: Double((hex >> 8) & 255) / 255, blue: Double(hex & 255) / 255)
    }
}

private struct UsageActivityGridLayout: Layout {
    let dayCount: Int
    let leading: Int
    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let width = proposal.width ?? 600
        return CGSize(width: width, height: UsageActivityLayout(width: width, days: dayCount, leading: leading).height)
    }
    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        for subview in subviews { subview.place(at: bounds.origin, anchor: .topLeading, proposal: ProposedViewSize(bounds.size)) }
    }
}

struct UsageDistributionChart: View {
    let groups: [JSONValue]
    @State private var mode = "cost"
    private let colors: [Color] = [.accentColor, .teal, .indigo, .orange]
    private var totals: UsageTotals { UsageTotals(rows: groups) }
    private var slices: [UsageSourceSlice] { usageSourceSlices(groups, mode: mode) }
    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack {
                Text("来源占比").font(.system(size: 13, weight: .semibold))
                Spacer(minLength: 5)
                Picker("统计方式", selection: $mode) { Text("费用").tag("cost"); Text("Token").tag("tokens") }
                    .labelsHidden().pickerStyle(.segmented).frame(width: 108).controlSize(.small)
            }
            HStack(alignment: .center, spacing: 10) {
                UsageCompactRing(slices: slices, colors: colors, total: mode == "cost" ? usageMoney(totals.cost) : usageCount(totals.total, estimated: totals.estimated), caption: mode == "cost" ? "参考费用" : "总 Token")
                    .frame(width: 128, height: 128)
                VStack(alignment: .leading, spacing: 15) {
                    if slices.isEmpty { Text(mode == "cost" ? "暂无已计价用量" : "暂无 Token 用量").font(.system(size: 11)).foregroundStyle(.secondary) }
                    ForEach(Array(slices.enumerated()), id: \.element.id) { index, slice in
                        HStack(alignment: .top, spacing: 6) {
                            RoundedRectangle(cornerRadius: 2).fill(colors[index]).frame(width: 3, height: 32).padding(.top, 1)
                            VStack(alignment: .leading, spacing: 4) {
                                Text(slice.name).font(.system(size: 12)).lineLimit(1).help(slice.name)
                                Text(mode == "cost" ? usageMoney(slice.value) : usageCount(slice.value))
                                    .font(.system(size: 15, weight: .medium, design: .rounded)).monospacedDigit().lineLimit(1).minimumScaleFactor(0.65)
                            }
                        }
                    }
                }.frame(maxWidth: .infinity, alignment: .leading)

            }.frame(maxHeight: .infinity)
            Text(mode == "cost" ? totals.costCoverage + "。按官方 API 价格折算，非实际账单。" + (totals.sum("unpriced_requests") > 0 ? "未收录价格不计入占比。" : "") : "输入与输出合计。缓存读取包含在输入内。")
                .font(.system(size: 9)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
        }.padding(16).frame(height: 342)
    }
}

private struct UsageCompactRing: View {
    let slices: [UsageSourceSlice]
    let colors: [Color]
    let total: String
    let caption: String
    var body: some View {
        Chart(Array(slices.enumerated()), id: \.element.id) { index, slice in
            SectorMark(angle: .value("用量", slice.value), innerRadius: .ratio(0.78), angularInset: slices.count > 1 ? 2 : 0)
                .foregroundStyle(colors[index]).cornerRadius(2)
        }.chartLegend(.hidden)
            .chartBackground { _ in
                VStack(spacing: 4) {
                    Text(total).font(.system(size: 15, weight: .semibold, design: .rounded)).monospacedDigit().lineLimit(1).minimumScaleFactor(0.6)
                    Text(caption).font(.system(size: 8)).foregroundStyle(.secondary)
                }.frame(width: 90)
            }
            .background { if slices.isEmpty { Circle().stroke(AppPalette.well, lineWidth: 10).padding(5) } }
    }
}
