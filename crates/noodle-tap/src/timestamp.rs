//! `Unix-ms` → `RFC3339Nano` `UTC` formatting.
//!
//! TAP timestamps are `RFC3339Nano` in `UTC`, matching the Go side's
//! `time.Now().UTC().Format(time.RFC3339Nano)`. Example:
//! `"2026-05-10T17:08:59.123456789Z"`.

use time::{OffsetDateTime, format_description::well_known::Rfc3339};

/// Format a `Unix`-millisecond timestamp as `RFC3339Nano` `UTC`.
///
/// Returns a fallback `"1970-01-01T00:00:00Z"` if the input overflows
/// `OffsetDateTime`'s range — this is logged via `tracing::warn!` in
/// the caller, but we never fail the `WireEvent` record path.
#[must_use]
pub fn format_rfc3339_nano(ts_unix_ms: u64) -> String {
    let nanos = i128::from(ts_unix_ms) * 1_000_000;
    OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .ok()
        .and_then(|d| d.format(&Rfc3339).ok())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_zero() {
        assert_eq!(format_rfc3339_nano(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn known_timestamp_round_trips() {
        // 2026-05-10T17:08:59.123Z, derived from `time::macros::datetime`
        // so the test isn't coupled to my mental arithmetic.
        let dt = time::macros::datetime!(2026-05-10 17:08:59.123 UTC);
        let ms_i128 = dt.unix_timestamp_nanos() / 1_000_000;
        let ms = u64::try_from(ms_i128).expect("positive");
        let s = format_rfc3339_nano(ms);
        assert_eq!(s, "2026-05-10T17:08:59.123Z");
    }

    #[test]
    fn no_fractional_when_ms_aligns_to_second() {
        let dt = time::macros::datetime!(2026-05-10 17:08:59 UTC);
        let ms_i128 = dt.unix_timestamp_nanos() / 1_000_000;
        let ms = u64::try_from(ms_i128).expect("positive");
        let s = format_rfc3339_nano(ms);
        assert_eq!(s, "2026-05-10T17:08:59Z");
    }

    #[test]
    fn never_panics_on_max_u64() {
        // Overflows; we just need a string back, not a correct one.
        let s = format_rfc3339_nano(u64::MAX);
        assert!(!s.is_empty());
    }
}
