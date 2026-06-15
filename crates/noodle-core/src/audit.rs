//! `AuditSink` port. Driven adapters: JSON-lines file, tracing-event, multi
//! (composite/fan-out).

use std::time::SystemTime;

use bytes::Bytes;
use smol_str::SmolStr;

use crate::{FinishReason, RoundTripId, SessionId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditEvent {
    Enhance {
        session: SessionId,
        adapter: SmolStr,
        ts: SystemTime,
    },
    TurnStart {
        session: SessionId,
        turn: RoundTripId,
        ts: SystemTime,
    },
    Redact {
        session: SessionId,
        turn: RoundTripId,
        marker: SmolStr,
        raw_bytes: Bytes,
        ts: SystemTime,
    },
    TurnEnd {
        session: SessionId,
        turn: RoundTripId,
        finish: FinishReason,
        ts: SystemTime,
    },
}

pub trait AuditSink: Send + Sync + 'static {
    /// Non-blocking. Implementations that need I/O must offload to a
    /// background task; the inspection path must never block on audit.
    fn record(&self, event: AuditEvent);
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use crate::session::SessionKey;

    fn sid() -> SessionId {
        SessionKey {
            auth_header: b"a",
            session_header: b"b",
        }
        .id()
    }

    #[test]
    fn enhance_event_carries_adapter_name() {
        let e = AuditEvent::Enhance {
            session: sid(),
            adapter: "openai".into(),
            ts: SystemTime::UNIX_EPOCH,
        };
        match e {
            AuditEvent::Enhance { adapter, .. } => assert_eq!(adapter.as_str(), "openai"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn variants_are_distinct() {
        let a = AuditEvent::Enhance {
            session: sid(),
            adapter: "openai".into(),
            ts: SystemTime::UNIX_EPOCH,
        };
        let b = AuditEvent::TurnStart {
            session: sid(),
            turn: RoundTripId::new("t1"),
            ts: SystemTime::UNIX_EPOCH,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn redact_records_marker_and_raw() {
        let e = AuditEvent::Redact {
            session: sid(),
            turn: RoundTripId::new("t1"),
            marker: "<<noodle:end>>".into(),
            raw_bytes: Bytes::from_static(b"<<noodle:end>>"),
            ts: SystemTime::UNIX_EPOCH,
        };
        let AuditEvent::Redact {
            marker, raw_bytes, ..
        } = e
        else {
            panic!("wrong variant");
        };
        assert_eq!(marker.as_str(), "<<noodle:end>>");
        assert_eq!(raw_bytes.as_ref(), b"<<noodle:end>>");
    }
}
