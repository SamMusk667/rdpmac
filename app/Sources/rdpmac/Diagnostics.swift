import Foundation

/// Gathers what a bug report needs into a zip archive on the Desktop: the server's status, its
/// logs and crash reports, the settings file, and the macOS version and Mac model. The logs name
/// the users who connected and their addresses; nothing else personal is included.
enum Diagnostics {
    static func collect() throws -> URL {
        let files = FileManager.default
        let home = URL(fileURLWithPath: NSHomeDirectory(), isDirectory: true)
        let stamp = Self.stamp()
        let name = "rdpmac-diagnostics-\(stamp)"
        let staging = files.temporaryDirectory.appendingPathComponent(name, isDirectory: true)
        try? files.removeItem(at: staging)
        try files.createDirectory(at: staging, withIntermediateDirectories: true)
        defer { try? files.removeItem(at: staging) }

        let status = (try? ControlClient().send(["cmd": "status"])) ?? Data("{\"ok\":false,\"error\":\"server not running\"}".utf8)
        try status.write(to: staging.appendingPathComponent("status.json"))

        copy(matching: { $0.hasSuffix(".log") }, from: home.appendingPathComponent("Library/Logs/rdpmac"), to: staging.appendingPathComponent("logs"))
        copy(matching: { $0.hasPrefix("rdpmac") }, from: home.appendingPathComponent("Library/Logs/DiagnosticReports"), to: staging.appendingPathComponent("crashes"))
        let config = home.appendingPathComponent("Library/Application Support/rdpmac/config.toml")
        if files.fileExists(atPath: config.path) {
            try? files.copyItem(at: config, to: staging.appendingPathComponent("config.toml"))
        }
        let system = [
            run("/usr/bin/sw_vers"),
            run("/usr/sbin/sysctl", "-n", "hw.model", "machdep.cpu.brand_string"),
            run("/usr/bin/uname", "-a"),
        ].joined(separator: "\n")
        try Data(system.utf8).write(to: staging.appendingPathComponent("system.txt"))

        let desktop = files.urls(for: .desktopDirectory, in: .userDomainMask).first ?? home
        let archive = desktop.appendingPathComponent("\(name).zip")
        let zipped = run("/usr/bin/ditto", "-c", "-k", "--keepParent", staging.path, archive.path)
        guard files.fileExists(atPath: archive.path) else {
            throw ControlError.io("ditto failed: \(zipped)")
        }
        return archive
    }

    private static func stamp() -> String {
        let format = DateFormatter()
        format.dateFormat = "yyyyMMdd-HHmmss"
        return format.string(from: Date())
    }

    private static func copy(matching wanted: (String) -> Bool, from source: URL, to target: URL) {
        let files = FileManager.default
        guard let names = try? files.contentsOfDirectory(atPath: source.path) else { return }
        let chosen = names.filter(wanted)
        guard !chosen.isEmpty else { return }
        try? files.createDirectory(at: target, withIntermediateDirectories: true)
        for name in chosen {
            try? files.copyItem(at: source.appendingPathComponent(name), to: target.appendingPathComponent(name))
        }
    }

    @discardableResult
    private static func run(_ tool: String, _ arguments: String...) -> String {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: tool)
        process.arguments = arguments
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = pipe
        do {
            try process.run()
        } catch {
            return "\(tool): \(error.localizedDescription)"
        }
        let output = pipe.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        return String(decoding: output, as: UTF8.self)
    }
}
