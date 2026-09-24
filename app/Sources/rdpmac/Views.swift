import AppKit
import ServiceManagement
import SwiftUI
import SystemConfiguration

struct MenuContent: View {
    @ObservedObject var model: Model
    let windows: Windows

    var body: some View {
        Text(model.headline)
        if let line = model.connectionLine {
            Text(line)
        }
        if let status = model.status {
            Divider()
            Button(status.permissions.screenRecording ? "Screen Recording: allowed" : "Allow Screen Recording…") {
                model.grantScreenRecording()
            }
            .disabled(status.permissions.screenRecording)
            Button(status.permissions.accessibility ? "Accessibility: allowed" : "Allow Accessibility…") {
                model.grantAccessibility()
            }
            .disabled(status.permissions.accessibility)
            Divider()
            Button("Settings…") { windows.showSettings(model) }
            Button("Import Certificate…") { model.importCertificate() }
            Button("Copy Certificate Thumbprint") { model.copyThumbprint() }
                .disabled(status.certificate.sha256 == nil)
            Button("Restart Server") { model.restartServer() }
        }
        Divider()
        switch model.server {
        case .on:
            Button("Turn Server Off") { model.disableServer() }
        case .needsApproval:
            Button("Approve the Server in Login Items…") { SMAppService.openSystemSettingsLoginItems() }
        case .off:
            Button("Turn Server On") { model.enableServer() }
        }
        Toggle("Open rdpmac at Login", isOn: Binding(get: { model.opensAtLogin }, set: { model.setOpensAtLogin($0) }))
        Divider()
        Button("Open Logs") { model.openLogs() }
        Button("Collect Diagnostics…") { model.collectDiagnostics() }
        Button("Welcome…") { windows.showWelcome(model) }
        Divider()
        Button("Quit rdpmac") { NSApp.terminate(nil) }
    }
}

/// First-run guide: turn the server on, grant the two permissions, connect.
struct WelcomeView: View {
    @ObservedObject var model: Model
    let windows: Windows

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("rdpmac lets Remote Desktop clients such as Microsoft Remote Desktop (mstsc) and Windows App use this Mac.")
                .fixedSize(horizontal: false, vertical: true)

            if let other = model.conflictingAgent {
                Label("Another rdpmacd is installed as a LaunchAgent (\(other)). Remove it first, for example with: sh scripts/agent.sh uninstall",
                      systemImage: "exclamationmark.triangle")
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
            }

            step(1, "Turn the server on", done: model.server == .on) {
                switch model.server {
                case .on: EmptyView()
                case .needsApproval: Button("Approve in Login Items…") { SMAppService.openSystemSettingsLoginItems() }
                case .off: Button("Turn On") { model.enableServer() }.disabled(model.conflictingAgent != nil)
                }
            }
            step(2, "Allow Screen Recording, so clients see the screen",
                 done: model.status?.permissions.screenRecording == true) {
                Button("Allow…") { model.grantScreenRecording() }.disabled(!model.running)
            }
            step(3, "Allow Accessibility, so clients can use the keyboard and mouse",
                 done: model.status?.permissions.accessibility == true) {
                Button("Allow…") { model.grantAccessibility() }.disabled(!model.running)
            }

            VStack(alignment: .leading, spacing: 6) {
                Text("4. Connect from another computer to").bold()
                Text(Network.addresses().joined(separator: "   "))
                    .textSelection(.enabled)
                    .font(.system(.body, design: .monospaced))
                Text("Sign in with your Mac user name and password. The first time, the client asks whether to trust this Mac's certificate; its SHA-256 thumbprint is")
                    .fixedSize(horizontal: false, vertical: true)
                Text(model.status?.certificate.sha256 ?? "shown once the server runs")
                    .textSelection(.enabled)
                    .font(.system(.caption, design: .monospaced))
                    .fixedSize(horizontal: false, vertical: true)
            }

            HStack {
                Button("Settings…") { windows.showSettings(model) }.disabled(!model.running)
                Spacer()
                Button("Done") { windows.close("welcome") }.keyboardShortcut(.defaultAction)
            }
        }
        .padding(24)
        .frame(width: 520)
    }

    @ViewBuilder
    private func step<Action: View>(_ number: Int, _ title: String, done: Bool, @ViewBuilder action: () -> Action) -> some View {
        HStack {
            Image(systemName: done ? "checkmark.circle.fill" : "circle")
                .foregroundStyle(done ? .green : .secondary)
            Text("\(number). \(title)")
            Spacer()
            if !done {
                action()
            }
        }
    }
}

/// Edits config.toml through the server and restarts it.
struct SettingsView: View {
    @ObservedObject var model: Model
    let close: () -> Void

    @State private var loaded: DaemonSettings?
    @State private var listen = ""
    @State private var followClient = true
    @State private var ownDisplay = true
    @State private var h264 = true
    @State private var clipboard = true
    @State private var fps = 30
    @State private var failure: String?

    var body: some View {
        Form {
            TextField("Listen on", text: $listen, prompt: Text("0.0.0.0:3389"))
            Picker("Resolution", selection: $followClient) {
                Text("Follow the client").tag(true)
                Text("The display's own").tag(false)
            }
            Toggle("Give sessions a display of their own when no screen is attached", isOn: $ownDisplay)
                .disabled(!followClient || model.status?.virtualDisplaysSupported == false)
            Toggle("Use H.264 when the client supports it", isOn: $h264)
            Toggle("Share the clipboard", isOn: $clipboard)
            Stepper("Frame rate: \(fps) per second", value: $fps, in: 5...60, step: 5)
            if let failure {
                Text(failure).foregroundStyle(.red).fixedSize(horizontal: false, vertical: true)
            }
            HStack {
                Spacer()
                Button("Cancel", action: close)
                Button("Save and Restart Server", action: save)
                    .keyboardShortcut(.defaultAction)
                    .disabled(loaded == nil)
            }
        }
        .padding(20)
        .frame(width: 480)
        .onAppear(perform: load)
    }

    private func load() {
        do {
            let file = try model.loadSettings()
            let effective = model.status?.settings
            loaded = file
            listen = file.listen ?? ""
            followClient = (file.resolution ?? effective?.resolution ?? "follow-client") == "follow-client"
            ownDisplay = (file.virtualDisplay ?? effective?.virtualDisplay ?? "auto") == "auto"
            h264 = (file.codec ?? effective?.codec ?? "auto") == "auto"
            clipboard = file.clipboard ?? effective?.clipboard ?? true
            fps = file.fps ?? effective?.fps ?? 30
        } catch {
            failure = error.localizedDescription
        }
    }

    private func save() {
        guard var settings = loaded else { return }
        let address = listen.trimmingCharacters(in: .whitespaces)
        settings.listen = address.isEmpty ? nil : address
        settings.resolution = followClient ? "follow-client" : "native"
        settings.virtualDisplay = ownDisplay ? "auto" : "off"
        settings.codec = h264 ? "auto" : "remotefx"
        settings.clipboard = clipboard
        settings.fps = fps
        do {
            try model.saveSettings(settings)
            close()
        } catch {
            failure = error.localizedDescription
        }
    }
}

enum Network {
    /// The computer's name and its IPv4 addresses, for clients to connect to.
    static func addresses() -> [String] {
        var result: [String] = []
        if let name = SCDynamicStoreCopyLocalHostName(nil) as String? {
            result.append(name + ".local")
        }
        var list: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&list) == 0, let first = list else { return result }
        defer { freeifaddrs(list) }
        for pointer in sequence(first: first, next: { $0.pointee.ifa_next }) {
            let entry = pointer.pointee
            let flags = Int32(entry.ifa_flags)
            guard let address = entry.ifa_addr, address.pointee.sa_family == UInt8(AF_INET),
                  flags & IFF_UP != 0, flags & IFF_LOOPBACK == 0 else { continue }
            var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
            if getnameinfo(address, socklen_t(address.pointee.sa_len), &host, socklen_t(host.count), nil, 0, NI_NUMERICHOST) == 0 {
                result.append(String(cString: host))
            }
        }
        return result
    }
}
