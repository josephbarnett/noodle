import Foundation

/// Pure, unit-testable matcher for "is this a noodle MITM root CA?"
///
/// Extracted as a free function so the test target can exercise it
/// without dragging in `SecCertificate`, AppKit, or the rest of
/// `ContainerController`. The keychain-purge code in
/// `ContainerController+Keychain.swift` calls this with values it
/// pulled from `SecCertificateCopyCommonName` /
/// `SecCertificateCopySubjectSummary`.
///
/// Match rule (in order):
/// 1. CN equals `ca.noodleproxy.macos` (case-insensitive) — the
///    canonical CN minted by
///    `crates/noodle-macos-tproxy/src/tls.rs`.
/// 2. CN contains `noodleproxy` (case-insensitive) — catches
///    rotated CNs and legacy variants.
/// 3. Subject summary contains `noodle` (case-insensitive) —
///    catches CAs whose CN is missing or unusual but whose
///    summary still names noodle.
///
/// Any non-match returns `false`. **This function is what stands
/// between the uninstall pipeline and accidentally deleting an
/// unrelated trust root** — keep the rules tight and the tests
/// honest.
public func isNoodleCertificateAuthority(
    commonName: String?,
    subjectSummary: String?
) -> Bool {
    if let cn = commonName {
        if cn.caseInsensitiveCompare("ca.noodleproxy.macos") == .orderedSame {
            return true
        }
        if cn.localizedCaseInsensitiveContains("noodleproxy") {
            return true
        }
    }
    if let summary = subjectSummary,
        summary.localizedCaseInsensitiveContains("noodle")
    {
        return true
    }
    return false
}
