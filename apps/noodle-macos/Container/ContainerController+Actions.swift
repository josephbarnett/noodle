import AppKit

extension ContainerController {
    @objc func startProxyAction(_: Any?) {
        startProxy()
    }

    @objc func stopProxyAction(_: Any?) {
        stopProxy(completion: nil)
    }

    @objc func resetProfileAction(_: Any?) {
        resetProxyConfigurationAndStart()
    }

    @objc func rotateCAAction(_: Any?) {
        rotateMITMCAAndApply()
    }

    @objc func installCAAction(_: Any?) {
        installMITMCA()
    }

    @objc func clearCAAction(_: Any?) {
        clearCA()
    }

    @objc func pingProviderAction(_: Any?) {
        sendProviderPing()
    }

    @objc func toggleHtmlBadgeAction(_: Any?) {
        demoSettings.htmlBadgeEnabled.toggle()
        updateDemoSettingsMenu()
        applyDemoSettings()
    }

    @objc func editBadgeLabelAction(_: Any?) {
        guard
            let value = promptForText(
                title: "Badge Label",
                message: "Choose the HTML badge label shown on rewritten pages.",
                defaultValue: demoSettings.htmlBadgeLabel
            )?.trimmingCharacters(in: .whitespacesAndNewlines),
            !value.isEmpty
        else {
            return
        }

        demoSettings.htmlBadgeLabel = value
        updateDemoSettingsMenu()
        applyDemoSettings()
    }

    @objc func editExcludeDomainsAction(_: Any?) {
        let defaultValue = demoSettings.excludeDomains.joined(separator: ", ")
        guard
            let value = promptForText(
                title: "Excluded Domains",
                message: "Comma-separated domains that should bypass the demo MITM behavior.",
                defaultValue: defaultValue
            )
        else {
            return
        }

        let domains =
            value
            .split(separator: ",")
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty }
        demoSettings.excludeDomains = domains.isEmpty ? DemoProxySettings().excludeDomains : domains
        updateDemoSettingsMenu()
        applyDemoSettings()
    }

    @objc func resetDemoSettingsAction(_: Any?) {
        demoSettings = DemoProxySettings()
        updateDemoSettingsMenu()
        applyDemoSettings()
    }

    @objc func refreshAction(_: Any?) {
        refreshManagerAndStatus()
    }

    @objc func quitAction(_: Any?) {
        NSApplication.shared.terminate(nil)
    }

    /// CA-bundle env vars noodle points at its root CA. Single
    /// source of truth lives in `TrustEnvVars.names` so the install
    /// (set) and uninstall (unset) sides cannot drift. Re-exported
    /// here as a typealias so existing call sites keep working.
    static var caTrustEnvVarNames: [String] { TrustEnvVars.names }

    /// Run `launchctl setenv` for the standard CA-bundle env vars
    /// pointing at noodle's on-disk root CA. Affects every process
    /// launched after this point in the current user's launchd
    /// session (Finder, Dock, Spotlight, Terminal-spawned shells)
    /// — already-running processes do not inherit; relaunch them.
    ///
    /// CA path matches `CA_PEM_PATH` in
    /// `crates/noodle-macos-tproxy/src/tls.rs` — world-readable
    /// system-wide path, not under $HOME (the sysext runs as root
    /// and its `userDomainMask` resolves to /var/root, mode 0700).
    @objc func setTrustEnvVarsAction(_: Any?) {
        let caPath = "/Library/Application Support/noodle/macos-tproxy-ca.pem"

        guard FileManager.default.fileExists(atPath: caPath) else {
            let alert = NSAlert()
            alert.messageText = "Noodle CA Not Found"
            alert.informativeText = """
                Expected the sysext to have written its root CA cert to:
                  \(caPath)

                Make sure the proxy is running (status connected) and
                that the system extension has restarted at least once
                since this build was installed.
                """
            alert.alertStyle = .warning
            alert.addButton(withTitle: "OK")
            alert.runModal()
            return
        }

        let envVars = Self.caTrustEnvVarNames

        var failures: [String] = []
        for name in envVars {
            let task = Process()
            task.executableURL = URL(fileURLWithPath: "/bin/launchctl")
            task.arguments = ["setenv", name, caPath]
            do {
                try task.run()
                task.waitUntilExit()
                if task.terminationStatus != 0 {
                    failures.append("\(name) (exit \(task.terminationStatus))")
                }
            } catch {
                failures.append("\(name) (\(error.localizedDescription))")
            }
        }

        let alert = NSAlert()
        if failures.isEmpty {
            alert.messageText = "Trust Env Vars Set"
            alert.informativeText = """
                Set via `launchctl setenv` for this user session:
                  • \(envVars.joined(separator: "\n  • "))

                All pointing at:
                  \(caPath)

                Already-running processes do not inherit — relaunch
                Claude Code, Cursor, your terminal, etc. to pick up
                the new trust.

                Note: this lasts until you log out. Iteration 5 will
                install a LaunchAgent for persistence + register the
                CA with the System Keychain.
                """
            alert.alertStyle = .informational
        } else {
            alert.messageText = "Trust Env Vars Partially Set"
            alert.informativeText = """
                These failed:
                  • \(failures.joined(separator: "\n  • "))

                Others were set successfully. Check Console.app
                under launchd for details.
                """
            alert.alertStyle = .warning
        }
        alert.addButton(withTitle: "OK")
        alert.runModal()
    }

    @objc func uninstallAction(_: Any?) {
        let alert = NSAlert()
        alert.messageText = "Uninstall Noodle"
        alert.informativeText = """
            This will:
              • Stop the proxy and remove its saved configuration (the row in System Settings → Network → VPN & Filters).
              • Clear the MITM root CA from the keychain.
              • Deactivate the system extension.
              • Quit Noodle.

            The .app bundle in /Applications stays — delete it manually if you also want that gone.
            """
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Uninstall")
        alert.addButton(withTitle: "Cancel")
        guard alert.runModal() == .alertFirstButtonReturn else {
            return
        }
        performUninstall()
    }

    private func performUninstall() {
        // Orchestration lives in UninstallService; this controller
        // implements the `UninstallSteps` protocol below to provide the
        // production step collaborators.
        UninstallService(steps: self, log: { [weak self] in self?.log($0) }).run()
    }

    func promptForText(
        title: String,
        message: String,
        defaultValue: String
    ) -> String? {
        let alert = NSAlert()
        alert.messageText = title
        alert.informativeText = message
        alert.alertStyle = .informational
        alert.addButton(withTitle: "Save")
        alert.addButton(withTitle: "Cancel")

        let textField = NSTextField(string: defaultValue)
        textField.frame = NSRect(x: 0, y: 0, width: 320, height: 24)
        alert.accessoryView = textField

        guard alert.runModal() == .alertFirstButtonReturn else {
            return nil
        }

        return textField.stringValue
    }

    func showPingError(_ message: String) {
        let alert = NSAlert()
        alert.messageText = "Ping Failed"
        alert.informativeText = message
        alert.alertStyle = .critical
        alert.addButton(withTitle: "OK")
        alert.runModal()
    }

    func flashPingSuccess() {
        guard let button = statusItem?.button else { return }
        // Show a green dot next to the icon for ~1.5s, then clear.
        button.title = "🟢"
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) { [weak button] in
            button?.title = ""
        }
    }
}
