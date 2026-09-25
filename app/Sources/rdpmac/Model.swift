import AppKit
import ServiceManagement

/// What the menu and windows show, refreshed from rdpmacd every two seconds, and the actions
/// they offer.
@MainActor
final class Model: ObservableObject {
    @Published private(set) var status: DaemonStatus?
    @Published private(set) var unreachable: String?
    @Published private(set) var server = Server.state
    @Published private(set) var conflictingAgent = Server.conflictingProgram
    @Published private(set) var opensAtLogin = false

    private var timer: Timer?
    private var refreshing = false

    var running: Bool { status != nil }

    var permissionsGranted: Bool {
        guard let permissions = status?.permissions else { return false }
        return permissions.screenRecording && permissions.accessibility && !restartNeeded
    }

    /// macOS applies a permission to the running server only after it restarts.
    var restartNeeded: Bool { status?.restartNeeded == true }

    var symbol: String {
        guard let status, permissionsGranted else { return "exclamationmark.triangle" }
        return status.connection == nil ? "display" : "person.crop.rectangle"
    }

    var headline: String {
        guard let status else {
            switch server {
            case .needsApproval: return "Server waiting for approval in Login Items"
            case .on: return "Server starting…"
            case .off: return "Server off"
            }
        }
        return "Listening on \(status.settings.listen ?? "port 3389")"
    }

    var connectionLine: String? {
        guard let connection = status?.connection else { return nil }
        let host = connection.peer.split(separator: ":").dropLast().joined(separator: ":")
        let since = Date(timeIntervalSince1970: connection.since).formatted(date: .omitted, time: .shortened)
        let size = status?.sessionSize.map { " at \($0[0])x\($0[1])" } ?? ""
        return "\(connection.user ?? "Signing in") from \(host) since \(since)\(size)"
    }

    func start() {
        refresh()
        timer = Timer.scheduledTimer(withTimeInterval: 2, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.refresh() }
        }
    }

    func refresh() {
        opensAtLogin = SMAppService.mainApp.status == .enabled
        guard !refreshing else { return }
        refreshing = true
        Task.detached {
            let result = Result { try ControlClient().status() }
            let (server, conflicting) = (Server.state, Server.conflictingProgram)
            await MainActor.run {
                self.refreshing = false
                self.server = server
                self.conflictingAgent = conflicting
                switch result {
                case .success(let status):
                    self.status = status
                    self.unreachable = nil
                case .failure(let error):
                    self.status = nil
                    self.unreachable = error.localizedDescription
                }
            }
        }
    }

    // MARK: - Server

    func enableServer() {
        Task.detached {
            let result = Result { try Server.turnOn() }
            let state = Server.state
            await MainActor.run {
                if case .failure(let error) = result {
                    self.report("The server could not be turned on", error)
                } else if state == .needsApproval {
                    SMAppService.openSystemSettingsLoginItems()
                }
                self.refreshSoon()
            }
        }
    }

    func disableServer() {
        Task.detached {
            let result = Result { try Server.turnOff() }
            await MainActor.run {
                if case .failure(let error) = result {
                    self.report("The server could not be turned off", error)
                }
                self.refreshSoon()
            }
        }
    }

    func restartServer() {
        perform { try $0.restart() }
    }

    func setOpensAtLogin(_ on: Bool) {
        do {
            if on {
                try SMAppService.mainApp.register()
            } else {
                try SMAppService.mainApp.unregister()
            }
        } catch {
            report("rdpmac could not change its login item", error)
        }
        refresh()
    }

    // MARK: - Permissions

    /// rdpmacd asks for both permissions itself, so macOS lists it under Privacy & Security.
    func grantScreenRecording() {
        perform({ try $0.requestPermissions() }) {
            Self.openSettings("Privacy_ScreenCapture")
        }
    }

    func grantAccessibility() {
        perform({ try $0.requestPermissions() }) {
            Self.openSettings("Privacy_Accessibility")
        }
    }

    private static func openSettings(_ anchor: String) {
        if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?\(anchor)") {
            NSWorkspace.shared.open(url)
        }
    }

    // MARK: - Settings and certificate

    func loadSettings() throws -> DaemonSettings {
        try ControlClient().fileSettings()
    }

    func saveSettings(_ settings: DaemonSettings) throws {
        let client = ControlClient()
        try client.save(settings)
        try client.restart()
        refreshSoon()
    }

    func importCertificate() {
        let panel = NSOpenPanel()
        panel.title = "Choose the certificate and its private key"
        panel.message = "PEM files, either one file with both or two files. The server restarts to use them."
        panel.allowsMultipleSelection = true
        panel.canChooseDirectories = false
        NSApp.activate(ignoringOtherApps: true)
        guard panel.runModal() == .OK else { return }
        let text = panel.urls.compactMap { try? String(contentsOf: $0, encoding: .utf8) }.joined(separator: "\n")
        let (certificate, key) = PEM.split(text)
        guard !certificate.isEmpty, !key.isEmpty else {
            report("No certificate and key found", ControlError.refused(
                "Choose PEM files that contain a certificate and its unencrypted private key."))
            return
        }
        perform { client in
            try client.importCertificate(certificate: certificate, key: key)
            try client.restart()
        }
    }

    func copyThumbprint() {
        guard let thumbprint = status?.certificate.sha256 else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(thumbprint, forType: .string)
    }

    // MARK: - Network Level Authentication

    /// Enrolls the server's user for NLA; returns why it failed, or nil.
    func enrollNla(password: String) async -> String? {
        let result = await Task.detached { Result { try ControlClient().enrollNla(password: password) } }.value
        refreshSoon()
        if case .failure(let error) = result {
            return error.localizedDescription
        }
        return nil
    }

    func removeNla() {
        perform { try $0.removeNla() }
    }

    /// NLA is on, but nobody is enrolled who could sign in.
    var nlaUnusable: Bool {
        status?.settings.security == "nla" && status?.nla?.enrolled == false
    }

    // MARK: - Logs and diagnostics

    func openLogs() {
        let path = status?.logDir ?? NSHomeDirectory() + "/Library/Logs/rdpmac"
        try? FileManager.default.createDirectory(atPath: path, withIntermediateDirectories: true)
        NSWorkspace.shared.open(URL(fileURLWithPath: path, isDirectory: true))
    }

    func collectDiagnostics() {
        Task.detached {
            let result = Result { try Diagnostics.collect() }
            await MainActor.run {
                switch result {
                case .success(let archive): NSWorkspace.shared.activateFileViewerSelecting([archive])
                case .failure(let error): self.report("Collecting diagnostics failed", error)
                }
            }
        }
    }

    // MARK: - Helpers

    private func perform(_ action: @escaping @Sendable (ControlClient) throws -> Void, then: (() -> Void)? = nil) {
        Task.detached {
            let result = Result { try action(ControlClient()) }
            await MainActor.run {
                if case .failure(let error) = result {
                    self.report("The server did not do that", error)
                } else {
                    then?()
                }
                self.refreshSoon()
            }
        }
    }

    private func refreshSoon() {
        refresh()
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) { [weak self] in self?.refresh() }
    }

    func report(_ title: String, _ error: Error) {
        let alert = NSAlert()
        alert.messageText = title
        alert.informativeText = error.localizedDescription
        NSApp.activate(ignoringOtherApps: true)
        alert.runModal()
    }
}

/// Picks certificates and the first private key out of PEM text.
enum PEM {
    static func split(_ text: String) -> (certificates: String, key: String) {
        var certificates: [String] = []
        var key: String?
        var block: [Substring] = []
        var label: Substring?
        for line in text.split(whereSeparator: \.isNewline) {
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            if label == nil, trimmed.hasPrefix("-----BEGIN "), trimmed.hasSuffix("-----") {
                label = Substring(trimmed.dropFirst(11).dropLast(5))
                block = [Substring(trimmed)]
            } else if let current = label {
                block.append(Substring(trimmed))
                if trimmed == "-----END \(current)-----" {
                    let pem = block.joined(separator: "\n") + "\n"
                    if current == "CERTIFICATE" {
                        certificates.append(pem)
                    } else if current.hasSuffix("PRIVATE KEY"), key == nil {
                        key = pem
                    }
                    label = nil
                }
            }
        }
        return (certificates.joined(), key ?? "")
    }
}
