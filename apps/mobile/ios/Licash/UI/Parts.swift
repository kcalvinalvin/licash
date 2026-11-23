import LicashCore
import SwiftUI
import UIKit

/// An icon: an SF Symbol or one of [Glyphs], tinted with the foreground style.
typealias ImageVector = Image

/// The name, as a mark.
struct Wordmark: View {
    let size: Int

    init(size: Int = 22) {
        self.size = size
    }

    var body: some View {
        Text("licash")
            .textStyle({
                var style = Theme.type.bodyLarge
                style.size = CGFloat(size)
                style.weight = .bold
                style.tracking = -0.5
                return style
            }())
            .foregroundStyle(Theme.color.primary)
    }
}

extension EnvironmentValues {
    /// The seconds a block takes on the wallet's network, for screens to tell a
    /// count of blocks in: set once at the root, Bitcoin's until then.
    var blockSeconds: UInt32 {
        get { self[BlockSeconds.self] }
        set { self[BlockSeconds.self] = newValue }
    }
}

private struct BlockSeconds: EnvironmentKey {
    static let defaultValue = BITCOIN_BLOCK_SECONDS
}

/// A network's name as said to a user: "Bitcoin", "Mutinynet".
func named(network: Network) -> String {
    let name = String(describing: network).lowercased()
    return name.prefix(1).uppercased() + name.dropFirst()
}

/// The network, unless it is Bitcoin's own.
struct NetworkChip: View {
    let network: Network

    init(network: Network) {
        self.network = network
    }

    var body: some View {
        if network != .bitcoin {
            Text(String(describing: network).uppercased())
                .textStyle(Theme.type.labelSmall)
                .foregroundStyle(Theme.color.secondary)
                .padding(.horizontal, 10)
                .padding(.vertical, 3)
                .background(Theme.color.secondary.opacity(0.18), in: Capsule())
        }
    }
}

/// A screen's title and the way back. `end` holds what else it offers, if anything.
struct TopBar<End: View>: View {
    let title: String
    let back: () -> Void
    let end: End

    init(title: String, back: @escaping () -> Void, @ViewBuilder end: () -> End) {
        self.title = title
        self.back = back
        self.end = end()
    }

    var body: some View {
        HStack(alignment: .center, spacing: 0) {
            IconButton(onClick: back) {
                Image(systemName: "chevron.backward")
                    .font(.system(size: 20, weight: .medium))
                    .accessibilityLabel("Back")
            }
            Text(title)
                .textStyle(Theme.type.titleLarge)
                .lineLimit(1)
                .truncationMode(.tail)
                .frame(maxWidth: .infinity, alignment: .leading)
            end
        }
        .padding(.vertical, 8)
        .frame(maxWidth: .infinity)
    }
}

extension TopBar where End == EmptyView {
    init(title: String, back: @escaping () -> Void) {
        self.init(title: title, back: back) { EmptyView() }
    }
}

/// A titled card of rows.
struct Group<Content: View>: View {
    let title: String
    let content: Content

    init(title: String, @ViewBuilder content: () -> Content) {
        self.title = title
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Spacer().frame(height: 20)
            // Untitled on a page of its own, the page's title saying it.
            if !title.isEmpty {
                Text(title)
                    .textStyle(Theme.type.labelLarge)
                    .foregroundStyle(Theme.color.primary)
                    .padding(.leading, 4)
                    .padding(.bottom, 8)
            }
            VStack(alignment: .leading, spacing: 0) { content }
                .padding(.horizontal, 16)
                .padding(.vertical, 4)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Theme.color.surfaceContainer, in: RoundedRectangle(cornerRadius: 20, style: .continuous))
        }
    }
}

struct Divider: View {
    init() {}

    var body: some View {
        Rectangle().fill(Theme.color.outlineVariant).frame(height: 1)
    }
}

/// The one thing to do on a screen. `quiet`, another beside it. `busy`, its
/// work under way: a spinner before its label, which then says what is
/// going on, and no tap until it is done.
struct BigButton: View {
    let text: String
    let enabled: Bool
    let quiet: Bool
    let danger: Bool
    let busy: Bool
    let onClick: () -> Void

    init(
        text: String,
        enabled: Bool = true,
        quiet: Bool = false,
        danger: Bool = false,
        busy: Bool = false,
        onClick: @escaping () -> Void
    ) {
        self.text = text
        self.enabled = enabled
        self.quiet = quiet
        self.danger = danger
        self.busy = busy
        self.onClick = onClick
    }

    var body: some View {
        let colors = Theme.color
        let (container, content) =
            quiet ? (colors.secondaryContainer, colors.onSecondaryContainer)
            : danger ? (colors.error, colors.onError)
            : (colors.primary, colors.onPrimary)
        let on = enabled && !busy
        Button(action: onClick) {
            HStack(alignment: .center, spacing: 0) {
                if busy {
                    CircularProgressIndicator(size: 18, strokeWidth: 2)
                    Spacer().frame(width: 10)
                }
                Text(text).textStyle(Theme.type.labelLarge).lineLimit(1).truncationMode(.tail)
            }
        }
        .buttonStyle(Big(container: container, content: content))
        .disabled(!on)
        .accessibilityValue(busy ? "Working" : "")
    }
}

/// A big button's look: 56 tall, corners of 18, as wide as it is let be.
private struct Big: ButtonStyle {
    let container: Color
    let content: Color

    func makeBody(configuration: Configuration) -> some View {
        Shown(configuration: configuration, style: self)
    }

    private struct Shown: View {
        let configuration: ButtonStyleConfiguration
        let style: Big
        @Environment(\.isEnabled) private var enabled

        var body: some View {
            let colors = Theme.color
            let shape = RoundedRectangle(cornerRadius: 18, style: .continuous)
            let content = enabled ? style.content : colors.onSurfaceVariant.opacity(0.38)
            configuration.label
                .foregroundStyle(content)
                .tint(content)
                .padding(.horizontal, 24)
                .frame(maxWidth: .infinity)
                .frame(height: 56)
                .background(enabled ? style.container : colors.onSurface.opacity(0.1), in: shape)
                // Pressed: a layer of what is in it, faintly.
                .overlay { shape.fill(content.opacity(configuration.isPressed ? 0.1 : 0)) }
                .contentShape(shape)
        }
    }
}

/// A state in a glyph and a word or two, never by color alone: `busy` turns
/// the glyph into a spinner, and a tap, if it takes one, opens more. `tag`
/// names its words, for the tests to read.
struct Pill: View {
    let icon: ImageVector
    let text: String
    let tint: Color
    let busy: Bool
    let tag: String?
    let onClick: (() -> Void)?

    init(
        icon: ImageVector,
        text: String,
        tint: Color = Theme.color.onSurfaceVariant,
        busy: Bool = false,
        tag: String? = nil,
        onClick: (() -> Void)? = nil
    ) {
        self.icon = icon
        self.text = text
        self.tint = tint
        self.busy = busy
        self.tag = tag
        self.onClick = onClick
    }

    var body: some View {
        let content = HStack(alignment: .center, spacing: 0) {
            if busy {
                CircularProgressIndicator(size: 16, strokeWidth: 2).tint(tint)
            } else {
                icon.resizable().scaledToFit().frame(width: 18, height: 18).foregroundStyle(tint).accessibilityHidden(true)
            }
            Spacer().frame(width: 8)
            Text(text)
                .textStyle(Theme.type.labelLarge)
                .foregroundStyle(tint)
                .lineLimit(1)
                .truncationMode(.tail)
                .accessibilityIdentifier(tag ?? "")
            if onClick != nil { Chevron(tint: tint.opacity(0.6)) }
        }
        .padding(.leading, 12)
        .padding(.trailing, onClick != nil ? 6 : 14)
        .padding(.vertical, 8)
        .background(Theme.color.surfaceContainer, in: Capsule())
        if let onClick {
            Button(action: onClick) { content.contentShape(Capsule()) }.buttonStyle(.plain)
        } else {
            content
        }
    }
}

/// What says a row opens something.
struct Chevron: View {
    let tint: Color

    init(tint: Color = Theme.color.outline) {
        self.tint = tint
    }

    var body: some View {
        Image(systemName: "chevron.forward")
            .font(.system(size: 14, weight: .semibold))
            .foregroundStyle(tint)
            .frame(width: 20, height: 20)
            .accessibilityHidden(true)
    }
}

/// What a row is about, in a glyph on a tile of its color: on a list's
/// first page only, every row of it with one.
private struct Tile: View {
    let icon: ImageVector
    let tint: Color

    var body: some View {
        icon.resizable().scaledToFit().frame(width: 18, height: 18).foregroundStyle(tint)
            .frame(width: 32, height: 32)
            .background(tint.opacity(0.16), in: RoundedRectangle(cornerRadius: 9, style: .continuous))
            .accessibilityHidden(true)
            .padding(.trailing, 16)
    }
}

/// A row of a list: what it is and, at its end, its value. A glyph on a
/// tile before it and a line under it, if any. With something to open, a
/// chevron. `tag` names the value, for the tests to read, or the title of
/// one without a value. `color`, the title's, for a row that destroys.
struct Entry: View {
    let title: String
    let value: String?
    let sub: String?
    let icon: ImageVector?
    let tint: Color
    let tag: String?
    let color: Color?
    let onClick: (() -> Void)?

    init(
        title: String,
        value: String? = nil,
        sub: String? = nil,
        icon: ImageVector? = nil,
        tint: Color = Theme.color.primary,
        tag: String? = nil,
        color: Color? = nil,
        onClick: (() -> Void)? = nil
    ) {
        self.title = title
        self.value = value
        self.sub = sub
        self.icon = icon
        self.tint = tint
        self.tag = tag
        self.color = color
        self.onClick = onClick
    }

    var body: some View {
        let row = HStack(alignment: .center, spacing: 0) {
            if let icon { Tile(icon: icon, tint: tint) }
            VStack(alignment: .leading, spacing: 0) {
                Text(title)
                    .textStyle(Theme.type.bodyLarge)
                    .foregroundStyle(color ?? Theme.color.onSurface)
                    .lineLimit(1)
                    .truncationMode(.tail)
                    .accessibilityIdentifier(value == nil ? tag ?? "" : "")
                if let sub {
                    Text(sub)
                        .textStyle(Theme.type.bodySmall)
                        .foregroundStyle(Theme.color.onSurfaceVariant)
                        .lineLimit(1)
                        .truncationMode(.tail)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            if let value {
                Spacer().frame(width: 12)
                Text(value)
                    .textStyle(Amounts)
                    .foregroundStyle(Theme.color.onSurfaceVariant)
                    .multilineTextAlignment(.trailing)
                    .lineLimit(1)
                    .layoutPriority(1)
                    .accessibilityIdentifier(tag ?? "")
            }
            if onClick != nil {
                Spacer().frame(width: 4)
                Chevron()
            }
        }
        .padding(.vertical, 8)
        .frame(maxWidth: .infinity, minHeight: 56, alignment: .leading)
        .contentShape(Rectangle())
        if let onClick {
            Button(action: onClick) { row }.buttonStyle(.plain)
        } else {
            row
        }
    }
}

/// A fact: its name, and what it is at the end, then, quieter, its `share`
/// of what it is on (a fee's `0.24%`). `tag` names the value, for the
/// tests, and `tag-share` its share.
struct Line: View {
    let label: String
    let value: String
    let tag: String?
    let share: String?
    let onClick: (() -> Void)?

    init(label: String, value: String, tag: String? = nil, share: String? = nil, onClick: (() -> Void)? = nil) {
        self.label = label
        self.value = value
        self.tag = tag
        self.share = share
        self.onClick = onClick
    }

    var body: some View {
        // On one line of text, though the amount's type is not the label's.
        let row = HStack(alignment: .firstTextBaseline, spacing: 0) {
            Text(label).textStyle(Theme.type.bodyLarge).layoutPriority(1)
            Spacer().frame(width: 16)
            Text(value)
                .textStyle(Amounts)
                .foregroundStyle(Theme.color.onSurfaceVariant)
                .multilineTextAlignment(.trailing)
                .frame(maxWidth: .infinity, alignment: .trailing)
                .accessibilityIdentifier(tag ?? "")
            if let share {
                Text(" · \(share)")
                    .textStyle(Amounts)
                    .foregroundStyle(Theme.color.outline)
                    .layoutPriority(1)
                    .accessibilityIdentifier(tag.map { "\($0)-share" } ?? "")
            }
            if onClick != nil { Chevron() }
        }
        .padding(.vertical, 12)
        .frame(maxWidth: .infinity)
        .contentShape(Rectangle())
        if let onClick {
            Button(action: onClick) { row }.buttonStyle(.plain)
        } else {
            row
        }
    }
}

/// Something on or off, as a row: a glyph on a tile and a line under it,
/// if any, and what it does behind an (i). Its switch is tagged `toggle`,
/// then its title.
struct Toggle: View {
    let title: String
    let on: Bool
    let sub: String?
    let icon: ImageVector?
    let means: String?
    let set: (Bool) -> Void

    init(
        title: String,
        on: Bool,
        sub: String? = nil,
        icon: ImageVector? = nil,
        means: String? = nil,
        set: @escaping (Bool) -> Void
    ) {
        self.title = title
        self.on = on
        self.sub = sub
        self.icon = icon
        self.means = means
        self.set = set
    }

    var body: some View {
        let feel = rememberFeel()
        // The row and its switch both turn it: felt once, as it turns.
        let turn = { (to: Bool) in
            feel.toggle(on: to)
            set(to)
        }
        HStack(alignment: .center, spacing: 0) {
            if let icon { Tile(icon: icon, tint: Theme.color.primary) }
            VStack(alignment: .leading, spacing: 0) {
                HStack(alignment: .center, spacing: 0) {
                    Text(title).textStyle(Theme.type.bodyLarge)
                    if let means { Info(title: title, text: means) }
                }
                if let sub {
                    Text(sub).textStyle(Theme.type.bodySmall).foregroundStyle(Theme.color.onSurfaceVariant)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            Spacer().frame(width: 12)
            Switch(checked: on, onCheckedChange: turn).accessibilityIdentifier("toggle \(title)")
        }
        .padding(.vertical, 4)
        .frame(maxWidth: .infinity, minHeight: 56)
        // A tap on the row, outside its (i) and its switch, which take
        // their own: the switch is what VoiceOver turns.
        .contentShape(Rectangle())
        .onTapGesture { turn(!on) }
    }
}

/// What a tap asked for, from the bottom of the screen: a title and a
/// little under it. For what is read and left, or one choice made. What
/// is worked in gets a screen. Not `dismissable` while what it started is
/// under way. `secure`, hidden while the screen is recorded or mirrored and
/// from the app switcher's snapshot, as iOS cannot keep it out of a
/// screenshot. Its caller presents it with `.sheet`, whose binding, set
/// false when she swipes it down, does what `dismiss` does.
struct Sheet<Content: View>: View {
    let title: String
    let dismiss: () -> Void
    let dismissable: Bool
    let secure: Bool
    let content: Content
    @SwiftUI.State private var height: CGFloat = 0
    @SwiftUI.State private var captured = false
    @Environment(\.scenePhase) private var phase

    init(
        title: String,
        dismiss: @escaping () -> Void,
        dismissable: Bool = true,
        secure: Bool = false,
        @ViewBuilder content: () -> Content
    ) {
        self.title = title
        self.dismiss = dismiss
        self.dismissable = dismissable
        self.secure = secure
        self.content = content()
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                Text(title).textStyle(Theme.type.titleLarge)
                Spacer().frame(height: 12)
                content
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 24)
            // Below its handle, as Material's sheet sits its content.
            .padding(.top, 48)
            .padding(.bottom, 32)
            .background(GeometryReader { Color.clear.preference(key: SheetHeight.self, value: $0.size.height) })
            .opacity(secure && (captured || phase != .active) ? 0 : 1)
        }
        .scrollBounceBehavior(.basedOnSize)
        .onPreferenceChange(SheetHeight.self) { height = $0 }
        .foregroundStyle(Theme.color.onSurface)
        // All of it at once: no half-open sheet hiding its button.
        .presentationDetents(height > 0 ? [.height(height)] : [.large])
        .presentationDragIndicator(.visible)
        .presentationCornerRadius(28)
        .presentationBackground(Theme.color.surfaceContainerLow)
        .interactiveDismissDisabled(!dismissable)
        .task {
            captured = UIScreen.main.isCaptured
            for await _ in NotificationCenter.default.notifications(named: UIScreen.capturedDidChangeNotification) {
                captured = UIScreen.main.isCaptured
            }
        }
    }
}

/// How tall a sheet's content is, for the sheet to open to.
private struct SheetHeight: PreferenceKey {
    static let defaultValue: CGFloat = 0

    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        value = nextValue()
    }
}

/// A paragraph of a [Sheet]: what a word means, said once, where asked.
struct Say: View {
    let text: String

    init(text: String) {
        self.text = text
    }

    var body: some View {
        Text(text)
            .textStyle(Theme.type.bodyLarge)
            .foregroundStyle(Theme.color.onSurfaceVariant)
            .padding(.bottom, 12)
    }
}

/// An (i): what `title` means, in a sheet, once tapped.
struct Info: View {
    let title: String
    let text: String
    @SwiftUI.State private var shown = false

    init(title: String, text: String) {
        self.title = title
        self.text = text
    }

    var body: some View {
        IconButton(onClick: { shown = true }) {
            Image(systemName: "info.circle")
                .font(.system(size: 17))
                .foregroundStyle(Theme.color.onSurfaceVariant)
                .frame(width: 20, height: 20)
                .accessibilityLabel("About \(title)")
        }
        .sheet(isPresented: $shown) {
            Sheet(title: title, dismiss: { shown = false }) { Say(text: text) }
        }
    }
}

/// What few ask for, folded under a word until tapped.
struct More<Content: View>: View {
    let label: String
    let content: Content
    @SwiftUI.State private var open = false
    @Environment(\.accessibilityReduceMotion) private var still

    init(label: String = "More", @ViewBuilder content: () -> Content) {
        self.label = label
        self.content = content()
    }

    var body: some View {
        let shape = RoundedRectangle(cornerRadius: 12, style: .continuous)
        VStack(alignment: .leading, spacing: 0) {
            Button {
                withAnimation(still ? nil : .default) { open.toggle() }
            } label: {
                HStack(alignment: .center, spacing: 0) {
                    Text(label)
                        .textStyle(Theme.type.labelLarge)
                        .foregroundStyle(Theme.color.primary)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    Image(systemName: open ? "chevron.up" : "chevron.down")
                        .font(.system(size: 17, weight: .semibold))
                        .foregroundStyle(Theme.color.primary)
                        .frame(width: 24, height: 24)
                        .accessibilityLabel(open ? "Hide" : "Show")
                }
                .padding(.horizontal, 4)
                .frame(maxWidth: .infinity, minHeight: 48)
                .contentShape(shape)
            }
            .buttonStyle(.plain)
            if open {
                VStack(alignment: .leading, spacing: 0) { content }.transition(.opacity)
            }
        }
    }
}

/// One of a few, side by side: the one chosen stands out. Each is tagged `segment`, then its name.
struct Segmented: View {
    let options: [String]
    let chosen: Int
    let choose: (Int) -> Void

    init(options: [String], chosen: Int, choose: @escaping (Int) -> Void) {
        self.options = options
        self.chosen = chosen
        self.choose = choose
    }

    var body: some View {
        let colors = Theme.color
        HStack(alignment: .center, spacing: 0) {
            ForEach(Array(options.enumerated()), id: \.offset) { i, option in
                let on = i == chosen
                Button { choose(i) } label: {
                    Text(option)
                        .textStyle(Theme.type.labelLarge)
                        .foregroundStyle(on ? colors.onSurface : colors.onSurfaceVariant)
                        .lineLimit(1)
                        .frame(maxWidth: .infinity)
                        .frame(height: 44)
                        .background(on ? colors.surfaceContainerHighest : Color.clear, in: Capsule())
                        .contentShape(Capsule())
                }
                .buttonStyle(.plain)
                .accessibilityIdentifier("segment \(option)")
                .accessibilityAddTraits(on ? .isSelected : [])
            }
        }
        .padding(4)
        .background(colors.surfaceContainer, in: Capsule())
    }
}

/// Where a step of something under way stands.
enum Mark { case done, now, next, failed }

/// A step of a timeline: a mark, joined to the next step's by a line unless
/// `last`, its title, what it says of it, and something to do, if any.
/// `tag` names it, for the tests, `step` then its title if not given.
struct Step: View {
    let mark: Mark
    let title: String
    let detail: String?
    let last: Bool
    let action: (String, () -> Void)?
    let tag: String

    init(
        mark: Mark,
        title: String,
        detail: String? = nil,
        last: Bool = false,
        action: (String, () -> Void)? = nil,
        tag: String? = nil
    ) {
        self.mark = mark
        self.title = title
        self.detail = detail
        self.last = last
        self.action = action
        self.tag = tag ?? "step \(title)"
    }

    var body: some View {
        let scheme = Theme.color
        HStack(alignment: .top, spacing: 0) {
            VStack(alignment: .center, spacing: 0) {
                Spacer().frame(height: 12)
                ZStack {
                    switch mark {
                    case .done:
                        Image(systemName: "checkmark.circle.fill")
                            .font(.system(size: 17))
                            .foregroundStyle(scheme.primary)
                            .accessibilityLabel("Done")
                    case .failed:
                        Image(systemName: "exclamationmark.triangle.fill")
                            .font(.system(size: 15))
                            .foregroundStyle(scheme.error)
                            .accessibilityLabel("Failed")
                    case .now:
                        CircularProgressIndicator(size: 16, strokeWidth: 2)
                    case .next:
                        Circle().strokeBorder(scheme.outline, lineWidth: 2).frame(width: 14, height: 14)
                    }
                }
                .frame(width: 20, height: 20)
                if !last {
                    Spacer().frame(height: 4)
                    let line = mark == .done ? scheme.primary : scheme.outlineVariant
                    Rectangle().fill(line).frame(width: 2).frame(maxHeight: .infinity)
                }
            }
            .frame(width: 28)
            .frame(maxHeight: .infinity, alignment: .top)
            Spacer().frame(width: 12)
            VStack(alignment: .leading, spacing: 0) {
                Text(title)
                    .textStyle(Theme.type.bodyLarge)
                    .foregroundStyle(
                        mark == .next ? scheme.onSurfaceVariant
                            : mark == .failed ? scheme.error
                            : scheme.onSurface
                    )
                if let detail {
                    Text(detail).textStyle(Theme.type.bodySmall).foregroundStyle(scheme.onSurfaceVariant)
                }
                if let action {
                    TextButton(onClick: action.1) { Text(action.0) }.frame(height: 36)
                }
            }
            .padding(.top, 10)
            .padding(.bottom, last ? 10 : 14)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        // As tall as its words: the line runs down beside them.
        .fixedSize(horizontal: false, vertical: true)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier(tag)
    }
}

/// The few Material icons (Apache 2.0) drawn from their path data: on
/// Material's grid of 24, as templates the foreground style tints.
enum Glyphs {
    static let up = icon(path: "M4,12l1.41,1.41L11,7.83V20h2V7.83l5.58,5.59L20,12l-8,-8 -8,8z")
    static let down = icon(path: "M20,12l-1.41,-1.41L13,16.17V4h-2v12.17l-5.58,-5.59L4,12l8,8 8,-8z")
    static let swap = icon(path: "M6.99,11L3,15l3.99,4v-3H14v-2H6.99v-3zM21,9l-3.99,-4v3H10v2h7.01v3L21,9z")
    static let bolt = icon(
        path: "M11,21h-1l1,-7H7.5c-0.58,0 -0.57,-0.32 -0.38,-0.66 0.19,-0.34 0.05,-0.08 0.07,-0.12" +
            "C8.48,10.94 10.42,7.54 13,3h1l-1,7h3.5c0.49,0 0.56,0.33 0.47,0.51l-0.07,0.15" +
            "C12.96,17.55 11,21 11,21z"
    )

    /// Protected: a shield, checked.
    static let shield = icon(
        path: "M12,1L3,5v6c0,5.55 3.84,10.74 9,12 5.16,-1.26 9,-6.45 9,-12V5l-9,-4z" +
            "M10,17l-4,-4 1.41,-1.41L10,14.17l6.59,-6.59L18,9l-8,8z"
    )

    /// Not protected: a shield's outline, empty.
    static let shieldOff = icon(
        path: "M12,1L3,5v6c0,5.55 3.84,10.74 9,12 5.16,-1.26 9,-6.45 9,-12V5l-9,-4z" +
            "M19,11c0,4.52 -2.98,8.69 -7,9.93 -4.02,-1.24 -7,-5.41 -7,-9.93V6.3l7,-3.11 7,3.11V11z"
    )

    /// Where money is held: a hub.
    static let bank = icon(path: "M4,10h3v7H4zM10.5,10h3v7h-3zM2,19h20v3H2zM17,10h3v7h-3zM12,1L2,6v2h20V6z")

    /// Bitcoin's sign.
    static let bitcoin = icon(
        path: "M17.06,11.57C17.65,10.88 18,9.98 18,9c0,-1.86 -1.27,-3.43 -3,-3.87L15,3h-2v2h-2V3H9v2H6v2h2v10H6v2h3v2" +
            "h2v-2h2v2h2v-2c2.21,0 4,-1.79 4,-4C19,13.55 18.22,12.27 17.06,11.57zM10,7h4c1.1,0 2,0.9 2,2" +
            "s-0.9,2 -2,2h-4V7zM15,17h-5v-4h5c1.1,0 2,0.9 2,2S16.1,17 15,17z"
    )

    /// What is under the hood: advanced mode.
    static let code = icon(
        path: "M9.4,16.6L4.8,12l4.6,-4.6L8,6l-6,6 6,6 1.4,-1.4zM14.6,16.6l4.6,-4.6 -4.6,-4.6L16,6l6,6 -6,6 -1.4,-1.4z"
    )

    /// What restores the wallet: a key.
    static let key = icon(
        path: "M12.65,10C11.83,7.67 9.61,6 7,6c-3.31,0 -6,2.69 -6,6s2.69,6 6,6c2.61,0 4.83,-1.67 5.65,-4" +
            "H17v4h4v-4h2v-4H12.65zM7,14c-1.1,0 -2,-0.9 -2,-2s0.9,-2 2,-2 2,0.9 2,2 -0.9,2 -2,2z"
    )

    private static func icon(path: String) -> ImageVector {
        let format = UIGraphicsImageRendererFormat()
        // Finer than any screen, to stay sharp scaled down to a glyph's size.
        format.scale = 4
        let drawn = UIGraphicsImageRenderer(size: CGSize(width: 24, height: 24), format: format).image { context in
            context.cgContext.addPath(parsed(path: path))
            context.cgContext.fillPath()
        }
        return Image(uiImage: drawn.withRenderingMode(.alwaysTemplate))
    }
}

/// A part of path data: a command's letter, or one of its numbers.
private enum PathPart {
    case command(Character)
    case number(CGFloat)
}

/// Path data as SVG writes it, as a path: the commands Material's icons use,
/// M, L, H, V, C, S and Z, absolute or, in lower case, relative, each
/// repeated while numbers follow it.
private func parsed(path data: String) -> CGPath {
    var parts: [PathPart] = []
    var number = ""
    func end() {
        if let n = Double(number) { parts.append(.number(CGFloat(n))) }
        number = ""
    }
    for c in data {
        if c.isLetter {
            end()
            parts.append(.command(c))
        } else if c == "," || c == " " {
            end()
        } else {
            // A sign, or a second point, starts the next number.
            if c == "-" || (c == "." && number.contains(".")) { end() }
            number.append(c)
        }
    }
    end()

    let path = CGMutablePath()
    var point = CGPoint.zero
    var start = CGPoint.zero
    // The second control point of the last curve, which a smooth one mirrors.
    var control: CGPoint?
    var command: Character = "M"
    var i = 0
    func next() -> CGFloat {
        defer { i += 1 }
        if i < parts.count, case .number(let n) = parts[i] { return n }
        return 0
    }
    while i < parts.count {
        if case .command(let c) = parts[i] {
            command = c
            i += 1
            if c == "z" || c == "Z" {
                path.closeSubpath()
                point = start
                control = nil
                continue
            }
        }
        let base = command.isLowercase ? point : .zero
        switch command.uppercased() {
        case "M":
            point = CGPoint(x: base.x + next(), y: base.y + next())
            path.move(to: point)
            start = point
            control = nil
            // Pairs after the first are lines.
            command = command.isLowercase ? "l" : "L"
        case "L":
            point = CGPoint(x: base.x + next(), y: base.y + next())
            path.addLine(to: point)
            control = nil
        case "H":
            point = CGPoint(x: base.x + next(), y: point.y)
            path.addLine(to: point)
            control = nil
        case "V":
            point = CGPoint(x: point.x, y: base.y + next())
            path.addLine(to: point)
            control = nil
        case "C":
            let first = CGPoint(x: base.x + next(), y: base.y + next())
            let second = CGPoint(x: base.x + next(), y: base.y + next())
            point = CGPoint(x: base.x + next(), y: base.y + next())
            path.addCurve(to: point, control1: first, control2: second)
            control = second
        case "S":
            let first = control.map { CGPoint(x: 2 * point.x - $0.x, y: 2 * point.y - $0.y) } ?? point
            let second = CGPoint(x: base.x + next(), y: base.y + next())
            point = CGPoint(x: base.x + next(), y: base.y + next())
            path.addCurve(to: point, control1: first, control2: second)
            control = second
        default:
            i += 1
        }
    }
    return path
}
