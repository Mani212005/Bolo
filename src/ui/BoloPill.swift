// bolo-pill: the small on-screen pill that shows whether Bolo is recording,
// transcribing or done. It is a separate process drawn from the daemon's event
// stream (`subscribe` on ~/.config/bolo/bolo.sock) and started and restarted
// by the daemon. Clicking it sends the same `toggle` / `pause` commands as the
// hotkeys.
//
// It runs as a bare binary, no app bundle: the accessory activation policy keeps
// it out of the Dock, and a non-activating panel that can never become key
// means it never takes keyboard focus from the app being dictated into.
// Nothing here may ever call NSApp.activate.
//
//   swiftc -O BoloPill.swift -o bolo-pill -framework Cocoa -framework QuartzCore
//   bolo-pill                      run (needs a daemon)
//   bolo-pill --snapshot <dir>     render every state of every style to PNG, offscreen, no window
//   bolo-pill --selftest           check layout, labels, menu and hit testing; exit 1 on a failure
import Cocoa
import QuartzCore

// MARK: - Palette

func color(_ hex: UInt32, _ alpha: CGFloat = 1) -> CGColor {
    CGColor(
        red: CGFloat((hex >> 16) & 0xff) / 255, green: CGFloat((hex >> 8) & 0xff) / 255,
        blue: CGFloat(hex & 0xff) / 255, alpha: alpha)
}

enum Palette {
    static let panel: UInt32 = 0x0c0d12
    static let handle: UInt32 = 0x9aa2b5
    static let recording: UInt32 = 0xf43f5e
    static let paused: UInt32 = 0xf59e0b
    static let transcribing: UInt32 = 0x38bdf8  // Bolo accent blue
    static let done: UInt32 = 0x10b981
    static let warning: UInt32 = 0xf59e0b
}

// MARK: - Model

enum PillState: String, CaseIterable {
    case idle, recording, paused, transcribing, done, error
}

/// `[pill] style` in config.toml. Hidden draws nothing (the daemon also stops this process).
enum PillStyle: String {
    case small, large, hidden
}

/// The two buttons of the Large panel.
enum PillButton {
    case pauseResume, stop
}

struct Level {
    var value: CGFloat  // 0...1, smoothed
    var speech: Bool  // the VAD heard speech in this chunk
}

/// Everything the layer tree needs to draw one frame of the pill.
struct PillAppearance {
    var state: PillState
    var style: PillStyle = .small
    var hover = false  // idle only: the handle grows into a "Dictate" button
    var label = ""  // done / error text, and "Dictate" on hover
    var levels: [Level] = []
    var elapsed: TimeInterval = 0  // Large: recording time so far
    var reduceMotion = false  // System Settings > Accessibility > Display > Reduce motion
    var pressed: PillButton?  // Large: the button under the pointer while the mouse is down

    /// The big panel is only for live states; the idle handle and results stay compact.
    var isLarge: Bool {
        style == .large && (state == .recording || state == .paused || state == .transcribing)
    }
}

/// What the pill says for a finished dictation (`outcome` event).
func outcomeAppearance(kind: String, detail: String) -> (state: PillState, label: String) {
    switch kind {
    case "done":
        switch detail {
        case "copied": return (.done, "Copied")
        case "typed": return (.done, "Typed")
        default: return (.done, "Pasted")
        }
    case "no-speech": return (.error, "No speech")
    case "mic-unavailable": return (.error, "Mic unavailable")
    case "paste-interrupted": return (.error, "Paste stopped")
    case "max-length": return (.error, "Max length")
    default: return (.error, "Error")
    }
}

/// How long a result stays on screen before the pill returns to idle.
func holdSeconds(_ state: PillState) -> TimeInterval { state == .done ? 0.7 : 1.8 }

/// Fast attack, slow release, so speech jumps up and room noise settles.
func smoothed(previous: CGFloat, next: CGFloat) -> CGFloat {
    let rate: CGFloat = next > previous ? 0.6 : 0.2
    return previous + (next - previous) * rate
}

// MARK: - Drawing

let barCount = 11
let barWidth: CGFloat = 3
let barGap: CGFloat = 3
let pillHeight: CGFloat = 32
let handleSize = CGSize(width: 44, height: 8)
let largeSize = CGSize(width: 300, height: 84)
let largeBarCount = 45
let largeWave = CGRect(x: 16, y: 38, width: 268, height: 36)
let largeRow = CGRect(x: 16, y: 8, width: 268, height: 24)
let timerFont = NSFont.monospacedDigitSystemFont(ofSize: 12, weight: .medium)
let labelFont = NSFont.systemFont(ofSize: 12, weight: .semibold)

/// A symbol rasterised at 3x in one colour, cached.
var glyphCache: [String: CGImage] = [:]
func glyph(_ name: String, points: CGFloat, hex: UInt32) -> CGImage? {
    let key = "\(name)/\(points)/\(hex)"
    if let cached = glyphCache[key] { return cached }
    guard let base = NSImage(systemSymbolName: name, accessibilityDescription: nil) else { return nil }
    let tint = NSColor(cgColor: color(hex)) ?? .white
    let config = NSImage.SymbolConfiguration(pointSize: points, weight: .bold)
        .applying(NSImage.SymbolConfiguration(paletteColors: [tint]))
    guard let image = base.withSymbolConfiguration(config) else { return nil }
    let scale: CGFloat = 3
    let pixels = NSSize(width: ceil(image.size.width * scale), height: ceil(image.size.height * scale))
    guard
        let rep = NSBitmapImageRep(
            bitmapDataPlanes: nil, pixelsWide: Int(pixels.width), pixelsHigh: Int(pixels.height),
            bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
            colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)
    else { return nil }
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    image.draw(in: NSRect(origin: .zero, size: pixels))
    NSGraphicsContext.restoreGraphicsState()
    guard let cg = rep.cgImage else { return nil }
    glyphCache[key] = cg
    return cg
}

func textWidth(_ text: String) -> CGFloat {
    ceil((text as NSString).size(withAttributes: [.font: labelFont]).width)
}

/// Recording time as a clock: 0:07, 12:34, 1:02:03.
func formatElapsed(_ seconds: TimeInterval) -> String {
    let total = max(0, Int(seconds))
    let h = total / 3600, m = (total % 3600) / 60, s = total % 60
    return h > 0 ? String(format: "%d:%02d:%02d", h, m, s) : String(format: "%d:%02d", m, s)
}

/// Where the Large panel's buttons are, and what they say. Shared by drawing and
/// hit testing so the two can never disagree.
struct ButtonSpec {
    var button: PillButton
    var rect: CGRect
    var title: String
    var symbol: String
}

func largeButtons(for a: PillAppearance) -> [ButtonSpec] {
    guard a.isLarge, a.state != .transcribing else { return [] }
    return [
        ButtonSpec(
            button: .pauseResume, rect: CGRect(x: 156, y: 8, width: 72, height: 24),
            title: a.state == .paused ? "Resume" : "Pause", symbol: a.state == .paused ? "play.fill" : "pause.fill"),
        ButtonSpec(
            button: .stop, rect: CGRect(x: 234, y: 8, width: 50, height: 24), title: "Stop", symbol: "stop.fill"),
    ]
}

/// Which Large button is at `point` (view coordinates, origin bottom left).
func buttonHit(_ point: CGPoint, in a: PillAppearance) -> PillButton? {
    largeButtons(for: a).first { $0.rect.contains(point) }?.button
}

/// What VoiceOver says. The pill is a non-activating panel, so it also announces
/// state changes itself (see `PillController.announce`).
func accessibilityText(for a: PillAppearance) -> (label: String, hint: String) {
    switch a.state {
    case .idle: return ("Bolo, ready to dictate", "Click to start recording")
    case .recording:
        return a.style == .large
            ? ("Bolo recording, \(formatElapsed(a.elapsed))", "Use Pause or Stop") : ("Bolo recording", "Click to stop and transcribe")
    case .paused:
        return a.style == .large
            ? ("Bolo paused, \(formatElapsed(a.elapsed))", "Use Resume or Stop") : ("Bolo paused", "Click to resume")
    case .transcribing: return ("Bolo transcribing", "")
    case .done: return ("Bolo, \(a.label)", "")
    case .error: return ("Bolo, \(a.label)", "Click to open the Bolo dashboard")
    }
}

/// A rounded button drawn inside the Large panel.
final class ButtonLayer: CALayer {
    let icon = CALayer()
    let text = CATextLayer()

    override init() {
        super.init()
        icon.contentsGravity = .resizeAspect
        addSublayer(icon)
        text.font = labelFont
        text.fontSize = 11.5
        text.alignmentMode = .left
        addSublayer(text)
    }
    override init(layer: Any) { super.init(layer: layer) }
    required init?(coder: NSCoder) { fatalError() }

    func configure(_ spec: ButtonSpec, pressed: Bool, scale: CGFloat) {
        frame = spec.rect
        cornerRadius = spec.rect.height / 2
        let isStop = spec.button == .stop
        let base: CGFloat = isStop ? 0.9 : 0.14
        backgroundColor = isStop ? color(Palette.recording, pressed ? 1 : base) : color(0xffffff, pressed ? 0.28 : base)
        let textWidth = ceil((spec.title as NSString).size(withAttributes: [.font: NSFont.systemFont(ofSize: 11.5, weight: .semibold)]).width)
        let iconSize: CGFloat = 10
        let total = iconSize + 5 + textWidth
        let x = (spec.rect.width - total) / 2
        icon.frame = CGRect(x: x, y: (spec.rect.height - iconSize) / 2, width: iconSize, height: iconSize)
        icon.contents = glyph(spec.symbol, points: 9, hex: 0xffffff)
        icon.contentsScale = scale
        text.contentsScale = scale
        text.string = spec.title
        text.foregroundColor = color(0xffffff, 0.95)
        text.frame = CGRect(x: x + iconSize + 5, y: (spec.rect.height - 15) / 2, width: textWidth + 4, height: 15)
    }
}

/// Layer tree for the pill. The same tree is shown on screen and rendered to PNG.
final class PillLayer: CALayer {
    let background = CALayer()
    let dot = CALayer()
    let icon = CALayer()
    let text = CATextLayer()
    var bars: [CALayer] = []
    let pauseButton = ButtonLayer()
    let stopButton = ButtonLayer()
    private(set) var shown: PillState?
    private enum Motion { case none, wave, fade }
    private var motion = Motion.none

    override init() {
        super.init()
        background.borderColor = color(0xffffff, 0.12)
        addSublayer(background)
        dot.cornerRadius = 4
        addSublayer(dot)
        icon.contentsGravity = .resizeAspect
        addSublayer(icon)
        text.font = labelFont
        text.fontSize = labelFont.pointSize
        text.alignmentMode = .left
        addSublayer(text)
        for _ in 0..<largeBarCount {
            let bar = CALayer()
            bar.cornerRadius = barWidth / 2
            addSublayer(bar)
            bars.append(bar)
        }
        addSublayer(pauseButton)
        addSublayer(stopButton)
    }
    override init(layer: Any) { super.init(layer: layer) }
    required init?(coder: NSCoder) { fatalError() }

    /// The pill is exactly as big as what it shows, so nothing transparent
    /// around it can intercept clicks meant for the app underneath.
    static func size(for a: PillAppearance) -> CGSize {
        if a.isLarge { return largeSize }
        switch a.state {
        case .idle:
            return a.hover ? CGSize(width: 14 + 13 + 6 + textWidth("Dictate") + 14, height: 28) : handleSize
        case .recording, .paused, .transcribing:
            let width = 14 + 8 + 10 + CGFloat(barCount) * barWidth + CGFloat(barCount - 1) * barGap + 14
            return CGSize(width: width, height: pillHeight)
        case .done, .error:
            return CGSize(width: 14 + 14 + 6 + textWidth(a.label) + 14, height: pillHeight)
        }
    }

    func apply(_ a: PillAppearance) {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        defer { CATransaction.commit() }

        let size = bounds.size
        let midY = size.height / 2
        let isHandle = a.state == .idle && !a.hover
        let large = a.isLarge
        shown = a.state

        background.frame = bounds
        background.cornerRadius = large ? 18 : size.height / 2
        background.backgroundColor = isHandle ? color(Palette.handle, 0.55) : color(Palette.panel, 0.92)
        background.borderWidth = isHandle ? 0 : 1

        // Left slot: recording dot, pause glyph, transcribing dot, or the result / mic glyph.
        let slotY = large ? largeRow.midY : midY
        let slot = CGRect(x: 14 + (large ? 2 : 0), y: slotY - 4, width: 8, height: 8)
        dot.isHidden = !(a.state == .recording || a.state == .transcribing) && !(large && a.state == .paused)
        dot.frame = slot
        dot.backgroundColor = color(
            a.state == .recording ? Palette.recording : a.state == .paused ? Palette.paused : Palette.transcribing)

        let iconSize: CGFloat = a.state == .done || a.state == .error ? 14 : 13
        var iconImage: CGImage?
        switch a.state {
        case .paused where !large: iconImage = glyph("pause.fill", points: 11, hex: Palette.paused)
        case .done: iconImage = glyph("checkmark", points: 11, hex: Palette.done)
        case .error: iconImage = glyph("exclamationmark", points: 11, hex: Palette.warning)
        case .idle where a.hover: iconImage = glyph("mic.fill", points: 11, hex: 0xffffff)
        default: break
        }
        icon.isHidden = iconImage == nil
        icon.contents = iconImage
        icon.contentsScale = contentsScale
        if a.state == .paused {
            icon.frame = CGRect(x: 14 - 1, y: midY - 6, width: 10, height: 12)
        } else {
            icon.frame = CGRect(x: 14, y: midY - iconSize / 2, width: iconSize, height: iconSize)
        }

        let showsText = a.state == .done || a.state == .error || (a.state == .idle && a.hover) || large
        text.isHidden = !showsText
        text.contentsScale = contentsScale
        if large {
            text.string = statusText(a)
            text.frame = CGRect(x: largeRow.minX + 18, y: largeRow.midY - 8, width: 120, height: 16)
        } else if showsText {
            text.string = a.state == .idle ? "Dictate" : a.label
            text.foregroundColor =
                a.state == .done
                ? color(Palette.done) : a.state == .error ? color(Palette.warning) : color(0xffffff, 0.92)
            let x: CGFloat = 14 + iconSize + 6
            text.frame = CGRect(x: x, y: midY - 8, width: size.width - x - 8, height: 16)
        }

        layoutBars(a, size: size)

        let buttons = largeButtons(for: a)
        for (layer, button) in [(pauseButton, PillButton.pauseResume), (stopButton, PillButton.stop)] {
            if let spec = buttons.first(where: { $0.button == button }) {
                layer.isHidden = false
                layer.configure(spec, pressed: a.pressed == button, scale: contentsScale)
            } else {
                layer.isHidden = true
            }
        }
        updateAnimations(for: a)
    }

    /// "Recording  0:07": the state in white, the clock in a quieter tint.
    private func statusText(_ a: PillAppearance) -> NSAttributedString {
        let name = a.state == .recording ? "Recording" : a.state == .paused ? "Paused" : "Transcribing…"
        let out = NSMutableAttributedString(
            string: name, attributes: [.font: labelFont, .foregroundColor: NSColor(white: 1, alpha: 0.92)])
        if a.state != .transcribing {
            out.append(
                NSAttributedString(
                    string: "  " + formatElapsed(a.elapsed),
                    attributes: [.font: timerFont, .foregroundColor: NSColor(white: 1, alpha: 0.55)]))
        }
        return out
    }

    private func layoutBars(_ a: PillAppearance, size: CGSize) {
        let showsBars = a.state == .recording || a.state == .paused || a.state == .transcribing
        let large = a.isLarge
        let count = large ? largeBarCount : barCount
        let total = CGFloat(count) * barWidth + CGFloat(count - 1) * barGap
        let startX = large ? largeWave.minX + (largeWave.width - total) / 2 : size.width - 14 - total
        let midY = large ? largeWave.midY : size.height / 2
        let maxHeight = large ? largeWave.height : pillHeight - 12
        let restingHeight: CGFloat = large ? 14 : 12  // the wave scales this between 0.35x and 1.6x
        // The newest level is the last bar; older ones scroll left.
        let levels = Array(a.levels.suffix(count))
        let missing = count - levels.count
        for (i, bar) in bars.enumerated() {
            bar.isHidden = !showsBars || i >= count
            guard !bar.isHidden else { continue }
            var height: CGFloat = 3
            var alpha: CGFloat = 0.92
            switch a.state {
            case .recording:
                let level = i >= missing ? levels[i - missing] : Level(value: 0, speech: false)
                height = max(3, level.value * maxHeight)
                alpha = level.speech ? 0.95 : 0.5
            case .paused:
                alpha = 0.35
            case .transcribing:
                height = restingHeight
            default: break
            }
            bar.bounds = CGRect(x: 0, y: 0, width: barWidth, height: height)
            bar.position = CGPoint(x: startX + CGFloat(i) * (barWidth + barGap) + barWidth / 2, y: midY)
            bar.backgroundColor =
                a.state == .transcribing ? color(Palette.transcribing) : color(0xffffff, alpha)
        }
    }

    /// Pulse, wave and fade run on the render server: no per-frame work in this process.
    /// With Reduce Motion nothing pulses or travels; transcribing is a slow fade instead.
    private func updateAnimations(for a: PillAppearance) {
        if a.state == .recording && !a.reduceMotion {
            if dot.animation(forKey: "pulse") == nil {
                let pulse = CABasicAnimation(keyPath: "opacity")
                pulse.fromValue = 1
                pulse.toValue = 0.3
                pulse.duration = 0.7
                pulse.autoreverses = true
                pulse.repeatCount = .infinity
                pulse.timingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
                dot.add(pulse, forKey: "pulse")
            }
        } else {
            dot.removeAnimation(forKey: "pulse")
        }

        let wanted: Motion = a.state != .transcribing ? .none : (a.reduceMotion ? .fade : .wave)
        guard wanted != motion else { return }
        for bar in bars {
            bar.removeAnimation(forKey: "wave")
            bar.removeAnimation(forKey: "fade")
        }
        motion = wanted
        let start = CACurrentMediaTime()
        for (i, bar) in bars.enumerated() where wanted != .none {
            if wanted == .wave {
                let wave = CABasicAnimation(keyPath: "transform.scale.y")
                wave.fromValue = 0.35
                wave.toValue = 1.6  // 14 pt * 1.6 stays inside the 36 pt of Large wave room
                wave.duration = 0.45
                wave.autoreverses = true
                wave.repeatCount = .infinity
                wave.beginTime = start + Double(i) * 0.07
                wave.fillMode = .backwards
                wave.timingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
                bar.add(wave, forKey: "wave")
            } else {
                let fade = CABasicAnimation(keyPath: "opacity")
                fade.fromValue = 1
                fade.toValue = 0.35
                fade.duration = 1.6
                fade.autoreverses = true
                fade.repeatCount = .infinity
                fade.timingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
                bar.add(fade, forKey: "fade")
            }
        }
    }

    /// Snapshots cannot show running animations; pose the wave at one instant instead.
    /// With Reduce Motion the bars stay level, which is what the slow fade shows.
    func poseWave(reduceMotion: Bool = false) {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        for (i, bar) in bars.enumerated() {
            let scale = reduceMotion ? 1.0 : 0.35 + 1.25 * (0.5 + 0.5 * sin(Double(i) * 0.75))
            bar.transform = CATransform3DMakeScale(1, CGFloat(scale), 1)
        }
        CATransaction.commit()
    }

    /// A quick press feedback, so a click never feels dead.
    func nudge() {
        let squeeze = CAKeyframeAnimation(keyPath: "transform.scale")
        squeeze.values = [1, 0.94, 1]
        squeeze.keyTimes = [0, 0.4, 1]
        squeeze.duration = 0.16
        add(squeeze, forKey: "nudge")
    }
}

// MARK: - Window

final class PillPanel: NSPanel {
    override var canBecomeKey: Bool { false }
    override var canBecomeMain: Bool { false }
}

/// A VoiceOver stand-in for one of the Large panel's drawn buttons.
final class PillAccessibilityButton: NSAccessibilityElement {
    var onPress: (() -> Void)?
    override func accessibilityPerformPress() -> Bool {
        onPress?()
        return true
    }
}

final class PillView: NSView {
    let pill = PillLayer()
    var onMouseDown: ((NSPoint) -> Void)?
    var onClick: ((NSPoint) -> Void)?
    var onHover: ((Bool) -> Void)?
    var onDragEnd: (() -> Void)?
    var onDragStart: (() -> Void)?
    var onContextMenu: ((NSEvent) -> Void)?
    /// VoiceOver: what the pill is, what pressing it does, and the Large buttons.
    var accessibilityLabelText = ""
    var accessibilityHintText = ""
    var accessibilityButtons: [PillAccessibilityButton] = []
    private var pressScreenPoint = NSPoint.zero
    private var pressOrigin = NSPoint.zero
    private var dragging = false

    override init(frame: NSRect) {
        super.init(frame: frame)
        layer = pill  // set before wantsLayer: this view hosts our layer tree
        wantsLayer = true
        pill.frame = bounds
        addTrackingArea(
            NSTrackingArea(
                rect: .zero, options: [.mouseEnteredAndExited, .activeAlways, .inVisibleRect],
                owner: self, userInfo: nil))
    }
    required init?(coder: NSCoder) { fatalError() }

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }
    override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
        pill.frame = bounds
    }

    override func mouseEntered(with event: NSEvent) { onHover?(true) }
    override func mouseExited(with event: NSEvent) { onHover?(false) }

    override func mouseDown(with event: NSEvent) {
        pressScreenPoint = NSEvent.mouseLocation
        pressOrigin = window?.frame.origin ?? .zero
        dragging = false
        onMouseDown?(convert(event.locationInWindow, from: nil))
    }

    override func mouseDragged(with event: NSEvent) {
        let now = NSEvent.mouseLocation
        let dx = now.x - pressScreenPoint.x
        let dy = now.y - pressScreenPoint.y
        // A press that barely moves is a click, not a drag.
        if !dragging && hypot(dx, dy) < 3 { return }
        if !dragging { onDragStart?() }
        dragging = true
        window?.setFrameOrigin(NSPoint(x: pressOrigin.x + dx, y: pressOrigin.y + dy))
    }

    override func mouseUp(with event: NSEvent) {
        if dragging {
            dragging = false
            onDragEnd?()
        } else {
            onClick?(convert(event.locationInWindow, from: nil))
        }
    }

    override func rightMouseDown(with event: NSEvent) { onContextMenu?(event) }

    // MARK: VoiceOver

    override func isAccessibilityElement() -> Bool { true }
    override func accessibilityRole() -> NSAccessibility.Role? { accessibilityButtons.isEmpty ? .button : .group }
    override func accessibilityLabel() -> String? { accessibilityLabelText }
    override func accessibilityHelp() -> String? { accessibilityHintText.isEmpty ? nil : accessibilityHintText }
    override func accessibilityChildren() -> [Any]? { accessibilityButtons.isEmpty ? nil : accessibilityButtons }
    override func accessibilityPerformPress() -> Bool {
        guard accessibilityButtons.isEmpty else { return false }
        onClick?(NSPoint(x: bounds.midX, y: bounds.midY))
        return true
    }
}

// MARK: - Position

/// Where the pill sits, remembered per display as a fraction of its visible frame.
struct PositionStore {
    let url: URL
    private var displays: [String: [String: Double]] = [:]

    init(url: URL) {
        self.url = url
        if let data = try? Data(contentsOf: url),
            let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let saved = json["displays"] as? [String: [String: Double]]
        {
            displays = saved
        }
    }

    func position(for screen: NSScreen) -> (fx: Double, fy: Double)? {
        guard let p = displays[screen.displayID], let fx = p["fx"], let fy = p["fy"] else { return nil }
        return (fx, fy)
    }

    mutating func save(fx: Double, fy: Double, for screen: NSScreen) {
        displays[screen.displayID] = ["fx": fx, "fy": fy]
        persist()
    }

    /// Forgets where the pill was on this display: it goes back to bottom center.
    mutating func reset(for screen: NSScreen) {
        displays[screen.displayID] = nil
        persist()
    }

    /// The same, by raw display id, for tests that have no screen.
    mutating func setPosition(fx: Double, fy: Double, forDisplay id: String) {
        displays[id] = ["fx": fx, "fy": fy]
        persist()
    }

    func position(forDisplay id: String) -> (fx: Double, fy: Double)? {
        guard let p = displays[id], let fx = p["fx"], let fy = p["fy"] else { return nil }
        return (fx, fy)
    }

    private func persist() {
        try? FileManager.default.createDirectory(
            at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        if let data = try? JSONSerialization.data(
            withJSONObject: ["displays": displays], options: [.prettyPrinted, .sortedKeys])
        {
            try? data.write(to: url, options: .atomic)
        }
    }
}

extension NSScreen {
    /// Stable across reboots and re-plugging, unlike the screen's index.
    var displayID: String {
        guard let number = deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber,
            let uuid = CGDisplayCreateUUIDFromDisplayID(number.uint32Value)?.takeRetainedValue(),
            let text = CFUUIDCreateString(nil, uuid) as String?
        else { return "unknown" }
        return text
    }
}

/// Gap between the Dock (or screen edge) and the default bottom-center pill.
let defaultBottomMargin: CGFloat = 12

func screenUnderPointer() -> NSScreen? {
    let mouse = NSEvent.mouseLocation
    return NSScreen.screens.first { NSMouseInRect(mouse, $0.frame, false) } ?? NSScreen.main
}

// MARK: - Daemon connection

func socketURL() -> URL {
    let home = ProcessInfo.processInfo.environment["HOME"] ?? NSHomeDirectory()
    return URL(fileURLWithPath: home).appendingPathComponent(".config/bolo/bolo.sock")
}

func dataURL(_ name: String) -> URL {
    let home = ProcessInfo.processInfo.environment["HOME"] ?? NSHomeDirectory()
    return URL(fileURLWithPath: home).appendingPathComponent(".local/share/bolo/\(name)")
}

func connectToDaemon() -> Int32? {
    let fd = socket(AF_UNIX, SOCK_STREAM, 0)
    guard fd >= 0 else { return nil }
    var one: Int32 = 1
    setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
    var address = sockaddr_un()
    address.sun_family = sa_family_t(AF_UNIX)
    let path = Array(socketURL().path.utf8CString)
    guard path.count <= MemoryLayout.size(ofValue: address.sun_path) else {
        close(fd)
        return nil
    }
    withUnsafeMutableBytes(of: &address.sun_path) { raw in
        for (i, byte) in path.enumerated() { raw[i] = UInt8(bitPattern: byte) }
    }
    let result = withUnsafePointer(to: &address) {
        $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
            connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
        }
    }
    if result != 0 {
        close(fd)
        return nil
    }
    return fd
}

func writeAll(_ fd: Int32, _ text: String) -> Bool {
    let bytes = Array(text.utf8)
    var sent = 0
    while sent < bytes.count {
        let n = bytes[sent...].withUnsafeBytes { write(fd, $0.baseAddress, $0.count) }
        if n <= 0 { return false }
        sent += n
    }
    return true
}

/// Sends one command on its own short connection (the daemon answers one line per connection).
func sendCommand(_ command: String) -> String? {
    guard let fd = connectToDaemon() else { return nil }
    defer { close(fd) }
    var timeout = timeval(tv_sec: 2, tv_usec: 0)
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
    guard writeAll(fd, command + "\n") else { return nil }
    var buffer = [UInt8](repeating: 0, count: 256)
    let n = read(fd, &buffer, buffer.count)
    return n > 0 ? String(decoding: buffer[..<n], as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines) : ""
}


// MARK: - Menu

/// What a right-click menu item does.
enum MenuAction: Equatable {
    case style(PillStyle)
    case showWhenIdle(Bool)
    case resetPosition
    case openDashboard
}

struct MenuEntry {
    var title: String
    var checked = false
    var action: MenuAction
}

/// The right-click menu, as data: nil is a separator.
func menuEntries(style: PillStyle, showIdle: Bool) -> [MenuEntry?] {
    [
        MenuEntry(title: "Small", checked: style == .small, action: .style(.small)),
        MenuEntry(title: "Large", checked: style == .large, action: .style(.large)),
        MenuEntry(title: "Hidden", checked: style == .hidden, action: .style(.hidden)),
        nil,
        MenuEntry(title: "Show when idle", checked: showIdle, action: .showWhenIdle(!showIdle)),
        MenuEntry(title: "Reset position", action: .resetPosition),
        nil,
        MenuEntry(title: "Open Bolo dashboard", action: .openDashboard),
    ]
}

final class ActionItem: NSMenuItem {
    private let handler: () -> Void

    init(_ title: String, checked: Bool, handler: @escaping () -> Void) {
        self.handler = handler
        super.init(title: title, action: #selector(fire), keyEquivalent: "")
        target = self
        state = checked ? .on : .off
    }
    required init(coder: NSCoder) { fatalError() }

    @objc func fire() { handler() }
}

/// The line VoiceOver speaks when the pill changes state; nil for idle.
func announcement(for a: PillAppearance) -> String? {
    switch a.state {
    case .idle: return nil
    case .recording: return "Bolo recording"
    case .paused: return "Bolo paused"
    case .transcribing: return "Bolo transcribing"
    case .done, .error: return "Bolo, \(a.label)"
    }
}

/// `bolo` next to this binary (how install.sh lays them out), else on PATH.
func runBolo(_ arguments: [String]) {
    let process = Process()
    let sibling = Bundle.main.executableURL?.deletingLastPathComponent().appendingPathComponent("bolo")
    if let sibling = sibling, FileManager.default.isExecutableFile(atPath: sibling.path) {
        process.executableURL = sibling
        process.arguments = arguments
    } else {
        process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        process.arguments = ["bolo"] + arguments
    }
    process.standardOutput = FileHandle.nullDevice
    process.standardError = FileHandle.nullDevice
    try? process.run()
}

// MARK: - Controller

final class PillController {
    private let panel: PillPanel
    private let view: PillView
    private var positions = PositionStore(url: dataURL("pill.json"))

    private var daemonPhase = PillState.idle
    private var hold: (state: PillState, label: String)?
    private var holdTimer: Timer?
    private var levels = [Level](repeating: Level(value: 0, speech: false), count: largeBarCount)
    private var hover = false {
        didSet { hoverWatch(hover) }
    }
    private var hoverTimer: Timer?
    private var wasIdle = false
    private var style = PillStyle.small
    private var showIdle = true
    private var reduceMotion = NSWorkspace.shared.accessibilityDisplayShouldReduceMotion
    private var pressedButton: PillButton?

    /// Recording time for the Large panel: time banked before the last pause
    /// plus the running segment.
    private var elapsedBase: TimeInterval = 0
    private var segmentStart: Date?
    private var clock: Timer?

    private var announcedState: PillState?
    private var accessibilityKey = ""

    /// Bottom-center of the pill in screen coordinates; the pill grows upward from it.
    private var anchor = NSPoint.zero
    private var screen: NSScreen?
    private var shownSize = CGSize.zero

    init() {
        let size = handleSize
        panel = PillPanel(
            contentRect: NSRect(origin: .zero, size: size),
            styleMask: [.borderless, .nonactivatingPanel], backing: .buffered, defer: false)
        // isFloatingPanel resets the level to .floating, so it must be set before `level`.
        panel.isFloatingPanel = true
        panel.level = .screenSaver  // above full-screen apps (Apple DTS)
        var behavior: NSWindow.CollectionBehavior = [
            .canJoinAllSpaces, .fullScreenAuxiliary, .stationary, .ignoresCycle,
        ]
        if #available(macOS 13.0, *) { behavior.insert(.canJoinAllApplications) }
        panel.collectionBehavior = behavior
        panel.hidesOnDeactivate = false
        panel.becomesKeyOnlyIfNeeded = true
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = true
        panel.animationBehavior = .none
        panel.isReleasedWhenClosed = false
        // Keep the pill out of screenshots and screen sharing, including Bolo's own
        // screen-context capture (`screencapture -x`).
        panel.sharingType = .none
        view = PillView(frame: NSRect(origin: .zero, size: size))
        view.pill.contentsScale = NSScreen.main?.backingScaleFactor ?? 2
        panel.contentView = view

        view.onMouseDown = { [weak self] point in self?.pressed(at: point) }
        view.onClick = { [weak self] point in self?.clicked(at: point) }
        view.onHover = { [weak self] inside in
            self?.hover = inside
            self?.render()
        }
        view.onDragStart = { [weak self] in
            self?.pressedButton = nil
            self?.render()
        }
        view.onDragEnd = { [weak self] in self?.dragged() }
        view.onContextMenu = { [weak self] event in self?.showMenu(event) }
        NotificationCenter.default.addObserver(
            forName: NSApplication.didChangeScreenParametersNotification, object: nil, queue: .main
        ) { [weak self] _ in self?.screensChanged() }
        NSWorkspace.shared.notificationCenter.addObserver(
            forName: NSWorkspace.accessibilityDisplayOptionsDidChangeNotification, object: nil, queue: .main
        ) { [weak self] _ in
            self?.reduceMotion = NSWorkspace.shared.accessibilityDisplayShouldReduceMotion
            self?.render()
        }
        place(on: screenUnderPointer())
    }

    // MARK: events

    func handle(_ event: [String: Any]) {
        switch event["type"] as? String {
        case "hello":
            if let v = event["v"] as? Int, v != 1 {
                FileHandle.standardError.write(Data("[bolo-pill] unknown protocol v\(v)\n".utf8))
            }
            applyConfig(event)
            setPhase(event["phase"] as? String ?? "idle")
        case "config":
            applyConfig(event)
            // The size can change under a still pointer: the frame follows on render.
            render()
        case "phase":
            setPhase(event["phase"] as? String ?? "idle")
        case "level":
            guard daemonPhase == .recording else { return }
            let raw = CGFloat((event["rms"] as? Double) ?? 0)
            let previous = levels.last?.value ?? 0
            levels.removeFirst()
            levels.append(Level(value: smoothed(previous: previous, next: raw), speech: (event["speech"] as? Bool) ?? false))
            render()
        case "outcome":
            let result = outcomeAppearance(
                kind: event["kind"] as? String ?? "", detail: event["detail"] as? String ?? "")
            hold = result
            holdTimer?.invalidate()
            holdTimer = Timer.scheduledTimer(withTimeInterval: holdSeconds(result.state), repeats: false) {
                [weak self] _ in
                self?.hold = nil
                self?.render()
            }
            render()
        default:
            break
        }
    }

    /// `style` and `show_idle`, from `hello` and from live `config` events.
    private func applyConfig(_ event: [String: Any]) {
        style = PillStyle(rawValue: event["style"] as? String ?? "") ?? .small
        showIdle = (event["show_idle"] as? Bool) ?? true
    }

    private func setPhase(_ name: String) {
        let previous = daemonPhase
        switch name {
        case "recording": daemonPhase = .recording
        case "paused": daemonPhase = .paused
        case "processing": daemonPhase = .transcribing
        default: daemonPhase = .idle
        }
        let now = Date()
        if daemonPhase == .recording && previous != .paused {
            // A new dictation: drop any lingering result, start with a flat meter and a
            // zero clock, and show up on the screen the pointer is on.
            hold = nil
            holdTimer?.invalidate()
            levels = [Level](repeating: Level(value: 0, speech: false), count: largeBarCount)
            elapsedBase = 0
            segmentStart = now
            place(on: screenUnderPointer())
        } else if daemonPhase == .recording {
            segmentStart = now  // resumed
        } else if let started = segmentStart {
            elapsedBase += now.timeIntervalSince(started)  // paused, stopped or finished
            segmentStart = nil
        }
        render()
    }

    private func elapsed() -> TimeInterval {
        elapsedBase + (segmentStart.map { Date().timeIntervalSince($0) } ?? 0)
    }

    // MARK: drawing

    private func appearance() -> PillAppearance? {
        guard style != .hidden else { return nil }
        var a: PillAppearance
        if let hold = hold {
            a = PillAppearance(state: hold.state, label: hold.label)
        } else if daemonPhase == .idle {
            guard showIdle else { return nil }
            a = PillAppearance(state: .idle, hover: hover)
        } else {
            a = PillAppearance(state: daemonPhase, levels: levels)
        }
        a.style = style
        a.elapsed = elapsed()
        a.reduceMotion = reduceMotion
        a.pressed = pressedButton
        return a
    }

    /// Enter/exit events are not enough: the pill resizes under a still pointer, and
    /// the pointer can jump away while it is not idle. So hover is re-derived from
    /// where the pointer really is whenever the pill settles back to idle.
    private func syncHover() {
        let idleNow = hold == nil && daemonPhase == .idle
        if !idleNow {
            hover = false
        } else if !wasIdle {
            hover = NSMouseInRect(NSEvent.mouseLocation, frame(for: handleSize), false)
        }
        wasIdle = idleNow
    }

    /// While the handle is grown, check now and then that the pointer is still on it.
    private func hoverWatch(_ on: Bool) {
        hoverTimer?.invalidate()
        hoverTimer = nil
        guard on else { return }
        hoverTimer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
            guard let self = self else { return }
            if !NSMouseInRect(NSEvent.mouseLocation, self.panel.frame, false) {
                self.hover = false
                self.render()
            }
        }
    }

    /// The Large panel shows a running clock: redraw a few times a second while it counts.
    private func updateClock(_ running: Bool) {
        if running && clock == nil {
            let timer = Timer(timeInterval: 0.25, repeats: true) { [weak self] _ in self?.render() }
            RunLoop.main.add(timer, forMode: .common)
            clock = timer
        } else if !running {
            clock?.invalidate()
            clock = nil
        }
    }

    private func render() {
        syncHover()
        guard let a = appearance() else {
            panel.orderOut(nil)
            shownSize = .zero
            announcedState = nil
            updateClock(false)
            return
        }
        updateClock(a.isLarge && a.state == .recording)
        let size = PillLayer.size(for: a)
        if size != shownSize {
            shownSize = size
            panel.setFrame(frame(for: size), display: false)
        }
        view.pill.apply(a)
        updateAccessibility(a)
        if !panel.isVisible { panel.orderFrontRegardless() }
    }

    // MARK: accessibility

    private func updateAccessibility(_ a: PillAppearance) {
        let text = accessibilityText(for: a)
        view.accessibilityLabelText = text.label
        view.accessibilityHintText = text.hint
        // The Large buttons are drawn, not controls: give VoiceOver real ones, rebuilt
        // only when what they say changes.
        let specs = largeButtons(for: a)
        let key = specs.map { $0.title }.joined(separator: "|")
        if key != accessibilityKey {
            accessibilityKey = key
            view.accessibilityButtons = specs.map { spec in
                let element = PillAccessibilityButton()
                element.setAccessibilityRole(.button)
                element.setAccessibilityLabel(spec.title)
                element.setAccessibilityParent(view)
                element.setAccessibilityFrameInParentSpace(spec.rect)
                element.onPress = { [weak self] in self?.perform(spec.button) }
                return element
            }
        }
        if a.state != announcedState {
            announcedState = a.state
            if NSWorkspace.shared.isVoiceOverEnabled, let line = announcement(for: a) {
                NSAccessibility.post(
                    element: NSApp as Any, notification: .announcementRequested,
                    userInfo: [
                        .announcement: line,
                        .priority: NSAccessibilityPriorityLevel.high.rawValue,
                    ])
            }
        }
    }

    // MARK: position

    private func frame(for size: CGSize) -> NSRect {
        var rect = NSRect(x: anchor.x - size.width / 2, y: anchor.y, width: size.width, height: size.height)
        if let vf = screen?.visibleFrame {
            rect.origin.x = min(max(rect.origin.x, vf.minX), max(vf.minX, vf.maxX - rect.width))
            rect.origin.y = min(max(rect.origin.y, vf.minY), max(vf.minY, vf.maxY - rect.height))
        }
        return rect
    }

    /// Puts the pill on `target` at its remembered spot (bottom center by default).
    private func place(on target: NSScreen?) {
        guard let target = target else { return }
        screen = target
        let vf = target.visibleFrame
        if let saved = positions.position(for: target) {
            anchor = NSPoint(x: vf.minX + CGFloat(saved.fx) * vf.width, y: vf.minY + CGFloat(saved.fy) * vf.height)
        } else {
            anchor = NSPoint(x: vf.midX, y: vf.minY + defaultBottomMargin)
        }
        shownSize = .zero  // force the frame to follow
        if panel.isVisible { render() }
    }

    private func dragged() {
        pressedButton = nil
        let frame = panel.frame
        let center = NSPoint(x: frame.midX, y: frame.midY)
        // The screen the pill was dropped on becomes its screen.
        if let target = NSScreen.screens.first(where: { NSMouseInRect(center, $0.frame, false) }) {
            screen = target
        }
        guard let target = screen else { return }
        anchor = NSPoint(x: frame.midX, y: frame.minY)
        let vf = target.visibleFrame
        positions.save(
            fx: Double((anchor.x - vf.minX) / vf.width), fy: Double((anchor.y - vf.minY) / vf.height),
            for: target)
    }

    private func screensChanged() {
        // A display went away or changed size: keep the pill on a screen that exists.
        let current = screen.flatMap { s in NSScreen.screens.first { $0.displayID == s.displayID } }
        place(on: current ?? screenUnderPointer())
    }

    // MARK: input

    /// "Not now" feedback: the window itself wiggles (a layer inside it would be
    /// clipped by the window, which is exactly as big as the pill). Reduce Motion
    /// skips it.
    private func shake() {
        guard !reduceMotion else { return }
        let home = panel.frame.origin
        for (i, dx) in [-4.0, 4.0, -3.0, 3.0, 0.0].enumerated() {
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.045 * Double(i)) { [weak self] in
                guard let self = self, self.panel.isVisible, self.shownSize == self.panel.frame.size else { return }
                self.panel.setFrameOrigin(NSPoint(x: home.x + CGFloat(dx), y: home.y))
            }
        }
    }

    private func pressed(at point: NSPoint) {
        pressedButton = appearance().flatMap { buttonHit(point, in: $0) }
        if pressedButton == nil && !reduceMotion { view.pill.nudge() }
        render()
    }

    private func clicked(at point: NSPoint) {
        let pressed = pressedButton
        pressedButton = nil
        defer { render() }
        if let pressed = pressed {
            // A button fires when the mouse comes up on the button it went down on.
            if let a = appearance(), buttonHit(point, in: a) == pressed { perform(pressed) }
            return
        }
        bodyClicked()
    }

    private func perform(_ button: PillButton) {
        send(button == .stop ? "toggle" : "pause")
    }

    private func bodyClicked() {
        if let result = hold {
            // A result is on screen. An error opens the dashboard's history; a success has
            // nothing to act on.
            if result.state == .error {
                holdTimer?.invalidate()
                hold = nil
                runBolo(["history"])
            }
            return
        }
        // The Large panel starts from its idle handle but is run with its buttons.
        if style == .large && daemonPhase != .idle { return }
        switch daemonPhase {
        case .idle, .recording: send("toggle")
        case .paused: send("pause")  // resume
        default: shake()  // transcribing: nothing to do yet
        }
    }

    private func send(_ command: String) {
        DispatchQueue.global().async { [weak self] in
            let reply = sendCommand(command)
            DispatchQueue.main.async {
                if let reply = reply, reply.hasPrefix("busy") || reply.hasPrefix("err") {
                    self?.shake()
                }
            }
        }
    }

    // MARK: menu

    private func showMenu(_ event: NSEvent) {
        let menu = NSMenu()
        menu.autoenablesItems = false
        for entry in menuEntries(style: style, showIdle: showIdle) {
            guard let entry = entry else {
                menu.addItem(.separator())
                continue
            }
            menu.addItem(ActionItem(entry.title, checked: entry.checked) { [weak self] in self?.run(entry.action) })
        }
        NSMenu.popUpContextMenu(menu, with: event, for: view)
    }

    private func run(_ action: MenuAction) {
        switch action {
        case .style(let style): send("pill-style \(style.rawValue)")
        case .showWhenIdle(let on): send("pill-idle \(on ? "on" : "off")")
        case .resetPosition:
            if let screen = screen {
                positions.reset(for: screen)
                place(on: screen)
            }
        case .openDashboard: runBolo(["settings"])
        }
    }
}

// MARK: - Snapshots

/// One state of one style, drawn offscreen at `scale`.
func renderImage(_ appearance: PillAppearance, scale: CGFloat) -> (image: CGImage, size: CGSize)? {
    let size = PillLayer.size(for: appearance)
    let layer = PillLayer()
    layer.contentsScale = scale
    layer.frame = CGRect(origin: .zero, size: size)
    layer.apply(appearance)
    if appearance.state == .transcribing { layer.poseWave(reduceMotion: appearance.reduceMotion) }
    guard
        let ctx = CGContext(
            data: nil, width: Int(size.width * scale), height: Int(size.height * scale),
            bitsPerComponent: 8, bytesPerRow: 0, space: CGColorSpace(name: CGColorSpace.sRGB)!,
            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
    else { return nil }
    ctx.scaleBy(x: scale, y: scale)
    layer.render(in: ctx)
    guard let image = ctx.makeImage() else { return nil }
    return (image, size)
}

/// A believable meter history for the Large waveform.
func sampleLevels(_ count: Int, speech: Bool, loudness: Double) -> [Level] {
    (0..<count).map { i in
        let swell = 0.5 + 0.5 * sin(Double(i) * 0.37) * cos(Double(i) * 0.11)
        let value = max(0.03, min(1, loudness * (0.25 + 0.75 * abs(swell)) + 0.04 * sin(Double(i) * 2.3)))
        return Level(value: CGFloat(value), speech: speech)
    }
}

/// Renders every state of every style to PNG without showing a window.
func renderSnapshots(to directory: String) {
    let scale: CGFloat = 3
    let speaking = [0.15, 0.3, 0.55, 0.8, 0.95, 0.7, 0.45, 0.65, 0.4, 0.25, 0.12].map { Level(value: $0, speech: true) }
    let quiet = [0.05, 0.06, 0.04, 0.08, 0.05, 0.07, 0.04, 0.06, 0.05, 0.04, 0.05].map { Level(value: $0, speech: false) }
    let flat = [Level](repeating: Level(value: 0, speech: false), count: barCount)
    let flatLarge = [Level](repeating: Level(value: 0, speech: false), count: largeBarCount)
    func large(_ state: PillState, levels: [Level] = [], elapsed: TimeInterval = 0, reduce: Bool = false, pressed: PillButton? = nil)
        -> PillAppearance
    {
        PillAppearance(state: state, style: .large, levels: levels, elapsed: elapsed, reduceMotion: reduce, pressed: pressed)
    }
    let cases: [(String, PillAppearance)] = [
        ("idle", PillAppearance(state: .idle)),
        ("idle-hover", PillAppearance(state: .idle, hover: true)),
        ("recording-speech", PillAppearance(state: .recording, levels: speaking)),
        ("recording-quiet", PillAppearance(state: .recording, levels: quiet)),
        ("recording-start", PillAppearance(state: .recording, levels: flat)),
        ("paused", PillAppearance(state: .paused)),
        ("transcribing", PillAppearance(state: .transcribing)),
        ("transcribing-reduced-motion", PillAppearance(state: .transcribing, reduceMotion: true)),
        ("done-pasted", PillAppearance(state: .done, label: "Pasted")),
        ("done-copied", PillAppearance(state: .done, label: "Copied")),
        ("error-no-speech", PillAppearance(state: .error, label: "No speech")),
        ("error-mic-unavailable", PillAppearance(state: .error, label: "Mic unavailable")),
        ("error-paste-stopped", PillAppearance(state: .error, label: "Paste stopped")),
        ("error-max-length", PillAppearance(state: .error, label: "Max length")),
        ("large-recording-speech", large(.recording, levels: sampleLevels(largeBarCount, speech: true, loudness: 0.9), elapsed: 7)),
        ("large-recording-quiet", large(.recording, levels: sampleLevels(largeBarCount, speech: false, loudness: 0.12), elapsed: 74)),
        ("large-recording-start", large(.recording, levels: flatLarge, elapsed: 0)),
        ("large-recording-long", large(.recording, levels: sampleLevels(largeBarCount, speech: true, loudness: 0.7), elapsed: 3723)),
        ("large-stop-pressed", large(.recording, levels: sampleLevels(largeBarCount, speech: true, loudness: 0.7), elapsed: 21, pressed: .stop)),
        ("large-paused", large(.paused, elapsed: 12)),
        ("large-paused-resume-pressed", large(.paused, elapsed: 12, pressed: .pauseResume)),
        ("large-transcribing", large(.transcribing)),
        ("large-transcribing-reduced-motion", large(.transcribing, reduce: true)),
        ("large-done-pasted", PillAppearance(state: .done, style: .large, label: "Pasted")),
        ("large-error-no-speech", PillAppearance(state: .error, style: .large, label: "No speech")),
    ]
    try? FileManager.default.createDirectory(atPath: directory, withIntermediateDirectories: true)
    let pad: CGFloat = 12
    var images: [(String, CGImage, CGSize)] = []
    for (name, appearance) in cases {
        guard let (image, size) = renderImage(appearance, scale: scale) else { continue }
        let url = URL(fileURLWithPath: directory).appendingPathComponent("pill-\(name).png")
        try? NSBitmapImageRep(cgImage: image).representation(using: .png, properties: [:])?.write(to: url)
        print(url.path)
        images.append((name, image, size))
    }

    // One sheet with every state on a dark and a light backdrop.
    let rowHeights = images.map { $0.2.height + pad * 2 }
    let columnWidth: CGFloat = 560
    let sheetSize = CGSize(width: columnWidth * 2, height: rowHeights.reduce(0, +))
    guard
        let sheet = CGContext(
            data: nil, width: Int(sheetSize.width * scale), height: Int(sheetSize.height * scale),
            bitsPerComponent: 8, bytesPerRow: 0, space: CGColorSpace(name: CGColorSpace.sRGB)!,
            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
    else { exit(1) }
    sheet.scaleBy(x: scale, y: scale)
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(cgContext: sheet, flipped: false)
    for (column, backdrop) in [(0, 0x1b1c22 as UInt32), (1, 0xe9ebf0)] {
        sheet.setFillColor(color(backdrop))
        sheet.fill(CGRect(x: CGFloat(column) * columnWidth, y: 0, width: columnWidth, height: sheetSize.height))
    }
    let caption: [NSAttributedString.Key: Any] = [
        .font: NSFont.monospacedSystemFont(ofSize: 10, weight: .regular), .foregroundColor: NSColor.gray,
    ]
    var top = sheetSize.height
    for (row, entry) in images.enumerated() {
        let y = top - rowHeights[row]
        for column in 0..<2 {
            let x = CGFloat(column) * columnWidth
            (entry.0 as NSString).draw(at: CGPoint(x: x + 10, y: y + rowHeights[row] / 2 - 6), withAttributes: caption)
            sheet.draw(entry.1, in: CGRect(x: x + 230, y: y + pad, width: entry.2.width, height: entry.2.height))
        }
        top = y
    }
    NSGraphicsContext.restoreGraphicsState()
    if let image = sheet.makeImage() {
        let url = URL(fileURLWithPath: directory).appendingPathComponent("pill-states.png")
        try? NSBitmapImageRep(cgImage: image).representation(using: .png, properties: [:])?.write(to: url)
        print(url.path)
    }
}

// MARK: - Self test

/// `bolo-pill --selftest`: the helper's pure logic and layer tree, with no window and
/// no daemon. Exits 1 on the first run with any failure.
func runSelfTest() -> Bool {
    var failures: [String] = []
    func expect(_ ok: Bool, _ what: String) {
        print((ok ? "  ok    " : "  FAIL  ") + what)
        if !ok { failures.append(what) }
    }
    func rect(_ r: CGRect, inside outer: CGRect) -> Bool { outer.insetBy(dx: -0.01, dy: -0.01).contains(r) }
    let speaking = sampleLevels(largeBarCount, speech: true, loudness: 0.9)

    // Sizes: Large only for the live states; handle and results stay compact.
    let largeRecording = PillAppearance(state: .recording, style: .large, levels: speaking, elapsed: 7)
    expect(PillLayer.size(for: largeRecording) == largeSize, "Large recording is \(Int(largeSize.width))x\(Int(largeSize.height))")
    expect(PillLayer.size(for: PillAppearance(state: .paused, style: .large)) == largeSize, "Large paused is the big panel")
    expect(PillLayer.size(for: PillAppearance(state: .transcribing, style: .large)) == largeSize, "Large transcribing is the big panel")
    expect(PillLayer.size(for: PillAppearance(state: .idle, style: .large)) == handleSize, "Large idle is the 44x8 handle")
    expect(
        PillLayer.size(for: PillAppearance(state: .done, style: .large, label: "Pasted")).height == pillHeight,
        "Large results are the compact capsule")
    expect(
        PillLayer.size(for: PillAppearance(state: .recording, style: .small)).height == pillHeight,
        "Small recording is the compact capsule")

    // Buttons: Pause/Resume and Stop, only in Large and not while transcribing.
    let buttons = largeButtons(for: largeRecording)
    expect(buttons.map { $0.title } == ["Pause", "Stop"], "Large recording has Pause and Stop")
    expect(
        largeButtons(for: PillAppearance(state: .paused, style: .large)).map { $0.title } == ["Resume", "Stop"],
        "Large paused has Resume and Stop")
    expect(largeButtons(for: PillAppearance(state: .transcribing, style: .large)).isEmpty, "no buttons while transcribing")
    expect(largeButtons(for: PillAppearance(state: .recording, style: .small)).isEmpty, "Small has no buttons")
    let panelBounds = CGRect(origin: .zero, size: largeSize)
    expect(buttons.allSatisfy { rect($0.rect, inside: panelBounds) }, "buttons sit inside the panel")
    expect(!buttons[0].rect.intersects(buttons[1].rect), "buttons do not overlap")
    expect(
        buttonHit(CGPoint(x: buttons[1].rect.midX, y: buttons[1].rect.midY), in: largeRecording) == .stop,
        "a click on Stop hits Stop")
    expect(
        buttonHit(CGPoint(x: buttons[0].rect.midX, y: buttons[0].rect.midY), in: largeRecording) == .pauseResume,
        "a click on Pause hits Pause")
    expect(buttonHit(CGPoint(x: 100, y: 60), in: largeRecording) == nil, "a click on the waveform hits no button")
    expect(
        buttonHit(CGPoint(x: buttons[1].rect.midX, y: buttons[1].rect.midY), in: PillAppearance(state: .recording)) == nil,
        "Small never hits a button")

    // The clock.
    expect(formatElapsed(7) == "0:07", "7 s reads 0:07")
    expect(formatElapsed(754) == "12:34", "754 s reads 12:34")
    expect(formatElapsed(3723) == "1:02:03", "3723 s reads 1:02:03")
    expect(formatElapsed(-4) == "0:00", "negative time reads 0:00")

    // VoiceOver.
    let every: [(PillAppearance, String)] = [
        (PillAppearance(state: .idle), "Bolo, ready to dictate"),
        (PillAppearance(state: .recording), "Bolo recording"),
        (PillAppearance(state: .paused), "Bolo paused"),
        (PillAppearance(state: .transcribing), "Bolo transcribing"),
        (PillAppearance(state: .done, label: "Pasted"), "Bolo, Pasted"),
        (PillAppearance(state: .error, label: "No speech"), "Bolo, No speech"),
        (largeRecording, "Bolo recording, 0:07"),
        (PillAppearance(state: .paused, style: .large, elapsed: 65), "Bolo paused, 1:05"),
    ]
    for (appearance, label) in every {
        expect(accessibilityText(for: appearance).label == label, "VoiceOver label \"\(label)\"")
    }
    expect(accessibilityText(for: PillAppearance(state: .error, label: "Mic unavailable")).hint.contains("dashboard"), "an error says a click opens the dashboard")
    expect(accessibilityText(for: PillAppearance(state: .recording)).hint.contains("stop"), "Small recording says a click stops")
    expect(announcement(for: PillAppearance(state: .idle)) == nil, "idle is not announced")
    expect(announcement(for: PillAppearance(state: .transcribing)) == "Bolo transcribing", "transcribing is announced")

    // The right-click menu.
    let menu = menuEntries(style: .large, showIdle: true)
    let titles = menu.map { $0?.title ?? "-" }
    expect(
        titles == ["Small", "Large", "Hidden", "-", "Show when idle", "Reset position", "-", "Open Bolo dashboard"],
        "menu is Small, Large, Hidden, Show when idle, Reset position, Open Bolo dashboard")
    expect(menu.compactMap { $0 }.filter { $0.checked }.map { $0.title } == ["Large", "Show when idle"], "the menu ticks the current style and idle handle")
    expect(menuEntries(style: .hidden, showIdle: false).compactMap { $0 }.filter { $0.checked }.map { $0.title } == ["Hidden"], "Hidden and no idle handle tick only Hidden")
    expect(menu.compactMap { $0 }.first { $0.title == "Show when idle" }?.action == .showWhenIdle(false), "choosing a ticked Show when idle turns it off")
    expect(menuEntries(style: .small, showIdle: false).compactMap { $0 }.first { $0.title == "Show when idle" }?.action == .showWhenIdle(true), "choosing an unticked Show when idle turns it on")

    // Results and the meter.
    expect(outcomeAppearance(kind: "done", detail: "copied").label == "Copied", "copied result says Copied")
    expect(outcomeAppearance(kind: "no-speech", detail: "").state == .error, "no speech is an error result")
    expect(smoothed(previous: 0, next: 1) > 1 - smoothed(previous: 1, next: 0), "the meter attacks faster than it releases")
    expect(holdSeconds(.done) < holdSeconds(.error), "errors stay longer than successes")

    // The layer tree.
    func layer(_ a: PillAppearance) -> PillLayer {
        let l = PillLayer()
        l.contentsScale = 2
        l.frame = CGRect(origin: .zero, size: PillLayer.size(for: a))
        l.apply(a)
        return l
    }
    let rec = layer(largeRecording)
    let visibleBars = rec.bars.filter { !$0.isHidden }
    expect(visibleBars.count == largeBarCount, "Large shows \(largeBarCount) bars")
    expect(visibleBars.allSatisfy { rect($0.frame, inside: largeWave) }, "every Large bar stays inside the waveform area")
    expect(!rec.pauseButton.isHidden && !rec.stopButton.isHidden, "Large recording draws both buttons")
    expect(layer(PillAppearance(state: .recording, levels: speaking)).bars.filter { !$0.isHidden }.count == barCount, "Small shows \(barCount) bars")
    expect(layer(PillAppearance(state: .recording, levels: speaking)).stopButton.isHidden, "Small draws no buttons")
    expect(rec.dot.animation(forKey: "pulse") != nil, "the recording dot pulses")
    let calm = layer(PillAppearance(state: .recording, style: .large, levels: speaking, reduceMotion: true))
    expect(calm.dot.animation(forKey: "pulse") == nil, "Reduce Motion: the recording dot does not pulse")
    let waving = layer(PillAppearance(state: .transcribing, style: .large))
    expect(waving.bars[0].animation(forKey: "wave") != nil && waving.bars[0].animation(forKey: "fade") == nil, "transcribing runs a travelling wave")
    let fading = layer(PillAppearance(state: .transcribing, style: .large, reduceMotion: true))
    expect(fading.bars[0].animation(forKey: "wave") == nil && fading.bars[0].animation(forKey: "fade") != nil, "Reduce Motion: transcribing is a slow fade, not a wave")
    let waveRoom = (waving.bars[0].bounds.height * 1.6) <= largeWave.height
    expect(waveRoom, "the Large wave at full swing stays inside the panel")
    let smallWave = layer(PillAppearance(state: .transcribing))
    expect(smallWave.bars[0].bounds.height * 1.6 <= pillHeight - 8, "the Small wave at full swing stays inside the capsule")
    // Switching state removes what the old state ran.
    waving.apply(PillAppearance(state: .done, style: .large, label: "Pasted"))
    expect(waving.bars.allSatisfy { $0.animation(forKey: "wave") == nil }, "leaving transcribing stops the wave")

    // Remembered position.
    let path = NSTemporaryDirectory() + "bolo-pill-selftest-\(getpid()).json"
    var store = PositionStore(url: URL(fileURLWithPath: path))
    store.setPosition(fx: 0.25, fy: 0.5, forDisplay: "DISPLAY-A")
    let reloaded = PositionStore(url: URL(fileURLWithPath: path))
    expect(reloaded.position(forDisplay: "DISPLAY-A")?.fx == 0.25, "a dragged position survives a restart")
    expect(reloaded.position(forDisplay: "DISPLAY-B") == nil, "positions are per display")
    try? FileManager.default.removeItem(atPath: path)

    print(failures.isEmpty ? "selftest passed" : "selftest: \(failures.count) failure(s)")
    return failures.isEmpty
}

// MARK: - Main

let arguments = CommandLine.arguments
let application = NSApplication.shared
application.setActivationPolicy(.accessory)  // no Dock icon, no menu bar, never frontmost

if arguments.count >= 2 && arguments[1] == "--selftest" {
    DispatchQueue.main.async { exit(runSelfTest() ? 0 : 1) }
    application.run()
} else if arguments.count >= 2 && arguments[1] == "--snapshot" {
    DispatchQueue.main.async {
        renderSnapshots(to: arguments.count > 2 ? arguments[2] : ".")
        exit(0)
    }
    application.run()
} else if arguments.count >= 2 {
    print("usage: bolo-pill [--snapshot <dir> | --selftest]")
    exit(arguments[1] == "--help" || arguments[1] == "-h" ? 0 : 2)
}

var controller: PillController?
DispatchQueue.main.async {
    controller = PillController()
}

// Read the event stream on a background thread; give up when the daemon is gone.
Thread.detachNewThread {
    var fd: Int32?
    for _ in 0..<40 {
        fd = connectToDaemon()
        if fd != nil { break }
        Thread.sleep(forTimeInterval: 0.25)
    }
    guard let fd = fd, writeAll(fd, "subscribe pill\n") else {
        FileHandle.standardError.write(Data("[bolo-pill] no daemon at \(socketURL().path)\n".utf8))
        exit(1)
    }
    var pending = Data()
    var buffer = [UInt8](repeating: 0, count: 4096)
    while true {
        let n = read(fd, &buffer, buffer.count)
        if n <= 0 { exit(0) }  // the daemon closed the stream: no orphan pill
        pending.append(contentsOf: buffer[..<n])
        while let newline = pending.firstIndex(of: 0x0a) {
            let line = pending[..<newline]
            pending.removeSubrange(...newline)
            if let event = (try? JSONSerialization.jsonObject(with: line)) as? [String: Any] {
                DispatchQueue.main.async { controller?.handle(event) }
            }
        }
    }
}
application.run()
