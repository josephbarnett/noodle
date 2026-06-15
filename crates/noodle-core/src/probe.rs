#![allow(deprecated)]
// A.8.a: this module defines or implements legacy ProviderCodec types; the deprecation warning is the signal for external callers, not this internal impl. Removal under A.8.b.

//! Request and response probe types.
//!
//! Cheap, read-only views the engine builds before deciding which
//! `ProviderCodec` (or `Detector`) to run. Decoupled from the rama
//! HTTP service stack — the driving adapter constructs these from
//! `http::Request`/`http::Response` at the boundary.

use http::{HeaderMap, Method, StatusCode, Uri};

/// Cheap, read-only view of a request used for codec matching,
/// directive enhancement, and detector flow resolution. Avoids
/// consuming the body until we've decided who wants it.
pub struct RequestProbe<'a> {
    pub method: &'a Method,
    pub uri: &'a Uri,
    pub headers: &'a HeaderMap,
}

/// Static description of a response shape. Codecs use it to pick a
/// decode strategy (SSE framing vs. one-shot JSON, etc.) without
/// re-sniffing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseShape {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub kind: ResponseKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseKind {
    /// `text/event-stream`, line-delimited.
    Sse,
    /// Single JSON document.
    JsonOnce,
    /// Anything else — pass through, no decoding.
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_kind_distinct_variants() {
        assert_eq!(ResponseKind::Sse, ResponseKind::Sse);
        assert_ne!(ResponseKind::Sse, ResponseKind::JsonOnce);
        assert_ne!(ResponseKind::JsonOnce, ResponseKind::Other);
    }

    #[test]
    fn response_shape_equality() {
        let a = ResponseShape {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            kind: ResponseKind::Sse,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn response_shape_inequality_on_kind() {
        let a = ResponseShape {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            kind: ResponseKind::Sse,
        };
        let b = ResponseShape {
            kind: ResponseKind::JsonOnce,
            ..a.clone()
        };
        assert_ne!(a, b);
    }

    #[test]
    fn request_probe_borrows_in_place() {
        let method = Method::POST;
        let uri: Uri = "https://api.openai.com/v1/chat".parse().unwrap();
        let headers = HeaderMap::new();
        let probe = RequestProbe {
            method: &method,
            uri: &uri,
            headers: &headers,
        };
        assert_eq!(probe.method, &Method::POST);
        assert_eq!(probe.uri.host(), Some("api.openai.com"));
    }
}
