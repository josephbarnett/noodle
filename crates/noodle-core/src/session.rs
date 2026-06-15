//! Session model.
//!
//! A session is a noodle construct, not an LLM-API construct. Session
//! identity is derived from caller-supplied headers; we never invent one
//! per request. See `docs/features/005-session-and-directive-enhancement.md`.

use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use sha2::{Digest, Sha256};
use smol_str::SmolStr;

use crate::Resolved;

/// Opaque, hashed session identity. The full hash bytes are kept private;
/// only the first 8 hex chars surface in logs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId([u8; 32]);

impl SessionId {
    /// First 8 hex chars — safe for log correlation, not for matching.
    #[must_use]
    pub fn prefix(&self) -> SmolStr {
        let mut s = String::with_capacity(8);
        for b in &self.0[..4] {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
        }
        SmolStr::from(s)
    }
}

/// Inputs that derive a `SessionId`. Auth credential bytes are not stored,
/// only hashed.
pub struct SessionKey<'a> {
    pub auth_header: &'a [u8],
    pub session_header: &'a [u8],
}

impl SessionKey<'_> {
    #[must_use]
    pub fn id(&self) -> SessionId {
        let mut h = Sha256::new();
        // Domain-separation tag + length-prefixed framing prevents
        // collisions like ("ab", "") vs ("a", "b").
        h.update(b"noodle-session-v1\0");
        h.update((self.auth_header.len() as u64).to_le_bytes());
        h.update(self.auth_header);
        h.update((self.session_header.len() as u64).to_le_bytes());
        h.update(self.session_header);
        SessionId(h.finalize().into())
    }
}

/// Per-session state. Adapters and policies stash their own state inside
/// this via the `extensions` map (typed-keyed).
pub struct Session {
    pub id: SessionId,
    pub directive_enhanced: AtomicBool,
    /// Accumulated attribution record across the session's flows
    /// (ADR 020 §2.3). The engine wrapper merges each flow's
    /// `Resolved` map into this at flow end. Later flows override
    /// earlier ones for categories that collide — keeps with ADR
    /// 004's "most recent / max-confidence wins" framing.
    ///
    /// `Mutex` rather than `RwLock`: writes happen once per flow
    /// end (rare relative to reads from a debug viewer), and the
    /// update is a small `HashMap` merge. `Mutex` keeps the API
    /// simple.
    pub resolved: Mutex<Resolved>,
}

impl Session {
    #[must_use]
    pub fn new(id: SessionId) -> Self {
        Self {
            id,
            directive_enhanced: AtomicBool::new(false),
            resolved: Mutex::new(Resolved::default()),
        }
    }

    /// Merge a flow's `Resolved` map into the session's
    /// accumulated record. Later entries override earlier ones
    /// for colliding categories. Best-effort: if the mutex is
    /// poisoned, the entry is dropped rather than panicking
    /// (poisoning indicates a panicked thread mid-write, which
    /// is unrelated to attribution correctness).
    pub fn merge_resolved(&self, incoming: &Resolved) {
        let Ok(mut current) = self.resolved.lock() else {
            return;
        };
        for (category, value) in &incoming.0 {
            current.0.insert(category.clone(), value.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;

    fn key<'a>(auth: &'a [u8], sess: &'a [u8]) -> SessionKey<'a> {
        SessionKey {
            auth_header: auth,
            session_header: sess,
        }
    }

    #[test]
    fn session_id_is_deterministic() {
        assert_eq!(key(b"Bearer x", b"s1").id(), key(b"Bearer x", b"s1").id());
    }

    #[test]
    fn different_auth_diverges() {
        assert_ne!(key(b"Bearer x", b"s1").id(), key(b"Bearer y", b"s1").id());
    }

    #[test]
    fn different_session_header_diverges() {
        assert_ne!(key(b"Bearer x", b"s1").id(), key(b"Bearer x", b"s2").id());
    }

    #[test]
    fn length_prefixing_prevents_concat_collision() {
        // Without length prefixing, ("ab", "") and ("a", "b") would hash
        // to the same id. Confirm they do not.
        assert_ne!(key(b"ab", b"").id(), key(b"a", b"b").id());
    }

    #[test]
    fn prefix_is_eight_lowercase_hex_chars() {
        let id = key(b"x", b"y").id();
        let p = id.prefix();
        assert_eq!(p.len(), 8);
        assert!(
            p.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn session_starts_with_directive_not_enhanced() {
        let id = key(b"x", b"y").id();
        let s = Session::new(id);
        assert!(!s.directive_enhanced.load(Ordering::Relaxed));
    }

    #[test]
    fn session_directive_flag_is_settable() {
        let s = Session::new(key(b"x", b"y").id());
        s.directive_enhanced.store(true, Ordering::Relaxed);
        assert!(s.directive_enhanced.load(Ordering::Relaxed));
    }
}
