//! Host-supplied response input to [`crate::detect`].

use bytes::Bytes;
use smol_str::SmolStr;

/// Response bytes + metadata as the host gateway saw them coming
/// back from the upstream provider.
///
/// The facade accepts a fully-materialised response body. Streaming
/// (per-frame) detection is a planned follow-up; for the v1 facade,
/// SSE responses should be buffered by the host before invoking
/// [`crate::detect`].
#[derive(Debug, Clone)]
pub struct DetectResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers as `(name, value)` pairs in arrival order.
    pub headers: Vec<(SmolStr, SmolStr)>,
    /// Response body bytes.
    pub body: Bytes,
}
