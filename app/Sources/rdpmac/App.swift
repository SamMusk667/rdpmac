import AppKit
import SwiftUI

/// `rdpmac --enable-server | --disable-server | --server-status | --collect-diagnostics` works
/// without the menu, for scripts and support; without arguments the menu-bar app starts.
@main
enum Entry {
    static func main() {
        let command = CommandLine.arguments.dropFirst().first
        do {
            switch command {
            case "--enable-server": try Server.turnOn()
            case "--disable-server": try Server.turnOff()
            case "--server-status": break
            case "--collect-diagnostics":
                print(try Diagnostics.collect().path)
                return
            default:
                RdpMacApp.main()
                return
            }
        } catch {
            print("error: \(error.localizedDescription)")
        }
        let how = Server.managed ? "managed by macOS" : "classic LaunchAgent, the app has no Team ID"
        switch Server.state {
        case .on: print("server: on (\(how))")
        case .needsApproval: print("server: waiting for approval in System Settings > General > Login Items")
        case .off: print("server: off (\(how))")
        }
    }
}

struct RdpMacApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate

    var body: some Scene {
        MenuBarExtra {
            MenuContent(model: delegate.model, windows: delegate.windows)
        } label: {
            MenuBarIcon(model: delegate.model)
        }
    }
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    let model = Model()
    let windows = Windows()

    func applicationDidFinishLaunching(_ notification: Notification) {
        model.start()
        // First launch, or the server was turned off: explain what to do.
        if model.server != .on {
            windows.showWelcome(model)
        }
    }
}

/// The menu-bar icon's states, drawn as template images in app/Icons/menu.
enum MenuIcon {
    /// No status yet: the server is starting.
    case host
    /// Listening, nobody connected.
    case ready
    /// A client is connected.
    case active
    /// The server is off.
    case paused
    /// Something needs the user: a permission, approval in Login Items, an NLA enrollment.
    case attention

    private var imageName: String {
        switch self {
        case .host: return "HostTemplate"
        case .ready: return "ReadyTemplate"
        case .active: return "ActiveTemplate"
        case .paused: return "PausedTemplate"
        case .attention: return "ErrorTemplate"
        }
    }

    /// The template image from the app's resources; nil in a build without them.
    var image: NSImage? {
        guard let image = NSImage(named: imageName) else { return nil }
        image.isTemplate = true
        return image
    }

    /// For a build without the icons, such as `swift run`.
    var symbol: String {
        switch self {
        case .host, .ready: return "display"
        case .active: return "person.crop.rectangle"
        case .paused: return "pause.circle"
        case .attention: return "exclamationmark.triangle"
        }
    }

    var label: String {
        switch self {
        case .host: return "rdpmac is starting"
        case .ready: return "rdpmac is waiting for connections"
        case .active: return "A client is connected to rdpmac"
        case .paused: return "rdpmac is off"
        case .attention: return "rdpmac needs attention"
        }
    }
}

struct MenuBarIcon: View {
    @ObservedObject var model: Model

    var body: some View {
        let icon = model.menuIcon
        Group {
            if let image = icon.image {
                Image(nsImage: image)
            } else {
                Image(systemName: icon.symbol)
            }
        }
        .accessibilityLabel(icon.label)
    }
}

/// The app's windows are plain AppKit windows around SwiftUI views, so they can be opened from
/// anywhere, including at launch, in an app that has no Dock icon.
@MainActor
final class Windows {
    private struct Shown {
        let window: NSWindow
        let closing: NSObjectProtocol
    }

    private var shown: [String: Shown] = [:]

    func showWelcome(_ model: Model) {
        show("welcome", title: "Welcome to rdpmac", WelcomeView(model: model, windows: self))
    }

    func showSettings(_ model: Model) {
        show("settings", title: "rdpmac Settings", SettingsView(model: model) { [weak self] in
            self?.close("settings")
        })
    }

    func close(_ id: String) {
        shown[id]?.window.close()
    }

    /// Brings a visible window forward; otherwise builds it afresh, so it shows current values.
    private func show<Content: View>(_ id: String, title: String, _ content: Content) {
        NSApp.activate(ignoringOtherApps: true)
        if let window = shown[id]?.window, window.isVisible {
            window.makeKeyAndOrderFront(nil)
            return
        }
        // AppKit must not size the window from the content while it lays the window out. With a
        // hosting controller as the window's content, it asks SwiftUI for the content's size in
        // its constraint passes, as minimum and maximum sizes or as the preferred content size,
        // and SwiftUI answers by asking for another pass. Once the window has the keyboard focus
        // on macOS 26 the passes never settle: AppKit gives up with an exception after as many
        // passes as the window has views, and the app quits. So the content goes in a hosting
        // view that constrains nothing, SwiftUI reports the size it needs, and the window takes
        // that size after the layout pass that measured it.
        let window = NSWindow(
            contentRect: NSRect(origin: .zero, size: Self.measure(content)),
            styleMask: [.titled, .closable],
            backing: .buffered,
            defer: false
        )
        let hosting = NSHostingView(rootView: Measured(content: content) { [weak window] size in
            DispatchQueue.main.async {
                guard let window, size.width > 0, size.height > 0,
                      window.contentRect(forFrameRect: window.frame).size != size else { return }
                window.setContentSize(size)
            }
        })
        hosting.sizingOptions = []
        window.contentView = hosting
        window.title = title
        window.isReleasedWhenClosed = false
        window.center()
        // A closed window is let go, so its content stops following the status.
        let closing = NotificationCenter.default.addObserver(
            forName: NSWindow.willCloseNotification, object: window, queue: .main
        ) { [weak self] _ in
            DispatchQueue.main.async { self?.forget(id) }
        }
        shown[id] = Shown(window: window, closing: closing)
        window.makeKeyAndOrderFront(nil)
    }

    /// The size the content asks for, measured outside any window.
    private static func measure<Content: View>(_ content: Content) -> NSSize {
        NSHostingController(rootView: content).sizeThatFits(in: NSSize(width: CGFloat.infinity, height: .infinity))
    }

    private func forget(_ id: String) {
        guard let gone = shown[id], !gone.window.isVisible else { return }
        NotificationCenter.default.removeObserver(gone.closing)
        shown[id] = nil
    }
}

/// A window's content at the size it asks for, which it reports whenever that changes.
private struct Measured<Content: View>: View {
    let content: Content
    let report: (CGSize) -> Void

    var body: some View {
        content
            .fixedSize()
            .background(GeometryReader { proxy in
                Color.clear.preference(key: IdealSize.self, value: proxy.size)
            })
            .onPreferenceChange(IdealSize.self, perform: report)
    }
}

private struct IdealSize: PreferenceKey {
    static let defaultValue = CGSize.zero

    /// Views that report no size take part as zero, so the largest size is the content's.
    static func reduce(value: inout CGSize, nextValue: () -> CGSize) {
        let next = nextValue()
        value = CGSize(width: max(value.width, next.width), height: max(value.height, next.height))
    }
}
