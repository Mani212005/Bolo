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
//   bolo-pill --snapshot <dir>     render every state to PNG, offscreen, no window
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

struct Level {
    var value: CGFloat  // 0...1, smoothed
    var speech: Bool  // the VAD heard speech in this chunk
}

/// Everything the layer tree needs to draw one frame of the pill.
struct PillAppearance {
    var state: PillState
    var hover = false  // idle only: the handle grows into a "Dictate" button
    var label = ""  // done / error text, and "Dictate" on hover
    var levels: [Level] = []
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

/// Layer tree for the pill. The same tree is shown on screen and rendered to PNG.
final class PillLayer: CALayer {
    let background = CALayer()
    let dot = CALayer()
    let icon = CALayer()
    let text = CATextLayer()
    var bars: [CALayer] = []
    private(set) var shown: PillState?
    private var waveRunning = false

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
        for _ in 0..<barCount {
            let bar = CALayer()
            bar.cornerRadius = barWidth / 2
            addSublayer(bar)
            bars.append(bar)
        }
    }
    override init(layer: Any) { super.init(layer: layer) }
    required init?(coder: NSCoder) { fatalError() }

    /// The pill is exactly as big as what it shows, so nothing transparent
    /// around it can intercept clicks meant for the app underneath.
    static func size(for a: PillAppearance) -> CGSize {
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
        shown = a.state

        background.frame = bounds
        background.cornerRadius = size.height / 2
        background.backgroundColor = isHandle ? color(Palette.handle, 0.55) : color(Palette.panel, 0.92)
        background.borderWidth = isHandle ? 0 : 1

        // Left slot: recording dot, pause glyph, transcribing dot, or the result / mic glyph.
        let slot = CGRect(x: 14, y: midY - 4, width: 8, height: 8)
        dot.isHidden = !(a.state == .recording || a.state == .transcribing)
        dot.frame = slot
        dot.backgroundColor = color(a.state == .recording ? Palette.recording : Palette.transcribing)

        let iconSize: CGFloat = a.state == .done || a.state == .error ? 14 : 13
        var iconImage: CGImage?
        switch a.state {
        case .paused: iconImage = glyph("pause.fill", points: 11, hex: Palette.paused)
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

        let showsText = a.state == .done || a.state == .error || (a.state == .idle && a.hover)
        text.isHidden = !showsText
        text.contentsScale = contentsScale
        if showsText {
            text.string = a.state == .idle ? "Dictate" : a.label
            text.foregroundColor =
                a.state == .done
                ? color(Palette.done) : a.state == .error ? color(Palette.warning) : color(0xffffff, 0.92)
            let x: CGFloat = 14 + iconSize + 6
            text.frame = CGRect(x: x, y: midY - 8, width: size.width - x - 8, height: 16)
        }

        let showsBars = a.state == .recording || a.state == .paused || a.state == .transcribing
        let total = CGFloat(barCount) * barWidth + CGFloat(barCount - 1) * barGap
        let startX = size.width - 14 - total
        let maxHeight = pillHeight - 12
        for (i, bar) in bars.enumerated() {
            bar.isHidden = !showsBars
            guard showsBars else { continue }
            var height: CGFloat = 3
            var alpha: CGFloat = 0.92
            switch a.state {
            case .recording:
                let level = i < a.levels.count ? a.levels[i] : Level(value: 0, speech: false)
                height = max(3, level.value * maxHeight)
                alpha = level.speech ? 0.95 : 0.5
            case .paused:
                alpha = 0.35
            case .transcribing:
                height = 12  // the wave scales this between 0.35x and 1.6x
            default: break
            }
            bar.bounds = CGRect(x: 0, y: 0, width: barWidth, height: height)
            bar.position = CGPoint(x: startX + CGFloat(i) * (barWidth + barGap) + barWidth / 2, y: midY)
            bar.backgroundColor =
                a.state == .transcribing ? color(Palette.transcribing) : color(0xffffff, alpha)
        }
        updateAnimations(for: a.state)
    }

    /// Pulse and wave run on the render server: no per-frame work in this process.
    private func updateAnimations(for state: PillState) {
        if state == .recording {
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

        if state == .transcribing, !waveRunning {
            waveRunning = true
            let start = CACurrentMediaTime()
            for (i, bar) in bars.enumerated() {
                let wave = CABasicAnimation(keyPath: "transform.scale.y")
                wave.fromValue = 0.35
                wave.toValue = 1.6  // 12 pt * 1.6 stays inside the 20 pt of bar room
                wave.duration = 0.45
                wave.autoreverses = true
                wave.repeatCount = .infinity
                wave.beginTime = start + Double(i) * 0.07
                wave.fillMode = .backwards
                wave.timingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
                bar.add(wave, forKey: "wave")
            }
        } else if state != .transcribing, waveRunning {
            waveRunning = false
            bars.forEach { $0.removeAnimation(forKey: "wave") }
        }
    }

    /// Snapshots cannot show running animations; pose the wave at one instant instead.
    func poseWave() {
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        for (i, bar) in bars.enumerated() {
            let scale = 0.35 + 1.25 * (0.5 + 0.5 * sin(Double(i) * 0.75))
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

final class PillView: NSView {
    let pill = PillLayer()
    var onClick: (() -> Void)?
    var onHover: ((Bool) -> Void)?
    var onDragEnd: (() -> Void)?
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
        pill.nudge()
    }

    override func mouseDragged(with event: NSEvent) {
        let now = NSEvent.mouseLocation
        let dx = now.x - pressScreenPoint.x
        let dy = now.y - pressScreenPoint.y
        // A press that barely moves is a click, not a drag.
        if !dragging && hypot(dx, dy) < 3 { return }
        dragging = true
        window?.setFrameOrigin(NSPoint(x: pressOrigin.x + dx, y: pressOrigin.y + dy))
    }

    override func mouseUp(with event: NSEvent) {
        if dragging {
            dragging = false
            onDragEnd?()
        } else {
            onClick?()
        }
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

// MARK: - Controller

final class PillController {
    private let panel: PillPanel
    private let view: PillView
    private var positions = PositionStore(url: dataURL("pill.json"))

    private var daemonPhase = PillState.idle
    private var hold: (state: PillState, label: String)?
    private var holdTimer: Timer?
    private var levels = [Level](repeating: Level(value: 0, speech: false), count: barCount)
    private var hover = false {
        didSet { hoverWatch(hover) }
    }
    private var hoverTimer: Timer?
    private var wasIdle = false
    private var visible = true  // style != hidden
    private var showIdle = true

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

        view.onClick = { [weak self] in self?.clicked() }
        view.onHover = { [weak self] inside in
            self?.hover = inside
            self?.render()
        }
        view.onDragEnd = { [weak self] in self?.dragged() }
        NotificationCenter.default.addObserver(
            forName: NSApplication.didChangeScreenParametersNotification, object: nil, queue: .main
        ) { [weak self] _ in self?.screensChanged() }
        place(on: screenUnderPointer())
    }

    // MARK: events

    func handle(_ event: [String: Any]) {
        switch event["type"] as? String {
        case "hello":
            if let v = event["v"] as? Int, v != 1 {
                FileHandle.standardError.write(Data("[bolo-pill] unknown protocol v\(v)\n".utf8))
            }
            visible = (event["style"] as? String) != "hidden"
            showIdle = (event["show_idle"] as? Bool) ?? true
            setPhase(event["phase"] as? String ?? "idle")
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

    private func setPhase(_ name: String) {
        let previous = daemonPhase
        switch name {
        case "recording": daemonPhase = .recording
        case "paused": daemonPhase = .paused
        case "processing": daemonPhase = .transcribing
        default: daemonPhase = .idle
        }
        if daemonPhase == .recording && previous != .paused {
            // A new dictation: drop any lingering result, start with a flat meter,
            // and show up on the screen the pointer is on.
            hold = nil
            holdTimer?.invalidate()
            levels = [Level](repeating: Level(value: 0, speech: false), count: barCount)
            place(on: screenUnderPointer())
        }
        render()
    }

    // MARK: drawing

    private func appearance() -> PillAppearance? {
        if let hold = hold {
            return PillAppearance(state: hold.state, label: hold.label)
        }
        if daemonPhase == .idle {
            return showIdle ? PillAppearance(state: .idle, hover: hover) : nil
        }
        return PillAppearance(state: daemonPhase, levels: levels)
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

    private func render() {
        syncHover()
        guard visible, let a = appearance() else {
            panel.orderOut(nil)
            shownSize = .zero
            return
        }
        let size = PillLayer.size(for: a)
        if size != shownSize {
            shownSize = size
            panel.setFrame(frame(for: size), display: false)
        }
        view.pill.apply(a)
        if !panel.isVisible { panel.orderFrontRegardless() }
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
    /// clipped by the window, which is exactly as big as the pill).
    private func shake() {
        let home = panel.frame.origin
        for (i, dx) in [-4.0, 4.0, -3.0, 3.0, 0.0].enumerated() {
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.045 * Double(i)) { [weak self] in
                guard let self = self, self.panel.isVisible, self.shownSize == self.panel.frame.size else { return }
                self.panel.setFrameOrigin(NSPoint(x: home.x + CGFloat(dx), y: home.y))
            }
        }
    }

    private func clicked() {
        guard hold == nil else { return }
        let command: String
        switch daemonPhase {
        case .idle, .recording: command = "toggle"
        case .paused: command = "pause"  // resume
        default:
            shake()  // transcribing: nothing to do yet
            return
        }
        DispatchQueue.global().async { [weak self] in
            let reply = sendCommand(command)
            DispatchQueue.main.async {
                if let reply = reply, reply.hasPrefix("busy") || reply.hasPrefix("err") {
                    self?.shake()
                }
            }
        }
    }
}

// MARK: - Snapshots

/// Renders every state to PNG without showing a window.
func renderSnapshots(to directory: String) {
    let scale: CGFloat = 3
    let speaking = [0.15, 0.3, 0.55, 0.8, 0.95, 0.7, 0.45, 0.65, 0.4, 0.25, 0.12].map { Level(value: $0, speech: true) }
    let quiet = [0.05, 0.06, 0.04, 0.08, 0.05, 0.07, 0.04, 0.06, 0.05, 0.04, 0.05].map { Level(value: $0, speech: false) }
    let cases: [(String, PillAppearance)] = [
        ("idle", PillAppearance(state: .idle)),
        ("idle-hover", PillAppearance(state: .idle, hover: true)),
        ("recording-speech", PillAppearance(state: .recording, levels: speaking)),
        ("recording-quiet", PillAppearance(state: .recording, levels: quiet)),
        ("recording-start", PillAppearance(state: .recording, levels: [Level](repeating: Level(value: 0, speech: false), count: barCount))),
        ("paused", PillAppearance(state: .paused)),
        ("transcribing", PillAppearance(state: .transcribing)),
        ("done-pasted", PillAppearance(state: .done, label: "Pasted")),
        ("done-copied", PillAppearance(state: .done, label: "Copied")),
        ("error-no-speech", PillAppearance(state: .error, label: "No speech")),
        ("error-mic-unavailable", PillAppearance(state: .error, label: "Mic unavailable")),
        ("error-paste-stopped", PillAppearance(state: .error, label: "Paste stopped")),
        ("error-max-length", PillAppearance(state: .error, label: "Max length")),
    ]
    try? FileManager.default.createDirectory(atPath: directory, withIntermediateDirectories: true)
    let pad: CGFloat = 12
    var images: [(String, CGImage, CGSize)] = []
    for (name, appearance) in cases {
        let size = PillLayer.size(for: appearance)
        let layer = PillLayer()
        layer.contentsScale = scale
        layer.frame = CGRect(origin: .zero, size: size)
        layer.apply(appearance)
        if appearance.state == .transcribing { layer.poseWave() }
        guard
            let ctx = CGContext(
                data: nil, width: Int(size.width * scale), height: Int(size.height * scale),
                bitsPerComponent: 8, bytesPerRow: 0, space: CGColorSpace(name: CGColorSpace.sRGB)!,
                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
        else { continue }
        ctx.scaleBy(x: scale, y: scale)
        layer.render(in: ctx)
        guard let image = ctx.makeImage() else { continue }
        let rep = NSBitmapImageRep(cgImage: image)
        let url = URL(fileURLWithPath: directory).appendingPathComponent("pill-\(name).png")
        try? rep.representation(using: .png, properties: [:])?.write(to: url)
        print(url.path)
        images.append((name, image, size))
    }

    // One sheet with every state on a dark and a light backdrop.
    let rowHeight = pillHeight + pad * 2
    let columnWidth: CGFloat = 340
    let sheetSize = CGSize(width: columnWidth * 2, height: rowHeight * CGFloat(images.count))
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
    for (row, entry) in images.enumerated() {
        let y = sheetSize.height - CGFloat(row + 1) * rowHeight
        for column in 0..<2 {
            let x = CGFloat(column) * columnWidth
            (entry.0 as NSString).draw(at: CGPoint(x: x + 10, y: y + rowHeight / 2 - 6), withAttributes: caption)
            sheet.draw(entry.1, in: CGRect(x: x + 160, y: y + pad, width: entry.2.width, height: entry.2.height))
        }
    }
    NSGraphicsContext.restoreGraphicsState()
    if let image = sheet.makeImage() {
        let url = URL(fileURLWithPath: directory).appendingPathComponent("pill-states.png")
        try? NSBitmapImageRep(cgImage: image).representation(using: .png, properties: [:])?.write(to: url)
        print(url.path)
    }
}

// MARK: - Main

let arguments = CommandLine.arguments
let application = NSApplication.shared
application.setActivationPolicy(.accessory)  // no Dock icon, no menu bar, never frontmost

if arguments.count >= 2 && arguments[1] == "--snapshot" {
    DispatchQueue.main.async {
        renderSnapshots(to: arguments.count > 2 ? arguments[2] : ".")
        exit(0)
    }
    application.run()
} else if arguments.count >= 2 {
    print("usage: bolo-pill [--snapshot <dir>]")
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
    guard let fd = fd, writeAll(fd, "subscribe\n") else {
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
