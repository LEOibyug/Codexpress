import AppKit
import SwiftUI

enum AppPalette {
    static let canvas = Color(nsColor: .windowBackgroundColor)
    static let card = Color(nsColor: .controlBackgroundColor)
    static let well = Color(nsColor: .quaternaryLabelColor).opacity(0.35)
    static let border = Color(nsColor: .separatorColor).opacity(0.55)
    static let accountHealthy = adaptive(light: 0x228044, dark: 0x65D98A)
    static let accountCaution = adaptive(light: 0xA86B00, dark: 0xF6C659)
    static let accountLow = adaptive(light: 0xCE3434, dark: 0xFF7777)

    private static func adaptive(light: UInt32, dark: UInt32) -> Color {
        Color(nsColor: NSColor(name: nil) { appearance in
            let hex = appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? dark : light
            return NSColor(srgbRed: CGFloat((hex >> 16) & 255) / 255,
                           green: CGFloat((hex >> 8) & 255) / 255,
                           blue: CGFloat(hex & 255) / 255, alpha: 1)
        })
    }
}

enum AppTypography {
    static let pageTitle = Font.system(size: 22, weight: .semibold)
    static let sectionTitle = Font.system(size: 14, weight: .semibold)
    static let body = Font.system(size: 13)
    static let caption = Font.system(size: 12)
    static let small = Font.system(size: 11)
}

enum AppMetrics {
    static let pageInset: CGFloat = 28
    static let sectionSpacing: CGFloat = 20
    static let cornerRadius: CGFloat = 10
}

struct PageHeading<Actions: View>: View {
    let title: String
    let subtitle: String?
    @ViewBuilder var actions: Actions

    init(_ title: String, subtitle: String? = nil, @ViewBuilder actions: () -> Actions) {
        self.title = title; self.subtitle = subtitle; self.actions = actions()
    }

    var body: some View {
        HStack(alignment: .center, spacing: 16) {
            VStack(alignment: .leading, spacing: 5) {
                Text(title).font(AppTypography.pageTitle)
                if let subtitle {
                    Text(subtitle).font(.system(size: 12)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                }
            }
            Spacer(minLength: 0)
            actions.fixedSize(horizontal: true, vertical: false)
        }.frame(minHeight: 34).padding(.bottom, 4)
    }
}

extension PageHeading where Actions == EmptyView {
    init(_ title: String, subtitle: String? = nil) { self.init(title, subtitle: subtitle) { EmptyView() } }
}

struct CardSurface: ViewModifier {
    func body(content: Content) -> some View {
        content
            .background(AppPalette.card, in: RoundedRectangle(cornerRadius: AppMetrics.cornerRadius, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: AppMetrics.cornerRadius, style: .continuous).strokeBorder(AppPalette.border, lineWidth: 0.5))
    }
}

extension View {
    func cardSurface() -> some View { modifier(CardSurface()) }
    func settingsField() -> some View {
        textFieldStyle(.roundedBorder).controlSize(.regular)
    }
    func settingsPage() -> some View {
        // Split-view columns are also measured at zero width for AppKit's minimum
        // size. Keep a readable minimum so wrapped text cannot create a huge height.
        padding(.horizontal, AppMetrics.pageInset).padding(.vertical, 24)
            .frame(minWidth: 700, maxWidth: 1120)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
    }
}

struct SettingsCard<Accessory: View, Content: View>: View {
    let title: String
    let subtitle: String?
    let symbol: String
    let accessory: Accessory
    let content: Content

    init(_ title: String, subtitle: String? = nil, symbol: String, @ViewBuilder accessory: () -> Accessory, @ViewBuilder content: () -> Content) {
        self.title = title; self.subtitle = subtitle; self.symbol = symbol
        self.accessory = accessory(); self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(alignment: .firstTextBaseline, spacing: 9) {
                Image(systemName: symbol).font(.system(size: 13, weight: .medium))
                    .foregroundStyle(.secondary).frame(width: 18)
                VStack(alignment: .leading, spacing: 4) {
                    Text(title).font(AppTypography.sectionTitle)
                    if let subtitle {
                        Text(subtitle).font(AppTypography.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                    }
                }
                Spacer(minLength: 8)
                accessory
            }
            content
        }.padding(20).frame(maxWidth: .infinity, alignment: .leading).cardSurface()
    }
}

extension SettingsCard where Accessory == EmptyView {
    init(_ title: String, subtitle: String? = nil, symbol: String, @ViewBuilder content: () -> Content) {
        self.init(title, subtitle: subtitle, symbol: symbol, accessory: { EmptyView() }, content: content)
    }
}

struct SettingRow<Control: View>: View {
    let title: String
    var subtitle: String? = nil
    @ViewBuilder var control: Control

    var body: some View {
        HStack(alignment: .center, spacing: 24) {
            VStack(alignment: .leading, spacing: 4) {
                Text(title).font(AppTypography.body)
                if let subtitle { Text(subtitle).font(AppTypography.caption).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true) }
            }
            Spacer(minLength: 8)
            control
        }.frame(minHeight: 32)
    }
}

struct StatusPill: View {
    let text: String
    var color: Color = .secondary
    var active = false
    var compact = false

    var body: some View {
        HStack(spacing: 5) {
            ActivityDot(color: color, active: active)
            Text(text).font(.system(size: compact ? 10 : 11, weight: .medium))
        }.foregroundStyle(color).padding(.horizontal, compact ? 6 : 8).padding(.vertical, compact ? 3 : 4)
            .background(color.opacity(0.08), in: RoundedRectangle(cornerRadius: 5))
            .accessibilityElement(children: .combine)
    }
}

struct InfoNote: View {
    let text: String
    var symbol = "info.circle"
    var color: Color = .secondary

    var body: some View {
        HStack(alignment: .top, spacing: 7) {
            Image(systemName: symbol).font(.system(size: 12)).padding(.top, 1)
            Text(text).font(AppTypography.caption).fixedSize(horizontal: false, vertical: true)
        }.foregroundStyle(color)
    }
}

struct EmptyState<Actions: View>: View {
    let title: String
    let subtitle: String
    let symbol: String
    let actions: Actions

    init(_ title: String, subtitle: String, symbol: String, @ViewBuilder actions: () -> Actions) {
        self.title = title; self.subtitle = subtitle; self.symbol = symbol; self.actions = actions()
    }

    var body: some View {
        // Native sizing keeps navigation columns stable when the actions are empty.
        ContentUnavailableView {
            VStack(spacing: 14) {
                Image(systemName: symbol).font(.system(size: 32, weight: .light))
                    .foregroundStyle(.secondary).frame(width: 58, height: 58)
                Text(title).font(.system(size: 17, weight: .semibold))
            }
        } description: {
            Text(subtitle).font(AppTypography.body).foregroundStyle(.secondary)
                .multilineTextAlignment(.center).lineSpacing(3).frame(maxWidth: 310)
        } actions: {
            actions.padding(.top, 5)
        }.frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

extension EmptyState where Actions == EmptyView {
    init(_ title: String, subtitle: String, symbol: String) { self.init(title, subtitle: subtitle, symbol: symbol) { EmptyView() } }
}
