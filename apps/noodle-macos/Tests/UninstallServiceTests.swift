import XCTest

/// Tests for `UninstallService` — verifies that the orchestration
/// runs every step in the correct order and that failures in
/// individual steps do not abort the pipeline.
///
/// The real step collaborators touch Network Extension, Keychain,
/// UserDefaults, and AppKit. These tests use a `StepRecorder` fake
/// that records the call order without touching any of that state.
final class UninstallServiceTests: XCTestCase {
    // MARK: - Happy path

    func test_runs_every_step_in_documented_order() {
        let recorder = StepRecorder()
        let logged = LoggedMessages()

        UninstallService(steps: recorder, log: logged.append).run()

        XCTAssertEqual(
            recorder.events,
            [
                "stopProxyForUninstall",
                "removeAllManagersForUninstall",
                "clearKeychainCAForUninstall",
                "clearTrustEnvVarsForUninstall",
                "clearUserDefaultsForUninstall",
                "deactivateSystemExtensionForUninstall",
                "trashAppBundleForUninstall",
                "quitForUninstall",
            ]
        )
    }

    func test_each_step_is_called_exactly_once() {
        let recorder = StepRecorder()
        UninstallService(steps: recorder, log: { _ in }).run()

        for event in recorder.events {
            XCTAssertEqual(
                recorder.events.filter { $0 == event }.count,
                1,
                "\(event) should be called exactly once"
            )
        }
    }

    func test_emits_a_log_line_for_every_step() {
        let recorder = StepRecorder()
        let logged = LoggedMessages()

        UninstallService(steps: recorder, log: logged.append).run()

        let stepHeaders = logged.messages.filter { $0.contains("step ") }
        // 8 numbered steps in the pipeline.
        XCTAssertEqual(stepHeaders.count, 8)
        XCTAssertTrue(stepHeaders[0].contains("step 1/8"))
        XCTAssertTrue(stepHeaders.last!.contains("step 8/8"))
    }

    // MARK: - Failure handling

    func test_deactivation_failure_still_reaches_trash_and_quit() {
        let recorder = StepRecorder(deactivationSucceeds: false)
        let logged = LoggedMessages()

        UninstallService(steps: recorder, log: logged.append).run()

        XCTAssertTrue(recorder.events.contains("trashAppBundleForUninstall"))
        XCTAssertEqual(recorder.events.last, "quitForUninstall")
        XCTAssertTrue(
            logged.messages.contains(where: { $0.contains("deactivation request failed") }),
            "Service should log when deactivation fails"
        )
    }

    func test_deactivation_success_is_logged() {
        let recorder = StepRecorder(deactivationSucceeds: true)
        let logged = LoggedMessages()

        UninstallService(steps: recorder, log: logged.append).run()

        XCTAssertTrue(
            logged.messages.contains(where: { $0.contains("deactivation completed") })
        )
    }

    // MARK: - Order invariants

    func test_proxy_is_stopped_before_managers_are_removed() {
        let recorder = StepRecorder()
        UninstallService(steps: recorder, log: { _ in }).run()

        guard
            let stopIdx = recorder.events.firstIndex(of: "stopProxyForUninstall"),
            let removeIdx = recorder.events.firstIndex(of: "removeAllManagersForUninstall")
        else {
            return XCTFail("expected both events in the record")
        }
        XCTAssertLessThan(stopIdx, removeIdx)
    }

    func test_bundle_is_trashed_before_quit() {
        let recorder = StepRecorder()
        UninstallService(steps: recorder, log: { _ in }).run()

        guard
            let trashIdx = recorder.events.firstIndex(of: "trashAppBundleForUninstall"),
            let quitIdx = recorder.events.firstIndex(of: "quitForUninstall")
        else {
            return XCTFail("expected both events in the record")
        }
        XCTAssertLessThan(trashIdx, quitIdx)
    }
}

// MARK: - Fakes

/// Records call order. All async-style callbacks fire synchronously
/// so the assertions in the tests can be made on the same tick.
private final class StepRecorder: UninstallSteps {
    var events: [String] = []
    let deactivationSucceeds: Bool

    init(deactivationSucceeds: Bool = true) {
        self.deactivationSucceeds = deactivationSucceeds
    }

    func stopProxyForUninstall(completion: @escaping () -> Void) {
        events.append("stopProxyForUninstall")
        completion()
    }

    func removeAllManagersForUninstall(completion: @escaping () -> Void) {
        events.append("removeAllManagersForUninstall")
        completion()
    }

    func clearKeychainCAForUninstall() {
        events.append("clearKeychainCAForUninstall")
    }

    func clearTrustEnvVarsForUninstall() {
        events.append("clearTrustEnvVarsForUninstall")
    }

    func clearUserDefaultsForUninstall() {
        events.append("clearUserDefaultsForUninstall")
    }

    func deactivateSystemExtensionForUninstall(completion: @escaping (Bool) -> Void) {
        events.append("deactivateSystemExtensionForUninstall")
        completion(deactivationSucceeds)
    }

    func trashAppBundleForUninstall(completion: @escaping () -> Void) {
        events.append("trashAppBundleForUninstall")
        completion()
    }

    func quitForUninstall() {
        events.append("quitForUninstall")
    }
}

private final class LoggedMessages {
    private(set) var messages: [String] = []
    func append(_ message: String) { messages.append(message) }
}
