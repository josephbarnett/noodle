//! Stdout JSONL `WireSink` + a fan-out composite.
//!
//! Each sink owns its own display policy. `JsonStdoutLog` formats each
//! event as a one-line JSON object with a `body: { len, truncated, text }`
//! envelope (UTF-8 lossy, capped at 64 KiB). Other sinks (e.g.
//! `noodle-tap::TapJsonlLog`) format differently against the same
//! `WireEvent` data.

use std::io::Write;
use std::sync::Arc;

use noodle_core::{WireEvent, WireSink};
use serde_json::json;

/// Cap for the inline-text rendering of bodies in `JsonStdoutLog`. Larger
/// bodies are truncated; the original `len` is still reported.
pub const STDOUT_BODY_CAP: usize = 64 * 1024;

/// Writes one JSON object per line to stdout. Best effort: write errors
/// are logged via `tracing::warn!` and dropped — the proxy hot path
/// must never fail because of a logging hiccup.
pub struct JsonStdoutLog;

impl JsonStdoutLog {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for JsonStdoutLog {
    fn default() -> Self {
        Self::new()
    }
}

impl WireSink for JsonStdoutLog {
    fn record(&self, event: WireEvent) {
        let line = render_line(&event);
        let stdout = std::io::stdout();
        let mut h = stdout.lock();
        if let Err(e) = writeln!(h, "{line}") {
            tracing::warn!(?e, "wire log: failed to write line");
        }
    }
}

fn render_line(event: &WireEvent) -> String {
    let body_in_view = body_view(&event.body_in, STDOUT_BODY_CAP);
    let mut obj = serde_json::Map::new();
    obj.insert("direction".into(), json!(event.direction));
    obj.insert("request_id".into(), json!(event.request_id.as_str()));
    obj.insert("ts_unix_ms".into(), json!(event.ts_unix_ms));
    if let Some(m) = &event.method {
        obj.insert("method".into(), json!(m.as_str()));
    }
    if let Some(u) = &event.url {
        obj.insert("url".into(), json!(u));
    }
    if let Some(s) = event.status {
        obj.insert("status".into(), json!(s));
    }
    obj.insert("headers".into(), json!(event.headers));
    obj.insert("body".into(), body_in_view);
    // Surface the post-mutation view only when noodle changed
    // bytes on this direction — keeps passthrough lines compact
    // and makes mutations visible at a glance.
    if event.body_out != event.body_in {
        let body_out_view = body_view(&event.body_out, STDOUT_BODY_CAP);
        obj.insert("body_out".into(), body_out_view);
    }
    serde_json::Value::Object(obj).to_string()
}

fn body_view(bytes: &[u8], cap: usize) -> serde_json::Value {
    let len = bytes.len();
    let truncated = len > cap;
    let slice = if truncated { &bytes[..cap] } else { bytes };
    let text = std::str::from_utf8(slice).ok().map(str::to_owned);
    let hex = if text.is_none() {
        Some(to_hex(slice))
    } else {
        None
    };
    let mut obj = serde_json::Map::new();
    obj.insert("len".into(), json!(len));
    obj.insert("truncated".into(), json!(truncated));
    if let Some(t) = text {
        obj.insert("text".into(), json!(t));
    }
    if let Some(h) = hex {
        obj.insert("hex".into(), json!(h));
    }
    serde_json::Value::Object(obj)
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Fan-out composite. Records to every wrapped sink in registration order.
///
/// Composition lives here, not in the proxy config — operators wrap
/// multiple sinks into a single `Arc<dyn WireSink>` slot.
pub struct MultiWireSink {
    sinks: Vec<Arc<dyn WireSink>>,
}

impl MultiWireSink {
    #[must_use]
    pub fn new(sinks: Vec<Arc<dyn WireSink>>) -> Self {
        Self { sinks }
    }
}

impl WireSink for MultiWireSink {
    fn record(&self, event: WireEvent) {
        for s in &self.sinks {
            s.record(event.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use bytes::Bytes;
    use noodle_core::{HeaderPair, WireDirection, WireEvent};

    use super::*;

    struct Capture(Mutex<Vec<WireEvent>>);
    impl WireSink for Capture {
        fn record(&self, event: WireEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn ev(body: Bytes) -> WireEvent {
        WireEvent {
            direction: WireDirection::Request,
            request_id: "nl-1".into(),
            ts_unix_ms: 0,
            method: Some("GET".into()),
            url: Some("http://example.com/".into()),
            status: None,
            headers: vec![HeaderPair {
                name: "host".into(),
                value: "example.com".into(),
            }],
            body_in: body.clone(),
            body_out: body,
            marks: None,
            provider: None,
            agent_app: None,
            machine: None,
            collector_app: None,
            subscription: None,
            usage: None,
            content_blocks: None,
            events: None,
            pairing: None,
            attribution: None,
        }
    }

    #[test]
    fn multi_sink_fans_out() {
        let a = Arc::new(Capture(Mutex::new(vec![])));
        let b = Arc::new(Capture(Mutex::new(vec![])));
        let multi = MultiWireSink::new(vec![a.clone(), b.clone()]);
        multi.record(ev(Bytes::new()));
        assert_eq!(a.0.lock().unwrap().len(), 1);
        assert_eq!(b.0.lock().unwrap().len(), 1);
    }

    #[test]
    fn renders_request_with_text_body() {
        let line = render_line(&ev(Bytes::from_static(b"hello")));
        // Round-trip through serde_json::Value so the test isn't
        // coupled to serde's key ordering (which is alphabetical for
        // BTreeMap-backed Maps without the `preserve_order` feature).
        let v: serde_json::Value = serde_json::from_str(&line).expect("valid json");
        assert_eq!(v["direction"], "request");
        assert_eq!(v["method"], "GET");
        assert_eq!(v["url"], "http://example.com/");
        assert_eq!(v["body"]["len"], 5);
        assert_eq!(v["body"]["truncated"], false);
        assert_eq!(v["body"]["text"], "hello");
        // optional fields omitted when None
        assert!(v.get("status").is_none());
    }

    #[test]
    fn renders_non_utf8_body_as_hex() {
        let line = render_line(&ev(Bytes::from_static(&[0xff, 0xfe, 0xfd])));
        assert!(line.contains("\"hex\":\"fffefd\""));
        assert!(!line.contains("\"text\""));
    }

    #[test]
    fn truncates_oversized_bodies() {
        let big = vec![b'a'; STDOUT_BODY_CAP + 100];
        let line = render_line(&ev(Bytes::from(big)));
        assert!(line.contains(&format!("\"len\":{}", STDOUT_BODY_CAP + 100)));
        assert!(line.contains("\"truncated\":true"));
    }
}
