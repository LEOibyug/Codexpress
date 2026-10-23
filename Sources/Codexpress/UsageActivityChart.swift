import SwiftUI

struct UsageActivityChart: View {
    let days: [UsageDaily]
    @Binding var period: UsageActivityPeriod
    @Environment(\.colorScheme) private var colorScheme
    @State private var hoveredDate: Date?
    @State private var pinnedDate: Date?
    private var maximum: Double { days.map(\.total).max() ?? 0 }
    private var leading: Int { days.first.map { (Calendar.current.component(.weekday, from: $0.date) + 5) % 7 } ?? 0 }
    private var weeks: Int { max(1, Int(ceil(Double(days.count + leading) / 7))) }
    private var activeDays: Int { days.filter { $0.total > 0 }.count }
    private var selectedDay: UsageDaily? { days.first { $0.date == (hoveredDate ?? pinnedDate) } }
    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            header
            UsageActivityGridLayout(dayCount: days.count, leading: leading) {
                GeometryReader { geometry in
                    activityGrid(width: geometry.size.width)
                }
            }
            activityDetails
            legend
        }.padding(18).cardSurface()
            .onChange(of: period) { _, _ in hoveredDate = nil; pinnedDate = nil }
    }
    private var header: some View {
        HStack {
            Text("每日活跃").font(.system(size: 13, weight: .semibold))
            Spacer()
            Text("\(activeDays) / \(days.count) 天有用量").font(.system(size: 10)).foregroundStyle(.secondary)
            Picker("活跃范围", selection: $period) {
                ForEach(UsageActivityPeriod.allCases) { item in Text(item.label).tag(item) }
            }.labelsHidden().pickerStyle(.segmented).controlSize(.small).frame(width: 86)
        }
    }
    private func activityGrid(width: CGFloat) -> some View {
        let layout = UsageActivityLayout(width: width, days: days.count, leading: leading)
        let cellWidth = period == .year ? layout.cell : max(4, (width - 21) / Double(weeks) - 3)
        return HStack(alignment: .top, spacing: 5) {
            VStack(spacing: 3) {
                ForEach(0..<7, id: \.self) { weekday in
                    Text(["一", "", "三", "", "五", "", "日"][weekday])
                        .font(.system(size: 9)).foregroundStyle(.tertiary)
                        .frame(width: 16, height: layout.cell)
                }
            }
            HStack(alignment: .top, spacing: 3) {
                ForEach(0..<weeks, id: \.self) { week in
                    activityWeek(week, width: cellWidth, height: layout.cell)
                }
            }
        }
    }
    private func activityWeek(_ week: Int, width: CGFloat, height: CGFloat) -> some View {
        VStack(spacing: 3) {
            ForEach(0..<7, id: \.self) { weekday in
                let index = week * 7 + weekday - leading
                if days.indices.contains(index) {
                    activityCell(days[index], width: width, height: height)
                } else {
                    RoundedRectangle(cornerRadius: min(3, height / 4)).fill(color(0))
                        .overlay(RoundedRectangle(cornerRadius: min(3, height / 4))
                            .strokeBorder(Color.primary.opacity(0.045), lineWidth: 0.5))
                        .frame(width: width, height: height).accessibilityHidden(true)
                }
            }
        }
    }
    private var legend: some View {
        HStack(spacing: 4) {
            Text(days.first?.date.formatted(.dateTime.month().day()) ?? "").foregroundStyle(.secondary)
            Text("–").foregroundStyle(.tertiary)
            Text(days.last?.date.formatted(.dateTime.month().day()) ?? "").foregroundStyle(.secondary)
            Spacer(); Text("少").foregroundStyle(.secondary)
            ForEach(0...4, id: \.self) { level in RoundedRectangle(cornerRadius: 2).fill(color(level)).frame(width: 10, height: 10) }
            Text("多").foregroundStyle(.secondary)
        }.font(.system(size: 9))
    }
    private func activityCell(_ day: UsageDaily, width: CGFloat, height: CGFloat) -> some View {
        let level = usageActivityLevel(day.total, maximum: maximum)
        let highlighted = selectedDay?.date == day.date
        return Button {
            pinnedDate = pinnedDate == day.date ? nil : day.date
        } label: {
            RoundedRectangle(cornerRadius: min(3, height / 4)).fill(color(level))
                .overlay(RoundedRectangle(cornerRadius: min(3, height / 4))
                    .strokeBorder(highlighted ? Color.accentColor : Color.primary.opacity(0.045), lineWidth: highlighted ? 1.5 : 0.5))
                .overlay {
                    if period == .month && width >= 42 {
                        HStack(spacing: 3) {
                            Text("\(Calendar.current.component(.month, from: day.date))/\(Calendar.current.component(.day, from: day.date))").font(.system(size: 8))
                            Spacer(minLength: 0)
                            Text(usageCount(day.total)).font(.system(size: 9, weight: .medium)).monospacedDigit()
                        }.lineLimit(1).minimumScaleFactor(0.7).padding(.horizontal, 5)
                            .foregroundStyle(cellTextColor(level)).allowsHitTesting(false)
                    }
                }
                .frame(width: width, height: height).contentShape(Rectangle())
        }.buttonStyle(.plain)
            .onHover { inside in
                if inside { hoveredDate = day.date }
                else if hoveredDate == day.date { hoveredDate = nil }
            }
            .accessibilityLabel(activityHelp(day))
            .accessibilityAddTraits(pinnedDate == day.date ? .isSelected : [])
    }
    private var activityDetails: some View {
        HStack(spacing: 8) {
            if let day = selectedDay {
                VStack(alignment: .leading, spacing: 3) {
                    HStack(spacing: 8) {
                        Text(day.date.formatted(.dateTime.year().month().day())).fontWeight(.medium)
                        Text(usageCount(day.requests) + " 次请求").foregroundStyle(.secondary)
                    }
                    HStack(spacing: 12) {
                        Text(usageCount(day.total, estimated: day.estimated) + " Token")
                        Text("参考费用 " + activityCost(day)).foregroundStyle(.secondary)
                    }.monospacedDigit()
                }
                Spacer(minLength: 0)
                if pinnedDate != nil {
                    Button { pinnedDate = nil; hoveredDate = nil } label: { Image(systemName: "xmark.circle.fill") }
                        .buttonStyle(.plain).foregroundStyle(.secondary).help("取消固定")
                }
            } else {
                Text("悬停查看每日用量，点击固定显示").foregroundStyle(.tertiary)
                Spacer(minLength: 0)
            }
        }.font(.system(size: 10)).frame(height: 32, alignment: .leading)
    }
    private func activityCost(_ day: UsageDaily) -> String {
        let cost = day.cost ?? (day.total == 0 && day.requests == 0 ? 0 : nil)
        return cost.map { usageMoney($0) } ?? "未计价"
    }
    private func activityHelp(_ day: UsageDaily) -> String {
        day.date.formatted(.dateTime.year().month().day()) + "\n" + usageCount(day.total, estimated: day.estimated) + " Token · " + usageCount(day.requests) + " 次请求\n参考费用：" + activityCost(day)
    }
    private func cellTextColor(_ level: Int) -> Color {
        if colorScheme == .dark { return level >= 3 ? Color.black.opacity(0.82) : .white.opacity(0.9) }
        return level >= 3 ? .white : Color.black.opacity(0.8)
    }
    private func color(_ level: Int) -> Color {
        let light: [UInt32] = [0xEBEDF0, 0xA9DFB2, 0x6EC481, 0x38A35A, 0x177B3A]
        let dark: [UInt32] = [0x2B3036, 0x225B35, 0x287A42, 0x39A456, 0x66CF7F]
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
