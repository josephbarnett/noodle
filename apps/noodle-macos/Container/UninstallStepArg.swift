import Foundation

/// One CLI arg = one uninstall step = one independently runnable
/// command. Run + verify each in isolation, e.g.:
///   open -n /Applications/Noodle.app --args --uninstall-deactivate-sysext
/// then check `systemextensionsctl list`. None of these CLI args
/// activate the sysext — the `--uninstall*` entry points in
/// `main.swift` bypass `ensureSystemExtensionActivated` so the
/// uninstall doesn't fight its own re-activation.
///
/// Extracted into its own file so the test target can verify the
/// rawValue mapping without dragging `main.swift` (and its
/// module-scope `app.run()`) into the test bundle.
public enum UninstallStepArg: String, CaseIterable {
    case stopProxy = "--uninstall-stop-proxy"
    case removeManagers = "--uninstall-remove-managers"
    case purgeCA = "--uninstall-purge-ca"
    case unsetEnv = "--uninstall-unset-env"
    case clearDefaults = "--uninstall-clear-defaults"
    case deactivateSysext = "--uninstall-deactivate-sysext"
    case trashApp = "--uninstall-trash-app"
}
