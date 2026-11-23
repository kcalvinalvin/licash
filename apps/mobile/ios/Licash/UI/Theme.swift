import SwiftUI
import UIKit

private let Ink: UInt32 = 0xFF0F1214
private let Slate: UInt32 = 0xFF181D20
private let Lichen: UInt32 = 0xFFA8E06C
private let Moss: UInt32 = 0xFF6FB7A8
private let Mist: UInt32 = 0xFFE7ECE9
private let Dim: UInt32 = 0xFF8A9590

private let dark = darkColorScheme(
    primary: Lichen,
    onPrimary: Ink,
    secondary: Moss,
    onSecondary: Ink,
    background: Ink,
    onBackground: Mist,
    surface: Ink,
    onSurface: Mist,
    surfaceVariant: Slate,
    onSurfaceVariant: Dim,
    primaryContainer: 0xFF2C3A22,
    onPrimaryContainer: Lichen,
    // A second button, of the same green as the first, quieter.
    secondaryContainer: 0xFF2C3A22,
    onSecondaryContainer: Lichen,
    tertiary: Moss,
    surfaceContainerLowest: Ink,
    surfaceContainerLow: 0xFF14181B,
    surfaceContainer: Slate,
    surfaceContainerHigh: 0xFF20262A,
    surfaceContainerHighest: 0xFF283033,
    outline: 0xFF39423F,
    outlineVariant: 0xFF2A3230,
    error: 0xFFFF8A80
)

private let light = lightColorScheme(
    primary: 0xFF3F7D20,
    onPrimary: 0xFFFFFFFF,
    secondary: 0xFF2F7F70,
    background: 0xFFF6F4EE,
    onBackground: 0xFF151916,
    surface: 0xFFF6F4EE,
    onSurface: 0xFF151916,
    surfaceVariant: 0xFFEAE7DF,
    onSurfaceVariant: 0xFF5E665F,
    primaryContainer: 0xFFD9EDC4,
    onPrimaryContainer: 0xFF1E3D0E,
    secondaryContainer: 0xFFDDEDCC,
    onSecondaryContainer: 0xFF2E5E17,
    tertiary: 0xFF2F7F70,
    surfaceContainerLowest: 0xFFFFFFFF,
    surfaceContainerLow: 0xFFF1EEE7,
    surfaceContainer: 0xFFEDEAE2,
    surfaceContainerHigh: 0xFFE4E1D8,
    surfaceContainerHighest: 0xFFDDDAD0,
    outline: 0xFFC9C5BA,
    outlineVariant: 0xFFDDD9CE
)

/// Big figures with digits of one width, so that amounts do not jump.
let Figures = TextStyle(size: 56, lineHeight: 60, weight: .semibold, tracking: -1, tabular: true)

/// The amount a screen is about, under its title: smaller than Home's.
let Heading = TextStyle(size: 36, lineHeight: 44, weight: .semibold, tracking: -0.5, tabular: true)

/// An amount or a value in a row: digits of one width, to line up down a list.
let Amounts = TextStyle(size: 16, lineHeight: 24, tracking: 0.5, tabular: true)

/// The colors and type of the app: Material 3's roles, in the app's colors.
enum Theme {
    static let color = Palette(light: light, dark: dark)

    static let type: Typography = {
        var typography = Typography()
        typography.titleLarge.weight = .semibold
        typography.labelLarge.weight = .semibold
        typography.labelLarge.size = 16
        return typography
    }()
}

/// The app: its background, its text in the surface's color, its controls in the primary.
struct LicashTheme<Content: View>: View {
    let content: Content

    init(@ViewBuilder content: () -> Content) {
        self.content = content()
    }

    var body: some View {
        content
            .foregroundStyle(Theme.color.onSurface)
            .tint(Theme.color.primary)
            .background(Theme.color.background.ignoresSafeArea())
    }
}

/// A style of text: its size and line height in points, its weight, its
/// tracking, and whether its digits are of one width (`tnum`).
struct TextStyle {
    var size: CGFloat
    var lineHeight: CGFloat
    var weight: Font.Weight = .regular
    var tracking: CGFloat = 0
    var tabular: Bool = false
}

extension View {
    /// Text in `style`, scaled with the reader's text size.
    func textStyle(_ style: TextStyle) -> some View {
        modifier(Styled(style: style))
    }
}

private struct Styled: ViewModifier {
    let style: TextStyle
    @ScaledMetric private var size: CGFloat
    @ScaledMetric private var lineHeight: CGFloat

    init(style: TextStyle) {
        self.style = style
        _size = ScaledMetric(wrappedValue: style.size, relativeTo: .body)
        _lineHeight = ScaledMetric(wrappedValue: style.lineHeight, relativeTo: .body)
    }

    func body(content: Content) -> some View {
        let font = Font.system(size: size, weight: style.weight)
        // Each line as tall as the line height, half of what it adds above
        // the text and half below, or what it takes away.
        let extra = lineHeight - UIFont.systemFont(ofSize: size, weight: uiWeight(weight: style.weight)).lineHeight
        content
            .font(style.tabular ? font.monospacedDigit() : font)
            .tracking(style.tracking)
            .lineSpacing(max(extra, 0))
            .padding(.vertical, extra / 2)
    }
}

private func uiWeight(weight: Font.Weight) -> UIFont.Weight {
    switch weight {
    case .medium: .medium
    case .semibold: .semibold
    case .bold: .bold
    default: .regular
    }
}

/// Material 3's type roles, at Material 3's sizes.
struct Typography {
    var displayLarge = TextStyle(size: 57, lineHeight: 64, tracking: -0.2)
    var displayMedium = TextStyle(size: 45, lineHeight: 52)
    var displaySmall = TextStyle(size: 36, lineHeight: 44)
    var headlineLarge = TextStyle(size: 32, lineHeight: 40)
    var headlineMedium = TextStyle(size: 28, lineHeight: 36)
    var headlineSmall = TextStyle(size: 24, lineHeight: 32)
    var titleLarge = TextStyle(size: 22, lineHeight: 28)
    var titleMedium = TextStyle(size: 16, lineHeight: 24, weight: .medium, tracking: 0.2)
    var titleSmall = TextStyle(size: 14, lineHeight: 20, weight: .medium, tracking: 0.1)
    var bodyLarge = TextStyle(size: 16, lineHeight: 24, tracking: 0.5)
    var bodyMedium = TextStyle(size: 14, lineHeight: 20, tracking: 0.2)
    var bodySmall = TextStyle(size: 12, lineHeight: 16, tracking: 0.4)
    var labelLarge = TextStyle(size: 14, lineHeight: 20, weight: .medium, tracking: 0.1)
    var labelMedium = TextStyle(size: 12, lineHeight: 16, weight: .medium, tracking: 0.5)
    var labelSmall = TextStyle(size: 11, lineHeight: 16, weight: .medium, tracking: 0.5)
}

/// Material 3's color roles, each following light and dark mode.
struct Palette {
    let primary: Color
    let onPrimary: Color
    let primaryContainer: Color
    let onPrimaryContainer: Color
    let secondary: Color
    let onSecondary: Color
    let secondaryContainer: Color
    let onSecondaryContainer: Color
    let tertiary: Color
    let onTertiary: Color
    let tertiaryContainer: Color
    let onTertiaryContainer: Color
    let error: Color
    let onError: Color
    let errorContainer: Color
    let onErrorContainer: Color
    let background: Color
    let onBackground: Color
    let surface: Color
    let onSurface: Color
    let surfaceVariant: Color
    let onSurfaceVariant: Color
    let surfaceContainerLowest: Color
    let surfaceContainerLow: Color
    let surfaceContainer: Color
    let surfaceContainerHigh: Color
    let surfaceContainerHighest: Color
    let outline: Color
    let outlineVariant: Color
    let inverseSurface: Color
    let inverseOnSurface: Color
    let inversePrimary: Color
    let scrim: Color

    fileprivate init(light: Scheme, dark: Scheme) {
        func role(key: KeyPath<Scheme, UInt32>) -> Color {
            let (day, night) = (uiColor(argb: light[keyPath: key]), uiColor(argb: dark[keyPath: key]))
            return Color(uiColor: UIColor { $0.userInterfaceStyle == .dark ? night : day })
        }
        primary = role(key: \.primary)
        onPrimary = role(key: \.onPrimary)
        primaryContainer = role(key: \.primaryContainer)
        onPrimaryContainer = role(key: \.onPrimaryContainer)
        secondary = role(key: \.secondary)
        onSecondary = role(key: \.onSecondary)
        secondaryContainer = role(key: \.secondaryContainer)
        onSecondaryContainer = role(key: \.onSecondaryContainer)
        tertiary = role(key: \.tertiary)
        onTertiary = role(key: \.onTertiary)
        tertiaryContainer = role(key: \.tertiaryContainer)
        onTertiaryContainer = role(key: \.onTertiaryContainer)
        error = role(key: \.error)
        onError = role(key: \.onError)
        errorContainer = role(key: \.errorContainer)
        onErrorContainer = role(key: \.onErrorContainer)
        background = role(key: \.background)
        onBackground = role(key: \.onBackground)
        surface = role(key: \.surface)
        onSurface = role(key: \.onSurface)
        surfaceVariant = role(key: \.surfaceVariant)
        onSurfaceVariant = role(key: \.onSurfaceVariant)
        surfaceContainerLowest = role(key: \.surfaceContainerLowest)
        surfaceContainerLow = role(key: \.surfaceContainerLow)
        surfaceContainer = role(key: \.surfaceContainer)
        surfaceContainerHigh = role(key: \.surfaceContainerHigh)
        surfaceContainerHighest = role(key: \.surfaceContainerHighest)
        outline = role(key: \.outline)
        outlineVariant = role(key: \.outlineVariant)
        inverseSurface = role(key: \.inverseSurface)
        inverseOnSurface = role(key: \.inverseOnSurface)
        inversePrimary = role(key: \.inversePrimary)
        scrim = role(key: \.scrim)
    }
}

/// A color written in hex: alpha, red, green, blue.
private func uiColor(argb: UInt32) -> UIColor {
    UIColor(
        red: CGFloat((argb >> 16) & 0xFF) / 255,
        green: CGFloat((argb >> 8) & 0xFF) / 255,
        blue: CGFloat(argb & 0xFF) / 255,
        alpha: CGFloat(argb >> 24) / 255
    )
}

/// One mode's colors, by role.
private struct Scheme {
    var primary: UInt32
    var onPrimary: UInt32
    var secondary: UInt32
    var onSecondary: UInt32
    var background: UInt32
    var onBackground: UInt32
    var surface: UInt32
    var onSurface: UInt32
    var surfaceVariant: UInt32
    var onSurfaceVariant: UInt32
    var primaryContainer: UInt32
    var onPrimaryContainer: UInt32
    var secondaryContainer: UInt32
    var onSecondaryContainer: UInt32
    var tertiary: UInt32
    var surfaceContainerLowest: UInt32
    var surfaceContainerLow: UInt32
    var surfaceContainer: UInt32
    var surfaceContainerHigh: UInt32
    var surfaceContainerHighest: UInt32
    var outline: UInt32
    var outlineVariant: UInt32
    var error: UInt32
    var onTertiary: UInt32
    var tertiaryContainer: UInt32
    var onTertiaryContainer: UInt32
    var onError: UInt32
    var errorContainer: UInt32
    var onErrorContainer: UInt32
    var inverseSurface: UInt32
    var inverseOnSurface: UInt32
    var inversePrimary: UInt32
    var scrim: UInt32
}

/// The dark colors, Material 3's baseline for each role not given.
private func darkColorScheme(
    primary: UInt32 = 0xFFD0BCFF,
    onPrimary: UInt32 = 0xFF381E72,
    secondary: UInt32 = 0xFFCCC2DC,
    onSecondary: UInt32 = 0xFF332D41,
    background: UInt32 = 0xFF141218,
    onBackground: UInt32 = 0xFFE6E0E9,
    surface: UInt32 = 0xFF141218,
    onSurface: UInt32 = 0xFFE6E0E9,
    surfaceVariant: UInt32 = 0xFF49454F,
    onSurfaceVariant: UInt32 = 0xFFCAC4D0,
    primaryContainer: UInt32 = 0xFF4F378B,
    onPrimaryContainer: UInt32 = 0xFFEADDFF,
    secondaryContainer: UInt32 = 0xFF4A4458,
    onSecondaryContainer: UInt32 = 0xFFE8DEF8,
    tertiary: UInt32 = 0xFFEFB8C8,
    surfaceContainerLowest: UInt32 = 0xFF0F0D13,
    surfaceContainerLow: UInt32 = 0xFF1D1B20,
    surfaceContainer: UInt32 = 0xFF211F26,
    surfaceContainerHigh: UInt32 = 0xFF2B2930,
    surfaceContainerHighest: UInt32 = 0xFF36343B,
    outline: UInt32 = 0xFF938F99,
    outlineVariant: UInt32 = 0xFF49454F,
    error: UInt32 = 0xFFF2B8B5,
    onTertiary: UInt32 = 0xFF492532,
    tertiaryContainer: UInt32 = 0xFF633B48,
    onTertiaryContainer: UInt32 = 0xFFFFD8E4,
    onError: UInt32 = 0xFF601410,
    errorContainer: UInt32 = 0xFF8C1D18,
    onErrorContainer: UInt32 = 0xFFF9DEDC,
    inverseSurface: UInt32 = 0xFFE6E0E9,
    inverseOnSurface: UInt32 = 0xFF322F35,
    inversePrimary: UInt32 = 0xFF6750A4,
    scrim: UInt32 = 0xFF000000
) -> Scheme {
    Scheme(
        primary: primary, onPrimary: onPrimary, secondary: secondary, onSecondary: onSecondary,
        background: background, onBackground: onBackground, surface: surface, onSurface: onSurface,
        surfaceVariant: surfaceVariant, onSurfaceVariant: onSurfaceVariant,
        primaryContainer: primaryContainer, onPrimaryContainer: onPrimaryContainer,
        secondaryContainer: secondaryContainer, onSecondaryContainer: onSecondaryContainer, tertiary: tertiary,
        surfaceContainerLowest: surfaceContainerLowest, surfaceContainerLow: surfaceContainerLow,
        surfaceContainer: surfaceContainer, surfaceContainerHigh: surfaceContainerHigh,
        surfaceContainerHighest: surfaceContainerHighest, outline: outline, outlineVariant: outlineVariant,
        error: error, onTertiary: onTertiary, tertiaryContainer: tertiaryContainer,
        onTertiaryContainer: onTertiaryContainer, onError: onError, errorContainer: errorContainer,
        onErrorContainer: onErrorContainer, inverseSurface: inverseSurface, inverseOnSurface: inverseOnSurface,
        inversePrimary: inversePrimary, scrim: scrim
    )
}

/// The light colors, Material 3's baseline for each role not given.
private func lightColorScheme(
    primary: UInt32 = 0xFF6750A4,
    onPrimary: UInt32 = 0xFFFFFFFF,
    secondary: UInt32 = 0xFF625B71,
    onSecondary: UInt32 = 0xFFFFFFFF,
    background: UInt32 = 0xFFFEF7FF,
    onBackground: UInt32 = 0xFF1D1B20,
    surface: UInt32 = 0xFFFEF7FF,
    onSurface: UInt32 = 0xFF1D1B20,
    surfaceVariant: UInt32 = 0xFFE7E0EC,
    onSurfaceVariant: UInt32 = 0xFF49454F,
    primaryContainer: UInt32 = 0xFFEADDFF,
    onPrimaryContainer: UInt32 = 0xFF21005D,
    secondaryContainer: UInt32 = 0xFFE8DEF8,
    onSecondaryContainer: UInt32 = 0xFF1D192B,
    tertiary: UInt32 = 0xFF7D5260,
    surfaceContainerLowest: UInt32 = 0xFFFFFFFF,
    surfaceContainerLow: UInt32 = 0xFFF7F2FA,
    surfaceContainer: UInt32 = 0xFFF3EDF7,
    surfaceContainerHigh: UInt32 = 0xFFECE6F0,
    surfaceContainerHighest: UInt32 = 0xFFE6E0E9,
    outline: UInt32 = 0xFF79747E,
    outlineVariant: UInt32 = 0xFFCAC4D0,
    error: UInt32 = 0xFFB3261E,
    onTertiary: UInt32 = 0xFFFFFFFF,
    tertiaryContainer: UInt32 = 0xFFFFD8E4,
    onTertiaryContainer: UInt32 = 0xFF31111D,
    onError: UInt32 = 0xFFFFFFFF,
    errorContainer: UInt32 = 0xFFF9DEDC,
    onErrorContainer: UInt32 = 0xFF410E0B,
    inverseSurface: UInt32 = 0xFF322F35,
    inverseOnSurface: UInt32 = 0xFFF5EFF7,
    inversePrimary: UInt32 = 0xFFD0BCFF,
    scrim: UInt32 = 0xFF000000
) -> Scheme {
    Scheme(
        primary: primary, onPrimary: onPrimary, secondary: secondary, onSecondary: onSecondary,
        background: background, onBackground: onBackground, surface: surface, onSurface: onSurface,
        surfaceVariant: surfaceVariant, onSurfaceVariant: onSurfaceVariant,
        primaryContainer: primaryContainer, onPrimaryContainer: onPrimaryContainer,
        secondaryContainer: secondaryContainer, onSecondaryContainer: onSecondaryContainer, tertiary: tertiary,
        surfaceContainerLowest: surfaceContainerLowest, surfaceContainerLow: surfaceContainerLow,
        surfaceContainer: surfaceContainer, surfaceContainerHigh: surfaceContainerHigh,
        surfaceContainerHighest: surfaceContainerHighest, outline: outline, outlineVariant: outlineVariant,
        error: error, onTertiary: onTertiary, tertiaryContainer: tertiaryContainer,
        onTertiaryContainer: onTertiaryContainer, onError: onError, errorContainer: errorContainer,
        onErrorContainer: onErrorContainer, inverseSurface: inverseSurface, inverseOnSurface: inverseOnSurface,
        inversePrimary: inversePrimary, scrim: scrim
    )
}
