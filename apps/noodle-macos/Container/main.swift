import AppKit
import Foundation
import NetworkExtension
import OSLog

// UninstallStepArg lives in `UninstallStepArg.swift` so the test
// target can exercise its rawValue mapping without dragging
// `main.swift` (and its module-scope `app.run()`) into the test
// bundle.

final class ContainerController: NSObject, NSApplicationDelegate {
    lazy var xpcServiceName: String = {
        return Bundle.main.object(
            forInfoDictionaryKey: "ProviderMachServiceName"
        ) as? String ?? ""
    }()

    lazy var extensionBundleId: String = {
        guard let bundleId = Bundle.main.bundleIdentifier, !bundleId.isEmpty else {
            return ""
        }
        return "\(bundleId).provider"
    }()

    let managerDescription = "Noodle Proxy"
    let managerServerAddress = "127.0.0.1"
    static let secretAccount = "com.noodleproxy.macos"
    static let secretServiceKeyPEM = "noodle-tproxy-demo-ca-key"
    static let secretServiceCertPEM = "noodle-tproxy-demo-ca-crt"
    /// Secure-Enclave-wrapped key blob stored next to the (now-encrypted)
    /// PEMs. The container cannot decrypt these but can still delete them
    /// by service name, which is what the rotate flow needs.
    static let secretServiceSEKey = "noodle-tproxy-demo-ca-se-key"
    static let secretServiceKeys = [
        secretServiceKeyPEM,
        secretServiceCertPEM,
        secretServiceSEKey,
    ]
    lazy var containerLogger = Logger(
        subsystem: "com.noodleproxy.macos", category: "container")
    lazy var logFileURL: URL = {
        let base = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Logs", isDirectory: true)
        return base.appendingPathComponent("Noodle.log")
    }()

    /// Set from `--uninstall` CLI arg. When true the app runs the
    /// uninstall pipeline and NOTHING else — crucially it never
    /// calls `ensureSystemExtensionActivated`, so the uninstall
    /// doesn't fight its own re-activation (the bug). Modeled on
    /// the proxy's `--uninstall` entrypoint.
    var isUninstallMode = false

    /// Single-step uninstall: one CLI arg runs exactly one step
    /// then quits, so each step is verifiable independently.
    var uninstallStep: UninstallStepArg?

    var statusItem: NSStatusItem?
    var statusMenuItem: NSMenuItem?
    var startMenuItem: NSMenuItem?
    var stopMenuItem: NSMenuItem?
    var badgeEnabledMenuItem: NSMenuItem?
    var badgeLabelMenuItem: NSMenuItem?
    var excludeDomainsMenuItem: NSMenuItem?
    var resetDemoSettingsMenuItem: NSMenuItem?
    var rotateCAMenuItem: NSMenuItem?
    var installCAMenuItem: NSMenuItem?
    var clearCAMenuItem: NSMenuItem?
    var pingProviderMenuItem: NSMenuItem?
    var resetMenuItem: NSMenuItem?

    var activeManager: NETransparentProxyManager?
    var statusObserver: NSObjectProtocol?
    var statusTimer: DispatchSourceTimer?
    var lastStatus: NEVPNStatus?
    var lastLoggedDisconnectSignature: String?
    var demoSettings = DemoProxySettings()
    /// True after demoSettings has been initialised from NE preferences at least once.
    /// Prevents subsequent loadOrCreateAndConfigureManager calls from overwriting in-memory
    /// settings with stale NE values (e.g. after an unexpected provider stop + restart).
    var settingsInitializedFromNE = false
    var systemExtensionActivationCompletions: [(Bool) -> Void] = []
    var systemExtensionActivationInFlight = false
    /// Set while a deactivation request is in flight. The shared
    /// `OSSystemExtensionRequestDelegate` callbacks fire this in
    /// preference to the activation path when non-nil.
    var systemExtensionDeactivationCompletion: ((Bool) -> Void)?
    lazy var resetProfileOnLaunch =
        ProcessInfo.processInfo.arguments.contains("--reset-profile-on-launch")
    lazy var cleanSecretsOnLaunch =
        ProcessInfo.processInfo.arguments.contains("--clean-secrets")

    /// Headless uninstall entrypoint (`--uninstall`). Runs the
    /// uninstall pipeline with a live run loop (so the
    /// OSSystemExtension deactivation `.main`-queue delegate
    /// callbacks complete before trash/quit) and NEVER activates
    /// the sysext. Mirrors the proxy's dedicated uninstall path.
    func runUninstallPipeline() {
        log("launched in --uninstall mode: NOT activating sysext")
        if let bid = Bundle.main.bundleIdentifier {
            let me = ProcessInfo.processInfo.processIdentifier
            for other in NSRunningApplication.runningApplications(
                withBundleIdentifier: bid)
            where other.processIdentifier != me {
                other.terminate()
            }
        }
        UninstallService(steps: self, log: { [weak self] in self?.log($0) })
            .run()
    }

    /// Run exactly one uninstall step, then quit. Never activates
    /// the sysext. Run loop stays live (app.run()) so the
    /// OSSystemExtension deactivation delegate callbacks complete
    /// for `--uninstall-deactivate-sysext`.
    func runSingleUninstallStep(_ step: UninstallStepArg) {
        log("single uninstall step \(step.rawValue): NOT activating sysext")
        let quit = { NSApplication.shared.terminate(nil) }
        switch step {
        case .stopProxy:
            stopProxyForUninstall(completion: quit)
        case .removeManagers:
            removeAllManagersForUninstall(completion: quit)
        case .purgeCA:
            clearKeychainCAForUninstall()
            quit()
        case .unsetEnv:
            clearTrustEnvVarsForUninstall()
            quit()
        case .clearDefaults:
            clearUserDefaultsForUninstall()
            quit()
        case .deactivateSysext:
            deactivateSystemExtensionForUninstall { _ in quit() }
        case .trashApp:
            trashAppBundleForUninstall(completion: quit)
        }
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        if let step = uninstallStep {
            runSingleUninstallStep(step)
            return
        }
        if isUninstallMode {
            runUninstallPipeline()
            return
        }
        setupStatusItem()
        log("container app launched")
        if cleanSecretsOnLaunch {
            log("launch flag detected: clearing MITM CA before start")
            clearCA()
        }
        if resetProfileOnLaunch {
            log("launch flag detected: resetting saved proxy profile before start")
        }
        ensureSystemExtensionActivated { [weak self] success in
            guard let self else { return }
            guard success else {
                self.setStatus(status: .invalid, detail: "system extension unavailable")
                return
            }
            self.startProxy(forceReinstall: self.resetProfileOnLaunch)
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        if let statusObserver {
            NotificationCenter.default.removeObserver(statusObserver)
        }
        statusTimer?.cancel()
        statusTimer = nil
        log("container app terminated")
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard let manager = activeManager else {
            return .terminateNow
        }

        switch manager.connection.status {
        case .connected, .connecting, .reasserting:
            log("quit requested: stopping proxy first")
            stopProxy { sender.reply(toApplicationShouldTerminate: true) }
            return .terminateLater
        default:
            return .terminateNow
        }
    }
}

extension Data {
    fileprivate var hexString: String {
        map { String(format: "%02x", $0) }.joined()
    }
}

extension String {
    var nilIfEmpty: String? {
        isEmpty ? nil : self
    }
}

let app = NSApplication.shared
let delegate = ContainerController()
app.delegate = delegate
delegate.uninstallStep = UninstallStepArg.allCases.first {
    CommandLine.arguments.contains($0.rawValue)
}
delegate.isUninstallMode = CommandLine.arguments.contains("--uninstall")
app.setActivationPolicy(.accessory)
app.run()
