import Foundation

/// CA-bundle trust env vars + the `launchctl unsetenv` plumbing,
/// extracted from `ContainerController` so the test target can
/// exercise the unset logic without launching `/bin/launchctl`.
///
/// Two seams:
/// - `TrustEnvVars.names` — single source of truth for the env-var
///   name list. Both the install/menu flow (set) and the uninstall
///   pipeline (unset) read from here so the two sides cannot
///   drift.
/// - `EnvUnsetter` protocol + `LaunchctlUnsetter` default impl —
///   the unset operation is one method (`unsetenv(_ name:)`) and
///   the tests substitute a fake.

public enum TrustEnvVars {
    /// The five CA-bundle env vars noodle's install/menu flow
    /// points at its on-disk root CA. The uninstall pipeline must
    /// unset every one of these or processes launched after
    /// uninstall keep trusting a stale path until the user logs
    /// out.
    public static let names = [
        "NODE_EXTRA_CA_CERTS",  // Node.js, Electron, npm, Claude Code itself
        "REQUESTS_CA_BUNDLE",  // Python requests
        "SSL_CERT_FILE",  // OpenSSL-linked tools, Go
        "CURL_CA_BUNDLE",  // curl
        "AWS_CA_BUNDLE",  // aws-cli
    ]
}

/// Outcome of one `unsetenv` invocation. The unset orchestrator
/// uses this to log non-zero exits without aborting the rest of
/// the pipeline.
public enum EnvUnsetResult {
    case success
    case nonZeroExit(Int32)
    case launchFailed(Error)
}

/// Test seam for `launchctl unsetenv`. Production uses
/// `LaunchctlUnsetter`; tests substitute a recorder.
public protocol EnvUnsetter {
    func unsetenv(_ name: String) -> EnvUnsetResult
}

/// Default impl — invokes `/bin/launchctl unsetenv <name>` in a
/// fresh `Process`. Mirrors the behaviour of the original inline
/// loop in `clearTrustEnvVarsForUninstall`.
public struct LaunchctlUnsetter: EnvUnsetter {
    public init() {}

    public func unsetenv(_ name: String) -> EnvUnsetResult {
        let task = Process()
        task.executableURL = URL(fileURLWithPath: "/bin/launchctl")
        task.arguments = ["unsetenv", name]
        do {
            try task.run()
            task.waitUntilExit()
            if task.terminationStatus == 0 {
                return .success
            }
            return .nonZeroExit(task.terminationStatus)
        } catch {
            return .launchFailed(error)
        }
    }
}

/// Unset every name in `names` via `unsetter`. Best-effort: a
/// failure on one var is logged through `log` and the rest still
/// run; this function never throws and never aborts. Returns the
/// per-name results so the caller (or a test) can assert on each.
@discardableResult
public func unsetTrustEnvVars(
    names: [String] = TrustEnvVars.names,
    unsetter: EnvUnsetter = LaunchctlUnsetter(),
    log: (String) -> Void = { _ in }
) -> [(name: String, result: EnvUnsetResult)] {
    var results: [(name: String, result: EnvUnsetResult)] = []
    for name in names {
        let result = unsetter.unsetenv(name)
        switch result {
        case .success:
            break
        case .nonZeroExit(let status):
            log("uninstall: launchctl unsetenv \(name) exit \(status)")
        case .launchFailed(let error):
            log(
                "uninstall: launchctl unsetenv \(name) failed: "
                    + "\(error.localizedDescription)")
        }
        results.append((name: name, result: result))
    }
    return results
}
