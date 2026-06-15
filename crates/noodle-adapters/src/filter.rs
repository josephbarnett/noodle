//! Filter driven adapters.
//!
//! - `PassThroughFilter` — emits input unchanged. Useful as a placeholder.
//! - `MarkerStripFilter` — wraps `MarkerScanner` to strip + capture
//!   `<noodle:NAME>VALUE</noodle:NAME>` markers from a streaming text
//!   sequence. The first real `Filter` impl in noodle.

use noodle_core::{Filter, FilterContext, FilterFactory, FilterOutput, MarkerScanner, ScanOutput};

// ── PassThrough ─────────────────────────────────────────────────────

/// Filter that emits its input unchanged. Useful as a default or
/// in tests that want to bypass marker handling.
pub struct PassThroughFilter;

impl Filter for PassThroughFilter {
    fn process(&mut self, chunk: &str) -> FilterOutput {
        FilterOutput::passthrough(chunk)
    }

    fn flush(&mut self) -> FilterOutput {
        FilterOutput::empty()
    }
}

/// Factory for `PassThroughFilter`.
pub struct PassThroughFilterFactory;

impl PassThroughFilterFactory {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Default for PassThroughFilterFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl FilterFactory for PassThroughFilterFactory {
    fn name(&self) -> &'static str {
        "passthrough"
    }

    fn make(&self, _ctx: &FilterContext<'_>) -> Box<dyn Filter> {
        Box::new(PassThroughFilter)
    }
}

// ── MarkerStrip ─────────────────────────────────────────────────────

/// Filter that recognizes and removes `<noodle:NAME>VALUE</noodle:NAME>`
/// markers from a streaming text sequence, capturing the values it
/// removes and surfacing them on each `process`/`flush` call.
///
/// State persists across calls so a marker split across SSE event
/// boundaries (the common case) resolves correctly. UTF-8 in the
/// surrounding text is preserved byte-for-byte; the marker itself is
/// ASCII so it never breaks a code-point boundary.
pub struct MarkerStripFilter {
    scanner: MarkerScanner,
}

impl MarkerStripFilter {
    #[must_use]
    pub fn new<I, S>(tag_names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            scanner: MarkerScanner::new(tag_names),
        }
    }
}

impl Filter for MarkerStripFilter {
    fn process(&mut self, chunk: &str) -> FilterOutput {
        emit(self.scanner.process(chunk.as_bytes()))
    }

    fn flush(&mut self) -> FilterOutput {
        emit(self.scanner.flush())
    }
}

fn emit(out: ScanOutput) -> FilterOutput {
    FilterOutput {
        // Marker is ASCII; input was UTF-8; output is therefore valid
        // UTF-8. The lossy fallback is defense against caller bugs.
        bytes: String::from_utf8(out.bytes)
            .unwrap_or_else(|e| String::from_utf8_lossy(&e.into_bytes()).into_owned()),
        markers: out.markers,
    }
}

/// Factory for `MarkerStripFilter`. Each call to `make()` returns a
/// fresh, independent filter — state is never shared across requests.
pub struct MarkerStripFilterFactory {
    tag_names: Vec<String>,
}

impl MarkerStripFilterFactory {
    /// Build a factory for the given tag-name allow-list.
    #[must_use]
    pub fn new<I, S>(tag_names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            tag_names: tag_names
                .into_iter()
                .map(|n| n.as_ref().to_owned())
                .collect(),
        }
    }
}

impl FilterFactory for MarkerStripFilterFactory {
    fn name(&self) -> &'static str {
        "marker_strip"
    }

    fn make(&self, _ctx: &FilterContext<'_>) -> Box<dyn Filter> {
        Box::new(MarkerStripFilter::new(&self.tag_names))
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use noodle_core::{Session, SessionKey};

    use super::*;

    fn session() -> Session {
        Session::new(
            SessionKey {
                auth_header: b"a",
                session_header: b"b",
            }
            .id(),
        )
    }

    fn ctx(session: &Session) -> FilterContext<'_> {
        FilterContext {
            provider: "openai",
            session,
        }
    }

    // ── PassThrough ────────────────────────────────────────────────

    #[test]
    fn passthrough_returns_input() {
        let factory = PassThroughFilterFactory::new();
        let s = session();
        let mut filter = factory.make(&ctx(&s));
        let out = filter.process("hello world");
        assert_eq!(out.bytes, "hello world");
        assert!(out.markers.is_empty());
        let flush = filter.flush();
        assert!(flush.bytes.is_empty());
        assert!(flush.markers.is_empty());
    }

    #[test]
    fn passthrough_factory_name() {
        assert_eq!(PassThroughFilterFactory::new().name(), "passthrough");
    }

    // ── MarkerStrip ────────────────────────────────────────────────

    fn drain<F: Filter + ?Sized>(filter: &mut F, chunks: &[&str]) -> FilterOutput {
        let mut acc = FilterOutput::default();
        for c in chunks {
            let o = filter.process(c);
            acc.bytes.push_str(&o.bytes);
            acc.markers.extend(o.markers);
        }
        let tail = filter.flush();
        acc.bytes.push_str(&tail.bytes);
        acc.markers.extend(tail.markers);
        acc
    }

    #[test]
    fn marker_strip_pass_through_when_no_marker() {
        let factory = MarkerStripFilterFactory::new(["work_type"]);
        let s = session();
        let mut filter = factory.make(&ctx(&s));
        let out = drain(filter.as_mut(), &["plain text, no markers."]);
        assert_eq!(out.bytes, "plain text, no markers.");
        assert!(out.markers.is_empty());
    }

    #[test]
    fn marker_strip_removes_and_captures_single_marker() {
        let factory = MarkerStripFilterFactory::new(["work_type"]);
        let s = session();
        let mut filter = factory.make(&ctx(&s));
        let out = drain(
            filter.as_mut(),
            &["before<noodle:work_type>build</noodle:work_type>after"],
        );
        assert_eq!(out.bytes, "beforeafter");
        assert_eq!(out.markers.len(), 1);
        assert_eq!(out.markers[0].name, "work_type");
        assert_eq!(out.markers[0].value, b"build");
    }

    #[test]
    fn marker_strip_handles_marker_split_across_chunks() {
        let factory = MarkerStripFilterFactory::new(["work_type"]);
        let s = session();
        let mut filter = factory.make(&ctx(&s));
        let out = drain(
            filter.as_mut(),
            &[
                "tokens <noodle:work_",
                "type>research</noodle:wo",
                "rk_type> done",
            ],
        );
        assert_eq!(out.bytes, "tokens  done");
        assert_eq!(out.markers.len(), 1);
        assert_eq!(out.markers[0].value, b"research");
    }

    #[test]
    fn marker_strip_unknown_tag_passes_through() {
        let factory = MarkerStripFilterFactory::new(["work_type"]);
        let s = session();
        let mut filter = factory.make(&ctx(&s));
        let out = drain(filter.as_mut(), &["see <noodle:foo>x</noodle:foo>"]);
        assert_eq!(out.bytes, "see <noodle:foo>x</noodle:foo>");
        assert!(out.markers.is_empty());
    }

    #[test]
    fn marker_strip_partial_marker_at_eof_flushes_verbatim() {
        let factory = MarkerStripFilterFactory::new(["work_type"]);
        let s = session();
        let mut filter = factory.make(&ctx(&s));
        let out = drain(filter.as_mut(), &["leading <noodle:work_ty"]);
        assert_eq!(out.bytes, "leading <noodle:work_ty");
        assert!(out.markers.is_empty());
    }

    #[test]
    fn factory_makes_independent_filters() {
        let factory = MarkerStripFilterFactory::new(["work_type"]);
        let s = session();
        // Filter A captures.
        let mut a = factory.make(&ctx(&s));
        let oa = drain(a.as_mut(), &["<noodle:work_type>A</noodle:work_type>"]);
        assert_eq!(oa.markers.len(), 1);
        // Filter B is a fresh instance — does not see A's state.
        let mut b = factory.make(&ctx(&s));
        let ob = drain(b.as_mut(), &["just text"]);
        assert_eq!(ob.bytes, "just text");
        assert!(ob.markers.is_empty());
    }

    #[test]
    fn marker_strip_factory_name() {
        let f = MarkerStripFilterFactory::new(["x"]);
        assert_eq!(f.name(), "marker_strip");
    }

    #[test]
    fn marker_strip_two_markers_in_one_chunk() {
        let factory = MarkerStripFilterFactory::new(["work_type", "project"]);
        let s = session();
        let mut filter = factory.make(&ctx(&s));
        let out = drain(
            filter.as_mut(),
            &[
                "<noodle:work_type>build</noodle:work_type><noodle:project>noodle</noodle:project>tail",
            ],
        );
        assert_eq!(out.bytes, "tail");
        assert_eq!(out.markers.len(), 2);
        assert_eq!(out.markers[0].name, "work_type");
        assert_eq!(out.markers[0].value, b"build");
        assert_eq!(out.markers[1].name, "project");
        assert_eq!(out.markers[1].value, b"noodle");
    }
}
