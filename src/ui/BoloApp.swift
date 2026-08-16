import Cocoa
import WebKit

class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate, WKNavigationDelegate {
    var window: NSWindow!
    var webView: WKWebView!
    var targetPort: String = "4525"
    var retryCount: Int = 0
    let maxRetries: Int = 30

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.regular)

        let windowWidth: CGFloat = 640
        let windowHeight: CGFloat = 820

        let screenSize = NSScreen.main?.visibleFrame.size ?? CGSize(width: 1440, height: 900)
        let screenOrigin = NSScreen.main?.visibleFrame.origin ?? CGPoint.zero
        let rect = NSRect(
            x: screenOrigin.x + (screenSize.width - windowWidth) / 2,
            y: screenOrigin.y + (screenSize.height - windowHeight) / 2,
            width: windowWidth,
            height: windowHeight
        )

        window = NSWindow(
            contentRect: rect,
            styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView],
            backing: .buffered,
            defer: false
        )

        window.title = "Bolo"
        window.titlebarAppearsTransparent = true
        window.titleVisibility = .hidden
        window.isMovableByWindowBackground = true
        window.backgroundColor = NSColor(red: 0.047, green: 0.051, blue: 0.071, alpha: 1.0)
        window.delegate = self
        window.minSize = NSSize(width: 460, height: 550)

        // WebKit Configuration with audio/media enablement
        let config = WKWebViewConfiguration()
        config.mediaTypesRequiringUserActionForPlayback = []
        let prefs = WKWebpagePreferences()
        prefs.allowsContentJavaScript = true
        config.defaultWebpagePreferences = prefs

        webView = WKWebView(frame: window.contentView!.bounds, configuration: config)
        webView.autoresizingMask = [.width, .height]
        webView.navigationDelegate = self
        webView.setValue(false, forKey: "drawsBackground")

        window.contentView?.addSubview(webView)

        if CommandLine.arguments.count > 1 && !CommandLine.arguments[1].isEmpty {
            targetPort = CommandLine.arguments[1]
        } else {
            let home = FileManager.default.homeDirectoryForCurrentUser.path
            let portFile = "\(home)/.local/share/bolo/port.txt"
            if let saved = try? String(contentsOfFile: portFile, encoding: .utf8).trimmingCharacters(in: .whitespacesAndNewlines), !saved.isEmpty {
                targetPort = saved
            }
        }

        loadApp()

        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    func loadApp() {
        if let url = URL(string: "http://127.0.0.1:\(targetPort)/") {
            let request = URLRequest(url: url, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 5.0)
            webView.load(request)
        }
    }

    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) {
        if retryCount < maxRetries {
            retryCount += 1
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.25) { [weak self] in
                self?.loadApp()
            }
        }
    }

    func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) {
        if retryCount < maxRetries {
            retryCount += 1
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.25) { [weak self] in
                self?.loadApp()
            }
        }
    }

    func windowWillClose(_ notification: Notification) {
        NSApp.terminate(nil)
    }
}

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.run()
