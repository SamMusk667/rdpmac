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
            MenuIcon(model: delegate.model)
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

struct MenuIcon: View {
    @ObservedObject var model: Model

    var body: some View {
        Image(systemName: model.symbol)
    }
}

/// The app's windows are plain AppKit windows around SwiftUI views, so they can be opened from
/// anywhere, including at launch, in an app that has no Dock icon.
@MainActor
final class Windows {
    private var open: [String: NSWindow] = [:]

    func showWelcome(_ model: Model) {
        show("welcome", title: "Welcome to rdpmac", WelcomeView(model: model, windows: self))
    }

    func showSettings(_ model: Model) {
        show("settings", title: "rdpmac Settings", SettingsView(model: model) { [weak self] in
            self?.close("settings")
        })
    }

    func close(_ id: String) {
        open[id]?.close()
    }

    /// Brings a visible window forward; otherwise builds it afresh, so it shows current values.
    private func show<Content: View>(_ id: String, title: String, _ content: Content) {
        NSApp.activate(ignoringOtherApps: true)
        if let window = open[id], window.isVisible {
            window.makeKeyAndOrderFront(nil)
            return
        }
        let window = NSWindow(contentViewController: NSHostingController(rootView: content))
        window.title = title
        window.styleMask = [.titled, .closable]
        window.isReleasedWhenClosed = false
        window.center()
        open[id] = window
        window.makeKeyAndOrderFront(nil)
    }
}
