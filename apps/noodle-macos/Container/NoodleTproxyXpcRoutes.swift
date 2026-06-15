import Foundation
import RamaAppleXpcClient

/// Typed XPC routes exposed by the sysext's router (Rust side: a
/// future `demo_xpc_server` module in `crates/noodle-macos-tproxy`,
/// not yet wired in iteration 2). Selectors, field names, and
/// shapes must stay in sync with the Rust `serde` types on each
/// route once they exist.

enum NoodleTproxyUpdateSettings: RamaXpcRoute {
    static let selector = "updateSettings:withReply:"

    struct Request: Encodable {
        let html_badge_enabled: Bool?
        let html_badge_label: String?
        let exclude_domains: [String]?
    }

    struct Reply: Decodable {
        let ok: Bool
    }
}

enum NoodleTproxyInstallRootCA: RamaXpcRoute {
    static let selector = "installRootCA:withReply:"
    typealias Reply = NoodleTproxyRootCaReply
}

enum NoodleTproxyUninstallRootCA: RamaXpcRoute {
    static let selector = "uninstallRootCA:withReply:"
    typealias Reply = NoodleTproxyRootCaReply
}

enum NoodleTproxyRotateRootCA: RamaXpcRoute {
    static let selector = "rotateRootCA:withReply:"

    struct Reply: Decodable {
        let ok: Bool
        let error: String?
        let previous_cert_der_b64: String?
        let new_cert_der_b64: String?
    }
}

/// Shared reply for install/uninstall (matches Rust `RootCaCommandReply`).
struct NoodleTproxyRootCaReply: Decodable {
    let ok: Bool
    let error: String?
    let cert_der_b64: String?
}
