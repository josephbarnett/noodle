//! `ContextEnhancer` port — request mutation + response artifact extraction.
//!
//! Owns one round-trip lifecycle. On the request side, mutates the
//! body to include a directive (e.g. attribution prompt fragment).
//! On the response side, extracts named artifacts the model emitted
//! (e.g. `<noodle:work_type>...</noodle:work_type>` tag values).
//!
//! Pattern: Strategy. One impl per attribution scheme.

use bytes::Bytes;

use crate::{BoxError, FieldWriter, Session};

pub struct EnhanceContext<'a> {
    pub provider: &'a str,
    pub path: &'a str,
    pub session: &'a Session,
}

pub struct DiscoverContext<'a> {
    pub provider: &'a str,
    pub session: &'a Session,
}

pub trait ContextEnhancer: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    /// Mutate the outbound body. MUST be idempotent on follow-up
    /// requests within the same session — concrete impls typically
    /// gate on `Session::directive_enhanced`.
    fn enhance(&self, ctx: &EnhanceContext<'_>, body: Bytes) -> Result<Bytes, BoxError>;

    /// Extract named artifacts from the (assembled) response text and
    /// write them as fields. Called once per turn after the response
    /// is fully decoded.
    fn discover(
        &self,
        ctx: &DiscoverContext<'_>,
        text: &str,
        fields: &mut dyn FieldWriter,
    ) -> Result<(), BoxError>;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn enhancer_is_object_safe() {
        let _v: Vec<Arc<dyn ContextEnhancer>> = Vec::new();
    }
}
