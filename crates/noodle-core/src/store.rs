//! `SessionStore` port. Driven adapters: in-memory (default), Redis (later).

use std::sync::Arc;

use crate::{Session, SessionId};

pub trait SessionStore: Send + Sync + 'static {
    /// Get the session for `id`, creating it lazily if absent.
    /// Implementations must serialize concurrent first-create attempts.
    fn get_or_init(&self, id: &SessionId) -> Arc<Session>;
}
