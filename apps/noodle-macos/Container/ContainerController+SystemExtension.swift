import SystemExtensions

extension ContainerController {
    func ensureSystemExtensionActivated(completion: @escaping (Bool) -> Void) {
        systemExtensionActivationCompletions.append(completion)
        guard !systemExtensionActivationInFlight else {
            log("system extension activation already in flight")
            return
        }

        systemExtensionActivationInFlight = true
        log("submitting system extension activation request for \(extensionBundleId)")
        let request = OSSystemExtensionRequest.activationRequest(
            forExtensionWithIdentifier: extensionBundleId,
            queue: .main
        )
        request.delegate = self
        OSSystemExtensionManager.shared.submitRequest(request)
    }

    func finishSystemExtensionActivation(success: Bool, detail: String) {
        systemExtensionActivationInFlight = false
        let completions = systemExtensionActivationCompletions
        systemExtensionActivationCompletions.removeAll()
        log(detail)
        for completion in completions {
            completion(success)
        }
    }

    /// Submit `OSSystemExtensionRequest.deactivationRequest` for the
    /// embedded system extension. This is the only supported way to
    /// uninstall a system extension while SIP is enabled — only the
    /// container app that originally activated the extension can
    /// deactivate it.
    func requestDeactivateExtension(completion: @escaping (Bool) -> Void) {
        guard systemExtensionDeactivationCompletion == nil else {
            log("deactivation already in flight; ignoring duplicate request")
            completion(false)
            return
        }
        systemExtensionDeactivationCompletion = completion
        log("submitting system extension deactivation request for \(extensionBundleId)")
        let request = OSSystemExtensionRequest.deactivationRequest(
            forExtensionWithIdentifier: extensionBundleId,
            queue: .main
        )
        request.delegate = self
        OSSystemExtensionManager.shared.submitRequest(request)
    }
}

extension ContainerController: OSSystemExtensionRequestDelegate {
    func requestNeedsUserApproval(_ request: OSSystemExtensionRequest) {
        log("system extension approval required for \(request.identifier)")
        setStatus(status: .disconnected, detail: "approve system extension in System Settings")
    }

    func request(
        _ request: OSSystemExtensionRequest,
        actionForReplacingExtension existing: OSSystemExtensionProperties,
        withExtension ext: OSSystemExtensionProperties
    ) -> OSSystemExtensionRequest.ReplacementAction {
        log(
            "replacing system extension \(existing.bundleShortVersion) with \(ext.bundleShortVersion)"
        )
        return .replace
    }

    func request(
        _ request: OSSystemExtensionRequest,
        didFinishWithResult result: OSSystemExtensionRequest.Result
    ) {
        if let completion = systemExtensionDeactivationCompletion {
            systemExtensionDeactivationCompletion = nil
            log("system extension deactivation finished result=\(result.rawValue)")
            completion(true)
            return
        }
        finishSystemExtensionActivation(
            success: true,
            detail: "system extension activation finished with result=\(result.rawValue)"
        )
    }

    func request(_ request: OSSystemExtensionRequest, didFailWithError error: Error) {
        if let completion = systemExtensionDeactivationCompletion {
            systemExtensionDeactivationCompletion = nil
            logError("system extension deactivation failed", error)
            completion(false)
            return
        }
        logError("system extension activation failed", error)
        finishSystemExtensionActivation(
            success: false, detail: "system extension activation failed")
    }
}
