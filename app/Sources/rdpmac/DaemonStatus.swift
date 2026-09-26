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

    /// Enrollment for Network Level Authentication of the user the server runs as.
    struct Nla: Decodable {
        let user: String
        let enrolled: Bool?
        /// When the user enrolled, in seconds since 1970.
        let since: TimeInterval?
        /// Set when an earlier build of the server stored the enrollment, which this one may not
        /// read; `enrolled` is then false until the user enrolls again.
        let stale: Bool?
        /// Set when the keychain could not be read.
        let error: String?
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
    /// Absent when accounts are not checked with PAM, where enrollment does not apply.
    let nla: Nla?
    let configPath: String
    let dataDir: String
    let logDir: String

    enum CodingKeys: String, CodingKey {
        case version, pid, permissions, connection, displays, settings, certificate, nla
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
    var security: String?
    var pamService: String?
    var codec: String?
    var parallelConversion: Bool?
    var clipboard: Bool?
    var audio: Bool?
    var muteMac: Bool?
    /// Set by hand in the file; not shown, only kept when saving.
    var audioRate: Int?
    var resolution: String?
    var virtualDisplay: String?
    var fps: Int?
    var cursorHz: Int?
    var cert: String?
    var key: String?
    /// A diagnostic set by hand in the file; not shown, only kept when saving.
    var h264Dump: Bool?

    enum CodingKeys: String, CodingKey {
        case listen, auth, security, codec, clipboard, audio, resolution, fps, cert, key
        case pamService = "pam-service"
        case virtualDisplay = "virtual-display"
        case parallelConversion = "parallel-conversion"
        case cursorHz = "cursor-hz"
        case muteMac = "mute-mac"
        case audioRate = "audio-rate"
        case h264Dump = "h264-dump"
    }
}
