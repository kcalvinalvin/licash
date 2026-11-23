import SwiftUI

/*
 * The Material 3 components the screens use, drawn as Material 3 draws
 * them: their sizes, shapes and colors, from the theme's roles.
 */

/// A button with no container: its label in the primary color.
struct TextButton<Content: View>: View {
    let onClick: () -> Void
    let enabled: Bool
    let content: Content

    init(onClick: @escaping () -> Void, enabled: Bool = true, @ViewBuilder content: () -> Content) {
        self.onClick = onClick
        self.enabled = enabled
        self.content = content()
    }

    var body: some View {
        Button(action: onClick) { content }
            .buttonStyle(MaterialButton(container: nil, content: Theme.color.primary, horizontal: 12))
            .disabled(!enabled)
    }
}

/// A second button, quieter than the first: the secondary container.
struct FilledTonalButton<Content: View>: View {
    let onClick: () -> Void
    let enabled: Bool
    let content: Content

    init(onClick: @escaping () -> Void, enabled: Bool = true, @ViewBuilder content: () -> Content) {
        self.onClick = onClick
        self.enabled = enabled
        self.content = content()
    }

    var body: some View {
        Button(action: onClick) { content }
            .buttonStyle(
                MaterialButton(
                    container: Theme.color.secondaryContainer,
                    content: Theme.color.onSecondaryContainer,
                    horizontal: 24
                )
            )
            .disabled(!enabled)
    }
}

/// A button in an outline, no container.
struct OutlinedButton<Content: View>: View {
    let onClick: () -> Void
    let enabled: Bool
    let content: Content

    init(onClick: @escaping () -> Void, enabled: Bool = true, @ViewBuilder content: () -> Content) {
        self.onClick = onClick
        self.enabled = enabled
        self.content = content()
    }

    var body: some View {
        Button(action: onClick) { content }
            .buttonStyle(
                MaterialButton(
                    container: nil,
                    content: Theme.color.onSurfaceVariant,
                    border: Theme.color.outlineVariant,
                    horizontal: 24
                )
            )
            .disabled(!enabled)
    }
}

/// Material's filled button: the primary color.
struct FilledButton<Content: View>: View {
    let onClick: () -> Void
    let enabled: Bool
    let content: Content

    init(onClick: @escaping () -> Void, enabled: Bool = true, @ViewBuilder content: () -> Content) {
        self.onClick = onClick
        self.enabled = enabled
        self.content = content()
    }

    var body: some View {
        Button(action: onClick) { content }
            .buttonStyle(MaterialButton(container: Theme.color.primary, content: Theme.color.onPrimary, horizontal: 24))
            .disabled(!enabled)
    }
}

/// A glyph to tap, in the color around it: 40 points drawn, 48 to touch.
struct IconButton<Content: View>: View {
    let onClick: () -> Void
    let enabled: Bool
    let content: Content

    init(onClick: @escaping () -> Void, enabled: Bool = true, @ViewBuilder content: () -> Content) {
        self.onClick = onClick
        self.enabled = enabled
        self.content = content()
    }

    var body: some View {
        Button(action: onClick) { content }
            .buttonStyle(IconStyle())
            .disabled(!enabled)
    }
}

/// A field in an outline: its label inside until it is typed in, then on
/// the outline, and a line under it, red if `isError`. Its `suffix` shows
/// at the end once the label moves up.
struct OutlinedTextField: View {
    let value: String
    let onValueChange: (String) -> Void
    let label: String?
    let placeholder: String?
    let suffix: String?
    let isError: Bool
    let supportingText: String?
    let singleLine: Bool
    let minLines: Int
    let enabled: Bool
    let readOnly: Bool
    let textStyle: TextStyle
    let shape: RoundedRectangle
    // What the field shows: what is typed, until the caller has it, then `value`.
    @State private var shown: String
    // The edits given to the caller: `value` shows after each, even one it
    // turned down and so left as it was.
    @State private var edits = 0
    @FocusState private var focused: Bool
    @State private var labelWidth: CGFloat = 0

    init(
        value: String,
        onValueChange: @escaping (String) -> Void,
        label: String? = nil,
        placeholder: String? = nil,
        suffix: String? = nil,
        isError: Bool = false,
        supportingText: String? = nil,
        singleLine: Bool = false,
        minLines: Int = 1,
        enabled: Bool = true,
        readOnly: Bool = false,
        textStyle: TextStyle = Theme.type.bodyLarge,
        shape: RoundedRectangle = RoundedRectangle(cornerRadius: 4)
    ) {
        self.value = value
        self.onValueChange = onValueChange
        self.label = label
        self.placeholder = placeholder
        self.suffix = suffix
        self.isError = isError
        self.supportingText = supportingText
        self.singleLine = singleLine
        self.minLines = minLines
        self.enabled = enabled
        self.readOnly = readOnly
        self.textStyle = textStyle
        self.shape = shape
        _shown = State(initialValue: value)
    }

    var body: some View {
        let colors = Theme.color
        let up = focused || !value.isEmpty
        let accent = isError ? colors.error : focused ? colors.primary : colors.onSurfaceVariant
        let outline = isError ? colors.error : focused ? colors.primary : colors.outline
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .firstTextBaseline, spacing: 2) {
                ZStack(alignment: singleLine ? .leading : .topLeading) {
                    if value.isEmpty, let placeholder, label == nil || focused {
                        Text(placeholder).foregroundStyle(colors.onSurfaceVariant)
                    }
                    if readOnly {
                        Text(value).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
                    } else {
                        TextField("", text: $shown, axis: singleLine ? .horizontal : .vertical)
                            .lineLimit(singleLine ? 1...1 : minLines...Int.max)
                            .focused($focused)
                            .onChange(of: shown) {
                                guard shown != value else { return }
                                onValueChange(shown)
                                edits += 1
                            }
                            .onChange(of: edits) { shown = value }
                            .onChange(of: value) { shown = value }
                    }
                }
                if let suffix {
                    Text(suffix).foregroundStyle(colors.onSurfaceVariant).opacity(label == nil || up ? 1 : 0)
                }
            }
            .textStyle(textStyle)
            .foregroundStyle(colors.onSurface)
            .tint(isError ? colors.error : colors.primary)
            .padding(.horizontal, 16)
            .padding(.vertical, 16)
            .frame(maxWidth: .infinity, minHeight: 56, alignment: singleLine ? .leading : .topLeading)
            .overlay(alignment: up ? .topLeading : singleLine ? .leading : .topLeading) {
                if let label {
                    Text(label)
                        .textStyle(up ? Theme.type.bodySmall : Theme.type.bodyLarge)
                        .foregroundStyle(up ? accent : colors.onSurfaceVariant)
                        .lineLimit(1)
                        .padding(.horizontal, up ? 4 : 0)
                        .background(GeometryReader { Color.clear.preference(key: LabelWidth.self, value: $0.size.width) })
                        .offset(x: up ? 12 : 16, y: up ? -8 : singleLine ? 0 : 16)
                        .allowsHitTesting(false)
                }
            }
            .onPreferenceChange(LabelWidth.self) { labelWidth = $0 }
            .background {
                Notched(gap: label != nil && up ? labelWidth : 0, radius: shape.cornerSize.width)
                    .stroke(outline, lineWidth: focused ? 2 : 1)
            }
            .contentShape(Rectangle())
            .onTapGesture { focused = true }
            .animation(.easeInOut(duration: 0.15), value: up)
            if let supportingText {
                Text(supportingText)
                    .textStyle(Theme.type.bodySmall)
                    .foregroundStyle(isError ? colors.error : colors.onSurfaceVariant)
                    .padding(.horizontal, 16)
                    .padding(.top, 4)
            }
        }
        .opacity(enabled ? 1 : 0.38)
        .disabled(!enabled)
    }
}

/// A bar of how far something is, or, `nil`, that it is under way.
struct LinearProgressIndicator: View {
    let progress: Double?

    init(progress: Double? = nil) {
        self.progress = progress
    }

    var body: some View {
        let track = Theme.color.secondaryContainer
        GeometryReader { geometry in
            let width = geometry.size.width
            if let progress {
                // What is done, a gap, what is left, and a dot where it ends.
                let done = width * min(max(progress, 0), 1)
                let left = done > 0 ? min(done + 4, width) : 0
                ZStack(alignment: .leading) {
                    if left < width {
                        Capsule().fill(track).frame(width: width - left).offset(x: left)
                        Circle().fill(.tint).frame(width: 4, height: 4).offset(x: width - 4)
                    }
                    if done > 0 { Capsule().fill(.tint).frame(width: done) }
                }
            } else {
                TimelineView(.animation) { timeline in
                    let (a, b) = sweeps(at: timeline.date)
                    ZStack(alignment: .leading) {
                        Capsule().fill(track)
                        Capsule().fill(.tint).frame(width: width * max(a.1 - a.0, 0)).offset(x: width * a.0)
                        Capsule().fill(.tint).frame(width: width * max(b.1 - b.0, 0)).offset(x: width * b.0)
                    }
                }
            }
        }
        .frame(height: 4)
        .clipped()
    }

    /// The two bars moving across, as fractions of the track: Material's 1.75 s cycle.
    private func sweeps(at date: Date) -> ((Double, Double), (Double, Double)) {
        let t = date.timeIntervalSinceReferenceDate.truncatingRemainder(dividingBy: 1.75) * 1000
        func eased(from: Double, to: Double) -> Double {
            let x = min(max((t - from) / (to - from), 0), 1)
            return x * x * (3 - 2 * x)
        }
        let first = (eased(from: 333, to: 1183), eased(from: 0, to: 750))
        let second = (eased(from: 1267, to: 1750), eased(from: 1000, to: 1567))
        return (first, second)
    }
}

/// A ring of how far something is, or, `nil`, spinning while it is under
/// way: in the tint, which `.tint(_:)` sets.
struct CircularProgressIndicator: View {
    let progress: Double?
    let size: CGFloat
    let strokeWidth: CGFloat

    init(progress: Double? = nil, size: CGFloat = 40, strokeWidth: CGFloat = 4) {
        self.progress = progress
        self.size = size
        self.strokeWidth = strokeWidth
    }

    var body: some View {
        let stroke = StrokeStyle(lineWidth: strokeWidth, lineCap: .round)
        SwiftUI.Group {
            if let progress {
                let done = min(max(progress, 0), 1)
                ZStack {
                    Circle().inset(by: strokeWidth / 2).trim(from: done, to: 1)
                        .stroke(Theme.color.secondaryContainer, style: stroke)
                    Circle().inset(by: strokeWidth / 2).trim(from: 0, to: done).stroke(.tint, style: stroke)
                }
                .rotationEffect(.degrees(-90))
            } else {
                TimelineView(.animation) { timeline in
                    let t = timeline.date.timeIntervalSinceReferenceDate
                    // The arc grows and shrinks each 1.33 s as the whole turns.
                    let phase = t.truncatingRemainder(dividingBy: 1.332) / 1.332
                    let sweep = 0.05 + 0.7 * (phase < 0.5 ? phase * 2 : 2 - phase * 2)
                    Circle().inset(by: strokeWidth / 2).trim(from: 0, to: sweep).stroke(.tint, style: stroke)
                        .rotationEffect(.degrees(t.truncatingRemainder(dividingBy: 2) * 360 + phase * 290))
                }
            }
        }
        .frame(width: size, height: size)
    }
}

/// A choice among a few, on or off: on, in the secondary container.
struct FilterChip: View {
    let selected: Bool
    let onClick: () -> Void
    let label: String

    init(selected: Bool, onClick: @escaping () -> Void, label: String) {
        self.selected = selected
        self.onClick = onClick
        self.label = label
    }

    var body: some View {
        let colors = Theme.color
        let shape = RoundedRectangle(cornerRadius: 8, style: .continuous)
        Button(action: onClick) {
            Text(label)
                .textStyle(Theme.type.labelLarge)
                .lineLimit(1)
                .foregroundStyle(selected ? colors.onSecondaryContainer : colors.onSurfaceVariant)
                .padding(.horizontal, 16)
                .frame(minHeight: 32)
                .background(selected ? colors.secondaryContainer : Color.clear, in: shape)
                .overlay { if !selected { shape.strokeBorder(colors.outlineVariant, lineWidth: 1) } }
                .frame(minHeight: 48)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityAddTraits(selected ? .isSelected : [])
    }
}

/// One of a few, chosen or not: a ring, filled when chosen. With no
/// `onClick`, its row takes the tap.
struct RadioButton: View {
    let selected: Bool
    let onClick: (() -> Void)?

    init(selected: Bool, onClick: (() -> Void)?) {
        self.selected = selected
        self.onClick = onClick
    }

    var body: some View {
        let color = selected ? Theme.color.primary : Theme.color.onSurfaceVariant
        let ring = ZStack {
            Circle().strokeBorder(color, lineWidth: 2).frame(width: 20, height: 20)
            if selected { Circle().fill(color).frame(width: 12, height: 12) }
        }
        .padding(2)
        if let onClick {
            Button(action: onClick) { ring.frame(width: 48, height: 48).contentShape(Rectangle()) }
                .buttonStyle(.plain)
                .accessibilityAddTraits(selected ? .isSelected : [])
        } else {
            ring.accessibilityAddTraits(selected ? .isSelected : [])
        }
    }
}

/// On or off, as Material draws it: a track, and a handle that grows as it
/// moves to the end. With no `onCheckedChange`, its row takes the tap.
struct Switch: View {
    let checked: Bool
    let onCheckedChange: ((Bool) -> Void)?

    init(checked: Bool, onCheckedChange: ((Bool) -> Void)?) {
        self.checked = checked
        self.onCheckedChange = onCheckedChange
    }

    var body: some View {
        let colors = Theme.color
        ZStack(alignment: checked ? .trailing : .leading) {
            Capsule().fill(checked ? colors.primary : colors.surfaceContainerHighest)
            if !checked { Capsule().strokeBorder(colors.outline, lineWidth: 2) }
            Circle()
                .fill(checked ? colors.onPrimary : colors.outline)
                .frame(width: checked ? 24 : 16, height: checked ? 24 : 16)
                .padding(.horizontal, checked ? 4 : 8)
        }
        .frame(width: 52, height: 32)
        .frame(minHeight: 48)
        .contentShape(Rectangle())
        .onTapGesture { onCheckedChange?(!checked) }
        .allowsHitTesting(onCheckedChange != nil)
        .animation(.easeInOut(duration: 0.15), value: checked)
        .accessibilityRepresentation {
            SwiftUI.Toggle("", isOn: Binding(get: { checked }, set: { onCheckedChange?($0) }))
        }
    }
}

/// The look of a Material button: its container, if any, the color of
/// what is in it, its outline, if any, and how far in from its ends what
/// is in it sits.
private struct MaterialButton: ButtonStyle {
    let container: Color?
    let content: Color
    var border: Color? = nil
    let horizontal: CGFloat

    func makeBody(configuration: Configuration) -> some View {
        Shown(configuration: configuration, style: self)
    }

    private struct Shown: View {
        let configuration: ButtonStyleConfiguration
        let style: MaterialButton
        @Environment(\.isEnabled) private var enabled

        var body: some View {
            let colors = Theme.color
            let content = enabled ? style.content : colors.onSurfaceVariant.opacity(0.38)
            let container = style.container.map { enabled ? $0 : colors.onSurface.opacity(0.1) } ?? .clear
            configuration.label
                .textStyle(Theme.type.labelLarge)
                .foregroundStyle(content)
                .tint(content)
                .padding(.horizontal, style.horizontal)
                .padding(.vertical, 8)
                .frame(minWidth: 58, minHeight: 40)
                .background(container, in: Capsule())
                .overlay { if let border = style.border { Capsule().strokeBorder(border, lineWidth: 1) } }
                // Pressed: a layer of what is in it, faintly.
                .overlay { Capsule().fill(content.opacity(configuration.isPressed ? 0.1 : 0)) }
                .frame(minHeight: 48)
                .contentShape(Rectangle())
        }
    }
}

/// An icon button's look: a circle of 40 under the glyph while pressed.
private struct IconStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        Shown(configuration: configuration)
    }

    private struct Shown: View {
        let configuration: ButtonStyleConfiguration
        @Environment(\.isEnabled) private var enabled

        var body: some View {
            configuration.label
                .frame(width: 40, height: 40)
                .background(Circle().fill(ForegroundStyle().opacity(configuration.isPressed ? 0.1 : 0)))
                .opacity(enabled ? 1 : 0.38)
                .frame(width: 48, height: 48)
                .contentShape(Rectangle())
        }
    }
}

/// How wide a field's label is, for the gap in its outline.
private struct LabelWidth: PreferenceKey {
    static let defaultValue: CGFloat = 0

    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) {
        value = nextValue()
    }
}

/// A field's outline: corners of 4, and a gap `gap` wide at the top, 12 in,
/// where its label sits once typed in.
private struct Notched: Shape {
    var gap: CGFloat
    let radius: CGFloat

    var animatableData: CGFloat {
        get { gap }
        set { gap = newValue }
    }

    func path(in rect: CGRect) -> Path {
        var path = Path()
        path.move(to: CGPoint(x: rect.minX + 12 + gap, y: rect.minY))
        path.addArc(tangent1End: CGPoint(x: rect.maxX, y: rect.minY), tangent2End: CGPoint(x: rect.maxX, y: rect.maxY), radius: radius)
        path.addArc(tangent1End: CGPoint(x: rect.maxX, y: rect.maxY), tangent2End: CGPoint(x: rect.minX, y: rect.maxY), radius: radius)
        path.addArc(tangent1End: CGPoint(x: rect.minX, y: rect.maxY), tangent2End: CGPoint(x: rect.minX, y: rect.minY), radius: radius)
        path.addArc(tangent1End: CGPoint(x: rect.minX, y: rect.minY), tangent2End: CGPoint(x: rect.maxX, y: rect.minY), radius: radius)
        path.addLine(to: CGPoint(x: rect.minX + 12, y: rect.minY))
        return path
    }
}
