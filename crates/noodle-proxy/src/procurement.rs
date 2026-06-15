//! Cert procurement: pre-warm the leaf cache at startup.
//!
//! Per ADR 034 §2.5 and feature 038 §2 #6, the proxy can be
//! configured with a set of hosts to pre-mint leaves for at
//! startup. For external signers with non-trivial latency
//! (50–500 ms per CSR round trip), pre-warming amortizes the
//! first-connection cost across all configured hosts so the
//! initial agent connection hits a warm cache.
//!
//! ## Properties
//!
//! - **Best-effort.** Signer outages do not block startup. The
//!   task logs warnings and moves on; on-demand mint still
//!   happens on the hot path.
//! - **Background.** Runs as a detached `tokio` task so the
//!   serve loop spins up immediately.
//! - **Idempotent.** Re-running for an already-pre-warmed host
//!   is a no-op (the inner mint service is responsible for
//!   serving from its own cache when applicable).
//!
//! ## Wiring
//!
//! `noodle_proxy::start` calls [`spawn`] iff
//! `ProxyConfig::procurement_hosts` is `Some(non_empty_list)`.
//! The hot path is unaffected — the rama `CachedBoringMitmCertIssuer`
//! still single-flights mint requests, so procurement that
//! beats the first real request to the punch just primes the
//! cache; procurement that loses the race is harmless.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use noodle_core::{DynCertMintService, LeafRequest};

/// Result snapshot returned by the procurement task on join.
///
/// Tests use this to assert pre-warming behaviour for every
/// configured host. Production code does not consume this —
/// procurement is fire-and-forget.
#[derive(Debug)]
pub struct ProcurementOutcome {
    /// Hosts that were successfully pre-minted.
    pub succeeded: Vec<String>,
    /// Hosts where the mint failed (host + error string).
    pub failed: Vec<(String, String)>,
}

impl ProcurementOutcome {
    /// Total number of hosts attempted.
    #[must_use]
    pub fn attempted(&self) -> usize {
        self.succeeded.len() + self.failed.len()
    }
}

/// Lock-free counter of hosts pre-warmed so far across all
/// running procurement tasks. Test hook; not used by production.
static PREWARMED_HOSTS: AtomicUsize = AtomicUsize::new(0);

/// Spawn the procurement background task.
///
/// The task runs to completion in a detached `tokio::spawn`; the
/// proxy startup path does not wait on it. For per-host
/// timeouts, lean on the mint service's `signer_timeout` (the
/// `ExternalCertMintService::with_timeout` setting from ADR 034
/// §3.3) — procurement does not wrap an additional timeout
/// because the inner service is the source of truth.
///
/// Returns the `tokio::task::JoinHandle` so callers (mainly
/// tests) can `await` completion + inspect the
/// [`ProcurementOutcome`].
pub fn spawn(
    svc: Arc<dyn DynCertMintService>,
    hosts: Vec<String>,
) -> tokio::task::JoinHandle<ProcurementOutcome> {
    tokio::spawn(async move {
        let mut succeeded = Vec::new();
        let mut failed = Vec::new();
        for host in hosts {
            let req = LeafRequest::new(
                host.clone(),
                vec![host.clone()],
                Some(host.clone()),
                Vec::new(),
            );
            match svc.mint_leaf_boxed(req).await {
                Ok(_leaf) => {
                    PREWARMED_HOSTS.fetch_add(1, Ordering::Relaxed);
                    tracing::info!(host = %host, "procurement: pre-minted leaf");
                    succeeded.push(host);
                }
                Err(e) => {
                    tracing::warn!(host = %host, error = %e, "procurement: leaf mint failed");
                    failed.push((host, e.to_string()));
                }
            }
        }
        tracing::info!(
            succeeded = succeeded.len(),
            failed = failed.len(),
            "procurement complete"
        );
        ProcurementOutcome { succeeded, failed }
    })
}

/// Cumulative count of hosts pre-warmed by all procurement
/// tasks since process start. Test hook.
#[must_use]
pub fn prewarmed_count() -> usize {
    PREWARMED_HOSTS.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use noodle_core::{CertMintService, LeafCert, MintError};
    use std::sync::Mutex;

    /// Counting mint service for tests. Each call records the
    /// host and returns a synthetic leaf.
    struct CountingMint {
        calls: Mutex<Vec<String>>,
        fail_for: Option<String>,
    }

    impl CountingMint {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail_for: None,
            }
        }

        fn with_failure(host: &str) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail_for: Some(host.to_string()),
            }
        }

        fn hosts(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CertMintService for CountingMint {
        async fn mint_leaf(&self, request: LeafRequest) -> Result<LeafCert, MintError> {
            self.calls.lock().unwrap().push(request.server_name.clone());
            if let Some(ref fh) = self.fail_for
                && fh == &request.server_name
            {
                return Err(MintError::SignerUnavailable("test-induced".into()));
            }
            Ok(LeafCert::from_leaf(vec![0x30u8; 10], vec![0u8; 16]))
        }
    }

    #[tokio::test]
    async fn procurement_mints_one_leaf_per_host() {
        let svc = Arc::new(CountingMint::new());
        let hosts = vec![
            "api.anthropic.com".to_string(),
            "api.openai.com".to_string(),
            "console.anthropic.com".to_string(),
        ];
        let handle = spawn(
            Arc::clone(&svc) as Arc<dyn DynCertMintService>,
            hosts.clone(),
        );
        let outcome = handle.await.expect("join procurement task");
        assert_eq!(outcome.succeeded.len(), 3, "all hosts succeed");
        assert!(outcome.failed.is_empty());
        let observed = svc.hosts();
        for h in &hosts {
            assert!(
                observed.contains(h),
                "procurement must call mint for {h}; got {observed:?}"
            );
        }
    }

    #[tokio::test]
    async fn procurement_continues_after_per_host_failure() {
        let svc = Arc::new(CountingMint::with_failure("api.openai.com"));
        let hosts = vec![
            "api.anthropic.com".to_string(),
            "api.openai.com".to_string(),
            "console.anthropic.com".to_string(),
        ];
        let handle = spawn(
            Arc::clone(&svc) as Arc<dyn DynCertMintService>,
            hosts.clone(),
        );
        let outcome = handle.await.expect("join");
        assert_eq!(outcome.attempted(), 3);
        assert_eq!(outcome.succeeded.len(), 2);
        assert_eq!(outcome.failed.len(), 1);
        assert_eq!(outcome.failed[0].0, "api.openai.com");
        // All three hosts were attempted — failure doesn't stop the loop.
        let observed = svc.hosts();
        assert_eq!(observed.len(), 3);
    }

    #[tokio::test]
    async fn procurement_handles_empty_host_list() {
        let svc = Arc::new(CountingMint::new());
        let handle = spawn(Arc::clone(&svc) as Arc<dyn DynCertMintService>, vec![]);
        let outcome = handle.await.expect("join");
        assert_eq!(outcome.attempted(), 0);
        assert!(svc.hosts().is_empty());
    }
}
