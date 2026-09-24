import Foundation

/// rdpmacd's answer to the `status` command.
struct DaemonStatus: Decodable {
    struct Permissions: Decodable {
        let screenRecording: Bool
        let accessibility: Bool

        enum CodingKeys: String, CodingKey {
            case screenRecording = "screen_recording"
            case accessibility
        }
    }

    struct Connection: Decodable {
        let peer: String
        let since: TimeInterval
        let user: String?
    }

    struct Ended: Decodable {
        let peer: String
        let user: String?
        let ended: TimeInterval
        let seconds: Int
        let error: String?
    }

    struct Display: Decodable {
        let id: UInt32
        let width: Int
        let height: Int
        let primary: Bool
        let placeholder: Bool
        let `virtual`: Bool
    }

    struct Certificate: Decodable {
        let path: String
        let sha1: String?
        let sha256: String?
    }

    let version: String
    let pid: Int32
    let permissions: Permissions
    /// A permission was granted after the server started; macOS applies it after a restart.
    let restartNeeded: Bool?
    let connection: Connection?
    let sessionSize: [Int]?
    let lastConnection: Ended?
    let displays: [Display]
    let virtualDisplaysSupported: Bool
    let settings: DaemonSettings
    let certificate: Certificate
    let configPath: String
    let dataDir: String
    let logDir: String

    enum CodingKeys: String, CodingKey {
        case version, pid, permissions, connection, displays, settings, certificate
        case restartNeeded = "restart_needed"
        case sessionSize = "session_size"
        case lastConnection = "last_connection"
        case virtualDisplaysSupported = "virtual_displays_supported"
        case configPath = "config_path"
        case dataDir = "data_dir"
        case logDir = "log_dir"
    }
}

/// The settings file, config.toml. Absent fields keep rdpmacd's defaults.
struct DaemonSettings: Codable, Equatable {
    var listen: String?
    var auth: String?
    var pamService: String?
    var codec: String?
    var clipboard: Bool?
    var resolution: String?
    var virtualDisplay: String?
    var fps: Int?
    var cursorHz: Int?
    var cert: String?
    var key: String?

    enum CodingKeys: String, CodingKey {
        case listen, auth, codec, clipboard, resolution, fps, cert, key
        case pamService = "pam-service"
        case virtualDisplay = "virtual-display"
        case cursorHz = "cursor-hz"
    }
}
