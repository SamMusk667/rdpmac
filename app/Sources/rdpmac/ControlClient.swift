import Foundation

enum ControlError: LocalizedError {
    case notRunning
    case io(String)
    case refused(String)

    var errorDescription: String? {
        switch self {
        case .notRunning: return "The rdpmac server is not running."
        case .io(let message): return "Talking to the server failed: \(message)"
        case .refused(let message): return message
        }
    }
}

/// One JSON request per line to rdpmacd's control socket, one JSON answer per line back.
struct ControlClient {
    static var socketPath: String {
        NSHomeDirectory() + "/Library/Application Support/rdpmac/control.sock"
    }

    /// Sends `request` and returns the answer's raw JSON; an answer with "ok": false throws.
    func send(_ request: [String: Any]) throws -> Data {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw ControlError.io(String(cString: strerror(errno))) }
        defer { close(fd) }

        var timeout = timeval(tv_sec: 5, tv_usec: 0)
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
        setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))

        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let path = Array(Self.socketPath.utf8)
        let capacity = MemoryLayout.size(ofValue: address.sun_path)
        guard path.count < capacity else { throw ControlError.io("socket path too long") }
        withUnsafeMutableBytes(of: &address.sun_path) { buffer in
            buffer.copyBytes(from: path)
            buffer[path.count] = 0
        }
        let connected = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard connected == 0 else { throw ControlError.notRunning }

        var line = try JSONSerialization.data(withJSONObject: request)
        line.append(0x0A)
        try line.withUnsafeBytes { buffer in
            var offset = 0
            while offset < buffer.count {
                let written = write(fd, buffer.baseAddress! + offset, buffer.count - offset)
                guard written > 0 else { throw ControlError.io(String(cString: strerror(errno))) }
                offset += written
            }
        }

        var reply = Data()
        var chunk = [UInt8](repeating: 0, count: 64 * 1024)
        while !reply.contains(0x0A) {
            let count = read(fd, &chunk, chunk.count)
            guard count > 0 else {
                throw ControlError.io(count == 0 ? "connection closed" : String(cString: strerror(errno)))
            }
            reply.append(contentsOf: chunk[0..<count])
        }
        if let newline = reply.firstIndex(of: 0x0A) {
            reply = reply[reply.startIndex..<newline]
        }
        if let answer = try? JSONDecoder().decode(Answer.self, from: reply), !answer.ok {
            throw ControlError.refused(answer.error ?? "The server refused the request.")
        }
        return reply
    }

    func status() throws -> DaemonStatus {
        try JSONDecoder().decode(DaemonStatus.self, from: send(["cmd": "status"]))
    }

    func fileSettings() throws -> DaemonSettings {
        try JSONDecoder().decode(ConfigAnswer.self, from: send(["cmd": "get_config"])).settings
    }

    func save(_ settings: DaemonSettings) throws {
        let encoded = try JSONSerialization.jsonObject(with: JSONEncoder().encode(settings))
        _ = try send(["cmd": "set_config", "settings": encoded])
    }

    func requestPermissions() throws {
        _ = try send(["cmd": "request_permissions"])
    }

    func importCertificate(certificate: String, key: String) throws {
        _ = try send(["cmd": "import_certificate", "cert_pem": certificate, "key_pem": key])
    }

    func restart() throws {
        _ = try send(["cmd": "restart"])
    }

    /// The server checks the password with PAM before it stores the account's NT hash.
    func enrollNla(password: String) throws {
        _ = try send(["cmd": "nla_enroll", "password": password])
    }

    func removeNla() throws {
        _ = try send(["cmd": "nla_remove"])
    }
}

private struct Answer: Decodable {
    let ok: Bool
    let error: String?
}

private struct ConfigAnswer: Decodable {
    let settings: DaemonSettings
}
