import Foundation

/// Step collaborators for the uninstall pipeline. Defined as a protocol
/// so the orchestration can be unit-tested with fakes that record call
/// order without touching real Network Extension, Keychain, or AppKit
/// state.
///
/// `ContainerController` provides the production implementation via an
/// extension; tests provide a recording fake.
protocol UninstallSteps: AnyObject {
    /// Stop the running transparent proxy. Calls `completion` when the
    /// connection has been signaled to shut down (does not wait for full
    /// teardown — the manager-removal step handles the rest).
    func stopProxyForUninstall(completion: @escaping () -> Void)

    /// Load every saved `NETransparentProxyManager` profile and remove
    /// it from system preferences. Clears the row in System Settings →
    /// Network → VPN & Filters.
    func removeAllManagersForUninstall(completion: @escaping () -> Void)

    /// Remove the MITM root CA from the keychain. No-op if the proxy is
    /// already inactive (the keychain helper guards on provider state).
    func clearKeychainCAForUninstall()

    /// `launchctl unsetenv` the CA-bundle trust env vars the install/
    /// menu flow set to point at noodle's root CA. Without this every
    /// process launched after "uninstall" keeps trusting a stale
    /// noodle CA path until the user logs out — the incomplete-cleanup
    /// bug. Best-effort; failures are logged, pipeline continues.
    func clearTrustEnvVarsForUninstall()

    /// Wipe NSUserDefaults for this app's bundle ID and its app-group
    /// suite.
    func clearUserDefaultsForUninstall()

    /// Submit `OSSystemExtensionRequest.deactivationRequest` for the
    /// embedded extension. `completion(true)` on success, `(false)` on
    /// any failure — the uninstall pipeline continues either way.
    func deactivateSystemExtensionForUninstall(completion: @escaping (Bool) -> Void)

    /// Move `/Applications/Noodle.app` to the Trash via
    /// `NSWorkspace.recycle`. `completion()` regardless of whether the
    /// recycle succeeded — failure is logged but the pipeline always
    /// terminates the app.
    func trashAppBundleForUninstall(completion: @escaping () -> Void)

    /// Terminate the app. Final step.
    func quitForUninstall()
}

/// Orchestrates the uninstall sequence. Each step is called via the
/// injected `steps` collaborator. Failures inside individual steps do
/// not abort the pipeline — partial progress is worse than partial
/// cleanup, so we always reach the terminal `quit`.
final class UninstallService {
    private let steps: UninstallSteps
    private let log: (String) -> Void

    init(steps: UninstallSteps, log: @escaping (String) -> Void) {
        self.steps = steps
        self.log = log
    }

    func run() {
        log("uninstall: step 1/8 — stopping proxy")
        steps.stopProxyForUninstall { [weak self] in
            guard let self else { return }
            self.log("uninstall: step 2/8 — removing saved transparent-proxy managers")
            self.steps.removeAllManagersForUninstall { [weak self] in
                guard let self else { return }
                self.log("uninstall: step 3/8 — clearing MITM CA from keychain")
                self.steps.clearKeychainCAForUninstall()
                self.log("uninstall: step 4/8 — unsetting CA-bundle trust env vars")
                self.steps.clearTrustEnvVarsForUninstall()
                self.log("uninstall: step 5/8 — clearing UserDefaults")
                self.steps.clearUserDefaultsForUninstall()
                self.log("uninstall: step 6/8 — submitting system extension deactivation request")
                self.steps.deactivateSystemExtensionForUninstall { [weak self] success in
                    guard let self else { return }
                    if success {
                        self.log("uninstall: deactivation completed")
                    } else {
                        self.log("uninstall: deactivation request failed; continuing")
                    }
                    self.log("uninstall: step 7/8 — moving app bundle to Trash")
                    self.steps.trashAppBundleForUninstall { [weak self] in
                        guard let self else { return }
                        self.log("uninstall: step 8/8 — quitting")
                        self.steps.quitForUninstall()
                    }
                }
            }
        }
    }
}
