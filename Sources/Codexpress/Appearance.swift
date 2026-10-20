import AppKit
import SwiftUI

enum AppPalette {
    static let canvas = adaptive(light: 0xF5F6F8, dark: 0x1C1D21)
    static let card = adaptive(light: 0xFFFFFF, dark: 0x27282D)
    static let well = adaptive(light: 0xF4F5F7, dark: 0x202126)
    static let border = Color.primary.opacity(0.075)

    private static func adaptive(light: UInt32, dark: UInt32) -> Color {
        Color(nsColor: NSColor(name: nil) { appearance in
            let hex = appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? dark : light
            return NSColor(srgbRed: CGFloat((hex >> 16) & 255) / 255,
                           green: CGFloat((hex >> 8) & 255) / 255,
                           blue: CGFloat(hex & 255) / 255, alpha: 1)
        })
    }
}

struct PageHeading<Actions: View>: View {
    let title: String
    let subtitle: String
    @ViewBuilder var actions: Actions

    init(_ title: String, subtitle: String, @ViewBuilder actions: () -> Actions) {
        self.title = title; self.subtitle = subtitle; self.actions = actions()
    }

    var body: some View {
        HStack(alignment: .center, spacing: 20) {
            VStack(alignment: .leading, spacing: 7) {
                Text(title).font(.system(size: 25, weight: .bold))
                Text(subtitle).font(.system(size: 12)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 0)
            actions.fixedSize(horizontal: true, vertical: false)
        }.padding(.bottom, 6)
    }
}

extension PageHeading where Actions == EmptyView {
    init(_ title: String, subtitle: String) { self.init(title, subtitle: subtitle) { EmptyView() } }
}

struct CardSurface: ViewModifier {
    func body(content: Content) -> some View {
        content
            .background(AppPalette.card, in: RoundedRectangle(cornerRadius: 14, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: 14, style: .continuous).strokeBorder(AppPalette.border))
    }
}

extension View {
    func cardSurface() -> some View { modifier(CardSurface()) }
    func settingsField() -> some View {
        textFieldStyle(.plain)
            .padding(.horizontal, 10).padding(.vertical, 7)
            .background(AppPalette.well, in: RoundedRectangle(cornerRadius: 7))
            .overlay(RoundedRectangle(cornerRadius: 7).strokeBorder(AppPalette.border))
    }
    func settingsPage() -> some View {
        // Split-view columns are also measured at zero width for AppKit's minimum
        // size. Keep a readable minimum so wrapped text cannot create a huge height.
        padding(28).frame(minWidth: 600, maxWidth: 820).frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
    }
}

struct SettingsCard<Accessory: View, Content: View>: View {
    let title: String
    let subtitle: String
    let symbol: String
    let accessory: Accessory
    let content: Content

    init(_ title: String, subtitle: String, symbol: String, @ViewBuilder accessory: () -> Accessory, @ViewBuilder content: () -> Content) {
        self.title = title; self.subtitle = subtitle; self.symbol = symbol
        self.accessory = accessory(); self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            HStack(spacing: 11) {
                Image(systemName: symbol).font(.system(size: 15, weight: .medium))
                    .foregroundStyle(Color.accentColor).frame(width: 34, height: 34)
                    .background(Color.accentColor.opacity(0.10), in: RoundedRectangle(cornerRadius: 9))
                VStack(alignment: .leading, spacing: 4) {
                    Text(title).font(.system(size: 13, weight: .semibold))
                    Text(subtitle).font(.system(size: 11)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                }
                Spacer(minLength: 8)
                accessory
            }
            content
        }.padding(18).frame(maxWidth: .infinity, alignment: .leading).cardSurface()
    }
}

extension SettingsCard where Accessory == EmptyView {
    init(_ title: String, subtitle: String, symbol: String, @ViewBuilder content: () -> Content) {
        self.init(title, subtitle: subtitle, symbol: symbol, accessory: { EmptyView() }, content: content)
    }
}

struct SettingRow<Control: View>: View {
    let title: String
    var subtitle: String? = nil
    @ViewBuilder var control: Control

    var body: some View {
        HStack(spacing: 24) {
            VStack(alignment: .leading, spacing: 4) {
                Text(title).font(.system(size: 12, weight: .medium))
                if let subtitle { Text(subtitle).font(.system(size: 11)).foregroundStyle(.secondary) }
            }
            Spacer(minLength: 8)
            control
        }.frame(minHeight: 28)
    }
}

struct StatusPill: View {
    let text: String
    var color: Color = .secondary

    var body: some View {
        HStack(spacing: 5) {
            Circle().fill(color).frame(width: 5, height: 5)
            Text(text).font(.system(size: 10, weight: .medium))
        }.foregroundStyle(color).padding(.horizontal, 8).padding(.vertical, 5)
            .background(color.opacity(0.09), in: Capsule())
            .accessibilityElement(children: .combine)
    }
}

struct InfoNote: View {
    let text: String
    var symbol = "info.circle"
    var color: Color = .secondary

    var body: some View {
        HStack(alignment: .top, spacing: 7) {
            Image(systemName: symbol).font(.system(size: 11)).padding(.top, 1)
            Text(text).font(.system(size: 11)).fixedSize(horizontal: false, vertical: true)
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
            VStack(spacing: 18) {
                Image(systemName: symbol).font(.system(size: 29, weight: .light))
                    .foregroundStyle(Color.accentColor).frame(width: 70, height: 70)
                    .background(Color.accentColor.opacity(0.08), in: RoundedRectangle(cornerRadius: 20))
                Text(title).font(.system(size: 17, weight: .semibold))
            }
        } description: {
            Text(subtitle).font(.system(size: 12)).foregroundStyle(.secondary)
                .multilineTextAlignment(.center).lineSpacing(3).frame(maxWidth: 310)
        } actions: {
            actions.padding(.top, 5)
        }.frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

extension EmptyState where Actions == EmptyView {
    init(_ title: String, subtitle: String, symbol: String) { self.init(title, subtitle: subtitle, symbol: symbol) { EmptyView() } }
}
