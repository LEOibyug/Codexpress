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
        VStack(alignment: .leading, spacing: 7) {
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

struct UsageDistributionChart: View {
    let groups: [JSONValue]
    @State private var groupBy = "source_id"
    @State private var hoveredSource: String?
    private let colors: [Color] = [.accentColor, .teal, .indigo, .orange]
    private var totals: UsageTotals { UsageTotals(rows: groups) }
    private var slices: [UsageSourceSlice] { usageDistributionSlices(groups, groupBy: groupBy) }
    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack {
                Text("用量占比").font(.system(size: 13, weight: .semibold))
                Spacer(minLength: 5)
                Picker("分组方式", selection: $groupBy) { Text("供应商").tag("source_id"); Text("模型").tag("model") }
                    .labelsHidden().pickerStyle(.segmented).frame(width: 108).controlSize(.small)
            }
            HStack(alignment: .center, spacing: 10) {
                UsageCompactRing(slices: slices, colors: colors, total: groupBy == "model" ? usageCount(totals.total, estimated: totals.estimated) : usageMoney(totals.cost, estimated: totals.costEstimated), caption: groupBy == "model" ? "总 Token" : "费用", hoveredSource: $hoveredSource)
                    .frame(width: 128, height: 128)
                VStack(alignment: .leading, spacing: 4) {
                    if slices.isEmpty { Text(groupBy == "model" ? "暂无 Token 用量" : "暂无已计价用量").font(.system(size: 11)).foregroundStyle(.secondary) }
                    ForEach(Array(slices.enumerated()), id: \.element.id) { index, slice in
                        HStack(alignment: .top, spacing: 6) {
                            RoundedRectangle(cornerRadius: 2).fill(colors[index]).frame(width: 3, height: 34).padding(.top, 1)
                            VStack(alignment: .leading, spacing: 4) {
                                HStack(alignment: .firstTextBaseline, spacing: 4) {
                                    Text(slice.name).font(.system(size: 12)).lineLimit(1).help(slice.name)
                                    Spacer(minLength: 0)
                                    Text(String(format: "%.1f%%", slice.value / max(1e-12, slices.reduce(0) { $0 + $1.value }) * 100))
                                        .font(.system(size: 10)).monospacedDigit().foregroundStyle(.secondary).fixedSize()
                                }
                                ViewThatFits(in: .horizontal) {
                                    HStack(alignment: .firstTextBaseline, spacing: 6) {
                                        sourceCost(slice).fixedSize()
                                        Spacer(minLength: 0)
                                        sourceTokens(slice).fixedSize()
                                    }
                                    VStack(alignment: .leading, spacing: 3) {
                                        sourceCost(slice)
                                        sourceTokens(slice)
                                    }
                                }
                            }
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, 5).padding(.vertical, 2)
                        .background(hoveredSource == slice.id ? colors[index].opacity(0.12) : Color.clear, in: RoundedRectangle(cornerRadius: 6))
                        .opacity(hoveredSource == nil || hoveredSource == slice.id ? 1 : 0.5)
                    }
                }.frame(maxWidth: .infinity, alignment: .leading)

            }.frame(maxHeight: .infinity)
            Text(groupBy == "model" ? "输入与输出 Token" : "上游扣费优先 · 缺失时使用参考价")
                .font(.system(size: 9)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
        }.padding(16).frame(height: 342)
            .onChange(of: groupBy) { _, _ in hoveredSource = nil }
            .onChange(of: slices.map(\.id)) { _, ids in if let hoveredSource, !ids.contains(hoveredSource) { self.hoveredSource = nil } }
    }
    private func sourceCost(_ slice: UsageSourceSlice) -> some View {
        Text(usageMoney(slice.cost, estimated: slice.costEstimated)).font(.system(size: 15, weight: .medium, design: .rounded)).monospacedDigit()
    }
    private func sourceTokens(_ slice: UsageSourceSlice) -> some View {
        Text(usageCount(slice.tokenTotal, estimated: slice.estimated) + " Token").font(.system(size: 11)).monospacedDigit().foregroundStyle(.secondary)
    }
}

private struct UsageCompactRing: View {
    let slices: [UsageSourceSlice]
    let colors: [Color]
    let total: String
    let caption: String
    @Binding var hoveredSource: String?
    var body: some View {
        Chart(Array(slices.enumerated()), id: \.element.id) { index, slice in
            SectorMark(angle: .value("用量", slice.value), innerRadius: .ratio(0.78), angularInset: slices.count > 1 ? 2 : 0)
                .foregroundStyle(colors[index]).cornerRadius(2)
                .opacity(hoveredSource == nil || hoveredSource == slice.id ? 1 : 0.32)
        }.chartLegend(.hidden)
            .chartBackground { _ in
                VStack(spacing: 4) {
                    Text(total).font(.system(size: 15, weight: .semibold, design: .rounded)).monospacedDigit().lineLimit(1).minimumScaleFactor(0.6)
                    Text(caption).font(.system(size: 8)).foregroundStyle(.secondary)
                }.frame(width: 90)
            }
            .background { if slices.isEmpty { Circle().stroke(AppPalette.well, lineWidth: 10).padding(5) } }
            .chartOverlay { proxy in
                GeometryReader { geometry in
                    Rectangle().fill(.clear).contentShape(Rectangle()).onContinuousHover { phase in
                        switch phase {
                        case .active(let location):
                            guard let plotFrame = proxy.plotFrame else { hoveredSource = nil; return }
                            let frame = geometry[plotFrame]
                            hoveredSource = usageRingHit(x: location.x - frame.midX, y: location.y - frame.midY, outerRadius: min(frame.width, frame.height) / 2, innerRatio: 0.78, slices: slices)
                        case .ended: hoveredSource = nil
                        }
                    }
                }
            }
    }
}
