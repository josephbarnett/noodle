//! Host-supplied request input to [`crate::detect`].

use bytes::Bytes;
use smol_str::SmolStr;

/// Request bytes + metadata as the host gateway saw them on the
/// wire.
///
/// The facade does not parse `url` — it consumes `method`, `headers`,
/// and `body` and lets the per-provider detector handle URL pattern
/// matching against `host` + `path`.
#[derive(Debug, Clone)]
pub struct DetectRequest {
    /// HTTP method, e.g. `POST`.
    pub method: SmolStr,
    /// Hostname (no scheme, no port). Example: `api.anthropic.com`.
    pub host: SmolStr,
    /// Request path including query string, e.g. `/v1/messages`.
    pub path: SmolStr,
    /// Request headers as `(name, value)` pairs in arrival order.
    /// Header names lower-cased by convention; the facade does not
    /// normalise.
    pub headers: Vec<(SmolStr, SmolStr)>,
    /// Request body bytes. Empty for GET-shaped requests.
    pub body: Bytes,
}
