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

/// The version this app was built as, such as 0.4.0-dev55 between releases; scripts/build-app.sh
/// writes it into Info.plist.
let appVersion = Bundle.main.object(forInfoDictionaryKey: "RDPMacVersion") as? String
    ?? Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
    ?? "development"

/// First-run guide: turn the server on, grant the two permissions, connect.
struct WelcomeView: View {
    @ObservedObject var model: Model
    let windows: Windows

    /// This app's version, and the server's when the running one differs, as it does until the
    /// server restarts after an update.
    private var versionLine: String {
        guard let server = model.status?.version, server != appVersion else { return "Version \(appVersion)" }
        return "Version \(appVersion), server \(server)"
    }

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
                Text(versionLine)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
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
    @State private var parallelConversion = true
    /// The choices as loaded, to tell whether saving restarts the server.
    @State private var initial: Choices?
    @State private var clipboard = true
    @State private var audio = true
    @State private var drives = true
    @State private var unlock = true
    @State private var udp = false
    @State private var muteMac = true
    @State private var fps = 30
    @State private var nla = false
    @State private var password = ""
    @State private var enrolling = false
    @State private var enrollment: String?
    @State private var failure: String?

    /// The groups scroll on a screen too short for them, such as a 1280x720 session, so that the
    /// buttons stay on the screen.
    private let groupsHeight = (NSScreen.main?.visibleFrame.height ?? 900) - 160

    var body: some View {
        VStack(spacing: 0) {
            ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                SettingsGroup("Connection") {
                    SettingsRow("Listen on") {
                        TextField("Listen on", text: $listen, prompt: Text("0.0.0.0:3389"))
                            .labelsHidden()
                            .frame(width: 180)
                    }
                    Divider()
                    SettingsRow(
                        "Offer UDP",
                        caption: "Clients that support it, such as mstsc, get the picture over UDP on the same port; others stay on TCP."
                    ) {
                        Toggle("Offer UDP", isOn: $udp).switchStyle()
                    }
                    Divider()
                    SettingsRow("Require Network Level Authentication (NLA)") {
                        Toggle("Require Network Level Authentication (NLA)", isOn: $nla).switchStyle()
                    }
                    .disabled(model.status?.nla == nil)
                    if let account = model.status?.nla {
                        Divider()
                        nlaAccount(account)
                    }
                }
                SettingsGroup("Lock Screen") {
                    SettingsRow(
                        "Unlock with the password used to log on",
                        caption: "When the Mac is locked, the password \(NSUserName()) logs on with is typed into "
                            + "the lock screen, so the session opens on the desktop."
                    ) {
                        Toggle("Unlock with the password used to log on", isOn: $unlock).switchStyle()
                    }
                    // Only a password PAM checked is the Mac's own.
                    .disabled(model.status != nil && model.status?.nla == nil)
                }
                SettingsGroup(
                    "Display",
                    footer: codec != "remotefx" && initial?.h264 == true
                        ? "The codec and the conversion apply from the next connection, without a restart." : nil
                ) {
                    SettingsRow("Resolution") {
                        Picker("Resolution", selection: $followClient) {
                            Text("Follow the client").tag(true)
                            Text("The display's own").tag(false)
                        }
                        .labelsHidden()
                        .fixedSize()
                    }
                    Divider()
                    SettingsRow("Virtual display when no screen is attached") {
                        Toggle("Virtual display when no screen is attached", isOn: $ownDisplay).switchStyle()
                    }
                    .disabled(!followClient || model.status?.virtualDisplaysSupported == false)
                    Divider()
                    SettingsRow("Frame rate") {
                        HStack(spacing: 6) {
                            Text("\(fps) per second").monospacedDigit().foregroundStyle(.secondary)
                            Stepper("Frame rate", value: $fps, in: 5...60, step: 5).labelsHidden()
                        }
                    }
                    Divider()
                    SettingsRow("Codec") {
                        Picker("Codec", selection: $codec) {
                            Text("H.264 in full colour (AVC444)").tag("auto")
                            Text("H.264 (AVC420)").tag("avc420")
                            Text("RemoteFX").tag("remotefx")
                        }
                        .labelsHidden()
                        .fixedSize()
                    }
                    Divider()
                    SettingsRow("Convert colours on several cores") {
                        Toggle("Convert colours on several cores", isOn: $parallelConversion).switchStyle()
                    }
                    .disabled(codec != "auto")
                }
                SettingsGroup("Devices") {
                    SettingsRow("Share the clipboard") {
                        Toggle("Share the clipboard", isOn: $clipboard).switchStyle()
                    }
                    Divider()
                    SettingsRow("Play the Mac's sound on the client") {
                        Toggle("Play the Mac's sound on the client", isOn: $audio).switchStyle()
                    }
                    Divider()
                    SettingsRow("Mute the Mac meanwhile") {
                        Toggle("Mute the Mac meanwhile", isOn: $muteMac).switchStyle()
                    }
                    .disabled(!audio)
                    Divider()
                    SettingsRow("Mount the drives the client shares", caption: "In ~/RDP Drives, named like “C on DESKTOP-01”.") {
                        Toggle("Mount the drives the client shares", isOn: $drives).switchStyle()
                    }
                }
            }
            .padding(20)
            }
            .frame(maxHeight: groupsHeight)
            Divider()
            HStack(spacing: 8) {
                if let failure {
                    Text(failure).foregroundStyle(.red).fixedSize(horizontal: false, vertical: true)
                }
                Spacer(minLength: 0)
                Button("Cancel", action: close)
                    .keyboardShortcut(.cancelAction)
                Button(restarts ? "Save and Restart Server" : "Save", action: save)
                    .keyboardShortcut(.defaultAction)
                    .disabled(loaded == nil)
            }
            .padding(.horizontal, 20)
            .padding(.vertical, 14)
        }
        .frame(width: 540)
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
            parallelConversion = file.parallelConversion ?? effective?.parallelConversion ?? true
            clipboard = file.clipboard ?? effective?.clipboard ?? true
            audio = file.audio ?? effective?.audio ?? true
            drives = file.drives ?? effective?.drives ?? true
            unlock = file.unlock ?? effective?.unlock ?? true
            udp = file.udp ?? effective?.udp ?? false
            muteMac = file.muteMac ?? effective?.muteMac ?? true
            fps = file.fps ?? effective?.fps ?? 30
            nla = (file.security ?? effective?.security ?? "tls") == "nla"
            initial = choices
        } catch {
            failure = error.localizedDescription
        }
    }

    /// What saving restarts the server for: everything but the choice between AVC444 and AVC420
    /// and the colour conversion, which reach the next connection without a restart.
    private struct Choices: Equatable {
        var listen: String
        var followClient: Bool
        var ownDisplay: Bool
        var h264: Bool
        var clipboard: Bool
        var audio: Bool
        var drives: Bool
        var unlock: Bool
        var udp: Bool
        var muteMac: Bool
        var fps: Int
        var nla: Bool
    }

    private var choices: Choices {
        Choices(listen: listen, followClient: followClient, ownDisplay: ownDisplay, h264: codec != "remotefx",
                clipboard: clipboard, audio: audio, drives: drives, unlock: unlock, udp: udp, muteMac: muteMac, fps: fps,
                nla: nla)
    }

    private var restarts: Bool {
        initial != choices
    }

    private func save() {
        guard var settings = loaded else { return }
        let address = listen.trimmingCharacters(in: .whitespaces)
        settings.listen = address.isEmpty ? nil : address
        settings.resolution = followClient ? "follow-client" : "native"
        settings.virtualDisplay = ownDisplay ? "auto" : "off"
        settings.codec = codec
        settings.parallelConversion = parallelConversion
        settings.clipboard = clipboard
        settings.audio = audio
        settings.drives = drives
        settings.unlock = unlock
        settings.udp = udp
        settings.muteMac = muteMac
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
        VStack(alignment: .leading, spacing: 8) {
            if let error = account.error {
                Text("The keychain could not be read: \(error)")
                    .foregroundStyle(.red)
                    .fixedSize(horizontal: false, vertical: true)
            } else if account.enrolled == true {
                SettingsRow(
                    account.since.map {
                        "\(account.user) enrolled on \(Date(timeIntervalSince1970: $0).formatted(date: .abbreviated, time: .shortened))"
                    } ?? "\(account.user) is enrolled",
                    caption: "Enroll again after changing the Mac password."
                ) {
                    Button("Remove") { model.removeNla() }
                }
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
            HStack(spacing: 8) {
                SecureField("Mac password", text: $password)
                    .onSubmit(enroll)
                Button(account.enrolled == true || account.stale == true ? "Enroll Again" : "Enroll", action: enroll)
                    .disabled(password.isEmpty || enrolling)
            }
            if let enrollment {
                Text(enrollment).foregroundStyle(.red).fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(.vertical, 8)
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

/// A titled box of settings rows, as System Settings groups them.
private struct SettingsGroup<Content: View>: View {
    let title: String
    let footer: String?
    let content: Content

    init(_ title: String, footer: String? = nil, @ViewBuilder content: () -> Content) {
        self.title = title
        self.footer = footer
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title).font(.headline)
            VStack(alignment: .leading, spacing: 0) {
                content
            }
            .padding(.horizontal, 12)
            .background(RoundedRectangle(cornerRadius: 8).fill(Color.primary.opacity(0.04)))
            .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Color.primary.opacity(0.1)))
            if let footer {
                Text(footer)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.horizontal, 12)
            }
        }
    }
}

/// A setting's name, with an optional explanation under it, and its control on the right.
private struct SettingsRow<Control: View>: View {
    let title: String
    let caption: String?
    let control: Control

    init(_ title: String, caption: String? = nil, @ViewBuilder control: () -> Control) {
        self.title = title
        self.caption = caption
        self.control = control()
    }

    var body: some View {
        HStack(spacing: 12) {
            VStack(alignment: .leading, spacing: 2) {
                Text(title).fixedSize(horizontal: false, vertical: true)
                if let caption {
                    Text(caption)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            Spacer(minLength: 0)
            control
        }
        .padding(.vertical, 7)
    }
}

private extension Toggle {
    /// A switch at the size System Settings uses, its name left to the row.
    func switchStyle() -> some View {
        labelsHidden().toggleStyle(.switch).controlSize(.small)
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
