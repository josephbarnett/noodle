//! SNI-keyed MITM-or-tunnel decision service.
//!
//! Sits behind `PeekTlsClientHelloService` and decides per-flow
//! whether to terminate TLS (MITM) or just byte-tunnel (no
//! termination, client speaks directly to upstream).
//!
//! Decision rule: SNI exactly matches a host in
//! [`hostname_filter::AI_PROVIDER_HOSTNAMES`] → MITM. Anything else
//! (including no SNI at all) → tunnel through unchanged.

use rama::{
    Service,
    error::BoxError,
    extensions,
    io::{BridgeIo, Io},
    net::{proxy::IoForwardService, tls::server::InputWithClientHello},
    telemetry::tracing,
    tls::boring::{
        TlsStream,
        proxy::{TlsMitmRelayService, cert_issuer::BoringMitmCertIssuer},
    },
};

use crate::hostname_filter::AI_PROVIDER_HOSTNAMES;

/// MITM iff SNI is on the AI provider allowlist; otherwise byte-tunnel.
///
/// Constructed once per handler (cheap clones share the relay's
/// internal `Arc`s) and passed in as the inner service of
/// [`rama::net::tls::server::PeekTlsClientHelloService`].
#[derive(Clone)]
pub struct MitmAllowlistService<Issuer, Inner> {
    mitm: TlsMitmRelayService<Issuer, Inner>,
    tunnel: IoForwardService,
}

impl<Issuer, Inner> MitmAllowlistService<Issuer, Inner> {
    pub fn new(mitm: TlsMitmRelayService<Issuer, Inner>) -> Self {
        Self {
            mitm,
            tunnel: IoForwardService::default(),
        }
    }
}

impl<Issuer, Inner, Ingress, Egress> Service<InputWithClientHello<BridgeIo<Ingress, Egress>>>
    for MitmAllowlistService<Issuer, Inner>
where
    Issuer: BoringMitmCertIssuer<Error: Into<BoxError>>,
    Inner: Service<BridgeIo<TlsStream<Ingress>, TlsStream<Egress>>, Output = (), Error: Into<BoxError>>,
    Ingress: Io + Unpin + extensions::ExtensionsRef,
    Egress: Io + Unpin + extensions::ExtensionsRef,
{
    type Output = ();
    type Error = BoxError;

    async fn serve(
        &self,
        input: InputWithClientHello<BridgeIo<Ingress, Egress>>,
    ) -> Result<Self::Output, Self::Error> {
        let sni = input.client_hello.ext_server_name().cloned();
        if matches_allowlist(sni.as_ref().map(rama::net::address::Domain::as_str)) {
            tracing::info!(
                sni = %sni.as_ref().map_or("<none>", |d| d.as_str()),
                "MITM allowlist hit — terminating TLS"
            );
            return self.mitm.serve(input).await.map_err(Into::into);
        }
        tracing::debug!(
            sni = %sni.as_ref().map_or("<none>", |d| d.as_str()),
            "MITM allowlist miss — transparent tunnel"
        );
        self.tunnel.serve(input.input).await
    }
}

/// `true` when `sni` exactly matches one of the AI provider hostnames
/// (case-insensitive, trailing-dot tolerant). `None` always returns
/// `false` — no SNI means we cannot identify the upstream and so we
/// pass through.
#[must_use]
pub fn matches_allowlist(sni: Option<&str>) -> bool {
    let Some(sni) = sni else { return false };
    let sni = sni.trim_end_matches('.');
    AI_PROVIDER_HOSTNAMES
        .iter()
        .any(|allowed| sni.eq_ignore_ascii_case(allowed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_sni_is_on_allowlist() {
        assert!(matches_allowlist(Some("api.anthropic.com")));
    }

    #[test]
    fn openai_sni_is_on_allowlist() {
        assert!(matches_allowlist(Some("api.openai.com")));
    }

    #[test]
    fn case_insensitive() {
        assert!(matches_allowlist(Some("API.ANTHROPIC.COM")));
    }

    #[test]
    fn trailing_dot_tolerated() {
        assert!(matches_allowlist(Some("api.anthropic.com.")));
    }

    #[test]
    fn unrelated_sni_misses() {
        assert!(!matches_allowlist(Some("api.github.com")));
        assert!(!matches_allowlist(Some("google.com")));
    }

    #[test]
    fn unrelated_subdomain_of_provider_misses() {
        // Strict equality only — the marketing site shouldn't be MITM'd.
        assert!(!matches_allowlist(Some("console.anthropic.com")));
    }

    #[test]
    fn missing_sni_misses() {
        assert!(!matches_allowlist(None));
    }
}
