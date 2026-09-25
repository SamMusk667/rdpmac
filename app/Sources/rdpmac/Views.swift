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
            if model.restartNeeded {
                Button("Restart Server to Use the New Permissions") { model.restartServer() }
            } else if !model.permissionsGranted {
                Text("Restart the server after allowing")
            }
            if model.nlaUnusable {
                Button(model.nlaStale
                       ? "Enroll Again for Network Level Authentication…"
                       : "Enroll for Network Level Authentication…") { windows.showSettings(model) }
            }
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
            if model.running && !model.permissionsGranted {
                HStack(alignment: .firstTextBaseline) {
                    Text(model.restartNeeded
                         ? "The server has to restart to use the permissions you allowed."
                         : "macOS applies a permission to the running server only after it restarts. After switching it on in System Settings, restart the server.")
                        .font(.callout)
                        .foregroundStyle(model.restartNeeded ? .orange : .secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    Spacer()
                    Button("Restart Server") { model.restartServer() }
                }
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
    @State private var codec = "auto"
    @State private var clipboard = true
    @State private var fps = 30
    @State private var nla = false
    @State private var password = ""
    @State private var enrolling = false
    @State private var enrollment: String?
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
            Picker("Codec", selection: $codec) {
                Text("H.264 in full colour (AVC444)").tag("auto")
                Text("H.264 (AVC420)").tag("avc420")
                Text("RemoteFX").tag("remotefx")
            }
            Toggle("Share the clipboard", isOn: $clipboard)
            Stepper("Frame rate: \(fps) per second", value: $fps, in: 5...60, step: 5)
            Toggle("Require Network Level Authentication (NLA)", isOn: $nla)
                .disabled(model.status?.nla == nil)
            if let account = model.status?.nla {
                nlaAccount(account)
            }
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
            codec = file.codec ?? effective?.codec ?? "auto"
            clipboard = file.clipboard ?? effective?.clipboard ?? true
            fps = file.fps ?? effective?.fps ?? 30
            nla = (file.security ?? effective?.security ?? "tls") == "nla"
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
        settings.codec = codec
        settings.clipboard = clipboard
        settings.fps = fps
        settings.security = nla ? "nla" : "tls"
        do {
            try model.saveSettings(settings)
            close()
        } catch {
            failure = error.localizedDescription
        }
    }
}

extension SettingsView {
    /// Clients prove the password before a session exists, against the NT hash the server keeps
    /// for the enrolled user; it is derived from the Mac password, so enroll again after changing it.
    @ViewBuilder
    private func nlaAccount(_ account: DaemonStatus.Nla) -> some View {
        if let error = account.error {
            Text("The keychain could not be read: \(error)")
                .foregroundStyle(.red)
                .fixedSize(horizontal: false, vertical: true)
        } else if account.enrolled == true {
            HStack {
                Text(account.since.map {
                    "\(account.user) enrolled on \(Date(timeIntervalSince1970: $0).formatted(date: .abbreviated, time: .shortened))"
                } ?? "\(account.user) is enrolled")
                Spacer()
                Button("Remove") { model.removeNla() }
            }
            Text("Enroll again after changing the Mac password.")
                .font(.callout)
                .foregroundStyle(.secondary)
        } else if account.stale == true {
            Text("After the update the server cannot read \(account.user)'s enrollment, so NLA sign-ins fail. Enroll again with the Mac password.")
                .font(.callout)
                .foregroundStyle(nla ? .orange : .secondary)
                .fixedSize(horizontal: false, vertical: true)
        } else {
            Text(nla
                 ? "Enroll \(account.user) with the Mac password, or nobody can sign in."
                 : "To use NLA, enroll \(account.user) with the Mac password first.")
                .font(.callout)
                .foregroundStyle(nla ? .orange : .secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        HStack {
            SecureField("Mac password", text: $password)
                .onSubmit(enroll)
            Button(account.enrolled == true || account.stale == true ? "Enroll Again" : "Enroll", action: enroll)
                .disabled(password.isEmpty || enrolling)
        }
        if let enrollment {
            Text(enrollment).foregroundStyle(.red).fixedSize(horizontal: false, vertical: true)
        }
    }

    private func enroll() {
        guard !password.isEmpty, !enrolling else { return }
        enrolling = true
        let entered = password
        Task {
            enrollment = await model.enrollNla(password: entered)
            if enrollment == nil {
                password = ""
            }
            enrolling = false
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
