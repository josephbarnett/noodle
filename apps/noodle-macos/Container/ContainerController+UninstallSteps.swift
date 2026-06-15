import AppKit
import Foundation

/// Production implementations of the `UninstallSteps` protocol.
/// Behavior matches the pre-refactor `performUninstall` exactly;
/// extracted here so `UninstallService` can be unit-tested with fakes.
extension ContainerController: UninstallSteps {
    func stopProxyForUninstall(completion: @escaping () -> Void) {
        stopProxy(completion: completion)
    }

    func removeAllManagersForUninstall(completion: @escaping () -> Void) {
        loadAndRemoveAllProxyManagers { _ in completion() }
    }

    func clearKeychainCAForUninstall() {
        // NOT clearCA(): that guards on isProviderActive() + needs
        // XPC to a running provider, but the pipeline stopped the
        // proxy in step 1 — so clearCA() no-ops and the CA is never
        // removed. This purge is provider-independent and loops
        // until every accumulated noodle CA is gone.
        purgeAllNoodleCAsForUninstall()
    }

    func clearTrustEnvVarsForUninstall() {
        // Mirror of `setTrustEnvVarsAction` — `launchctl unsetenv`
        // every var that flow set. Delegates to the free function
        // `unsetTrustEnvVars` in `TrustEnvVars.swift` (covered by
        // unit tests with a fake `EnvUnsetter`). Best-effort: a
        // failure on one var is logged and the rest still run; the
        // pipeline never aborts.
        unsetTrustEnvVars(log: { [weak self] in self?.log($0) })
    }

    func clearUserDefaultsForUninstall() {
        if let bundleID = Bundle.main.bundleIdentifier {
            UserDefaults.standard.removePersistentDomain(forName: bundleID)
        }
        // App-group suite used to share state with the system extension.
        if let appGroupID = Bundle.main.object(forInfoDictionaryKey: "APP_GROUP_ID") as? String,
            let grouped = UserDefaults(suiteName: appGroupID)
        {
            grouped.removePersistentDomain(forName: appGroupID)
        }
    }

    func deactivateSystemExtensionForUninstall(completion: @escaping (Bool) -> Void) {
        requestDeactivateExtension(completion: completion)
    }

    func trashAppBundleForUninstall(completion: @escaping () -> Void) {
        // NSWorkspace.recycle moves to Trash without admin when the user
        // owns the bundle (the normal `ditto`-from-derived-data install
        // path satisfies that). Failure is logged but never blocks the
        // pipeline — the next step always runs.
        let bundleURL = Bundle.main.bundleURL
        NSWorkspace.shared.recycle([bundleURL]) { [weak self] _, error in
            if let error {
                self?.logError("uninstall: NSWorkspace.recycle failed", error)
            } else {
                self?.log("uninstall: app bundle moved to Trash")
            }
            DispatchQueue.main.async { completion() }
        }
    }

    func quitForUninstall() {
        NSApplication.shared.terminate(nil)
    }
}
