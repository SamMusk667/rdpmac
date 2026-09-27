import Foundation
import Security
import ServiceManagement

enum ServerState {
    case on
    case needsApproval
    case off
}

/// Turns rdpmacd, the daemon inside the app, on and off as a launchd job in the user's session.
///
/// SMAppService only starts an agent from the app bundle when the app is signed with a Team ID
/// (Developer ID or Apple Development). A build without one, such as a development build or a
/// copy built from source, gets a classic LaunchAgent in ~/Library/LaunchAgents that runs
/// the same daemon by its full path.
enum Server {
    static let label = "com.rdpmac.rdpmacd"
    static let managed = teamIdentifier() != nil

    private static let service = SMAppService.agent(plistName: label + ".plist")
    private static let domain = "gui/\(getuid())"
    private static let plist = URL(fileURLWithPath: NSHomeDirectory() + "/Library/LaunchAgents/\(label).plist")
    private static let logs = NSHomeDirectory() + "/Library/Logs/rdpmac"
    static let program = Bundle.main.bundleURL.appendingPathComponent("Contents/MacOS/rdpmacd").path

    static var state: ServerState {
        if managed {
            switch service.status {
            case .enabled: return .on
            case .requiresApproval: return .needsApproval
            default: return .off
            }
        }
        return classicProgram() == program && loaded() ? .on : .off
    }

    /// A job with our label that runs some other rdpmacd, such as one installed by
    /// scripts/agent.sh; it holds the label and the port.
    static var conflictingProgram: String? {
        guard let other = classicProgram(), other != program else { return nil }
        return other
    }

    static func turnOn() throws {
        if let other = conflictingProgram {
            throw ControlError.refused("Another rdpmacd is installed as a LaunchAgent (\(other)). Remove it first, for example with: sh scripts/agent.sh uninstall")
        }
        if managed {
            try service.register()
            return
        }
        let job: [String: Any] = [
            "Label": label,
            "ProgramArguments": [program],
            "EnvironmentVariables": ["RDPMAC_LOG": "info", "RDPMAC_LOG_DIR": logs],
            "RunAtLoad": true,
            "KeepAlive": ["SuccessfulExit": false],
            "ThrottleInterval": 10,
            "ProcessType": "Interactive",
            "LimitLoadToSessionType": "Aqua",
            "StandardOutPath": logs + "/launchd.log",
            "StandardErrorPath": logs + "/launchd.log",
        ]
        let files = FileManager.default
        try files.createDirectory(at: plist.deletingLastPathComponent(), withIntermediateDirectories: true)
        try files.createDirectory(atPath: logs, withIntermediateDirectories: true)
        try PropertyListSerialization.data(fromPropertyList: job, format: .xml, options: 0).write(to: plist, options: .atomic)
        if loaded() {
            _ = launchctl("bootout", "\(domain)/\(label)")
        }
        let result = launchctl("bootstrap", domain, plist.path)
        guard result.status == 0 else {
            throw ControlError.refused("launchd did not start the server: \(result.output)")
        }
    }

    static func turnOff() throws {
        if managed {
            try service.unregister()
            return
        }
        guard conflictingProgram == nil else { return }
        if loaded() {
            _ = launchctl("bootout", "\(domain)/\(label)")
        }
        try? FileManager.default.removeItem(at: plist)
    }

    private static func loaded() -> Bool {
        launchctl("print", "\(domain)/\(label)").status == 0
    }

    private static func classicProgram() -> String? {
        guard let data = try? Data(contentsOf: plist),
              let job = try? PropertyListSerialization.propertyList(from: data, format: nil) as? [String: Any],
              let arguments = job["ProgramArguments"] as? [String] else { return nil }
        return arguments.first
    }

    private static func launchctl(_ arguments: String...) -> (status: Int32, output: String) {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/launchctl")
        process.arguments = arguments
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = pipe
        do {
            try process.run()
        } catch {
            return (-1, error.localizedDescription)
        }
        let output = pipe.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        return (process.terminationStatus, String(decoding: output, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines))
    }

    private static func teamIdentifier() -> String? {
        var code: SecCode?
        var staticCode: SecStaticCode?
        var information: CFDictionary?
        guard SecCodeCopySelf([], &code) == errSecSuccess, let code,
              SecCodeCopyStaticCode(code, [], &staticCode) == errSecSuccess, let staticCode,
              SecCodeCopySigningInformation(staticCode, SecCSFlags(rawValue: kSecCSSigningInformation), &information) == errSecSuccess,
              let signing = information as? [String: Any] else { return nil }
        return signing[kSecCodeInfoTeamIdentifier as String] as? String
    }
}
