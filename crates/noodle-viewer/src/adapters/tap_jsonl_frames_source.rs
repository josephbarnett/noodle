//! Tail `~/.noodle/tap.jsonl` and synthesize per-SSE-frame [`Frame`]s
//! from each response record's `events[]` array (ADR 027 §1 / refactor
//! overview §10).
//!
//! Replaces the legacy `frames.jsonl` sidecar source. The accumulator
//! that built `frames.jsonl` writes the same SSE event payloads into
//! `TapEntry.events[]` (with `ts_offset_ms` measured from the
//! response's first-byte instant), so we can derive the viewer's
//! per-frame stream from `tap.jsonl` alone — one file, one boundary.
//!
//! Mirrors [`crate::adapters::tap_jsonl_source::TapJsonlSource`] in
//! every operational respect — initial replay, fsnotify-with-poll
//! safety net, truncation reset, skip-on-parse-error — and only
//! differs in that one tap line can fan out into many `Frame`s.

use std::path::PathBuf;

use notify::{Event, EventKind, RecursiveMode, Watcher};
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader, SeekFrom};
use tokio::sync::mpsc;

use crate::model::Frame;
use crate::ports::FrameSource;

/// fsnotify-driven JSONL tail over `tap.jsonl`. For every response
/// record with a non-empty `events[]`, emits one [`Frame`] per event
/// preserving the SSE arrival order.
pub struct TapJsonlFramesSource {
    rx: std::sync::Mutex<Option<mpsc::Receiver<Frame>>>,
}

impl TapJsonlFramesSource {
    /// Begin tailing `path` (typically `~/.noodle/tap.jsonl`). The
    /// initial replay (everything already in the file) completes
    /// before this returns.
    pub async fn spawn(path: PathBuf, capacity: usize) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel::<Frame>(capacity);
        spawn_tail(path, tx).await?;
        Ok(Self {
            rx: std::sync::Mutex::new(Some(rx)),
        })
    }
}

impl FrameSource for TapJsonlFramesSource {
    fn subscribe(&self) -> mpsc::Receiver<Frame> {
        self.rx
            .lock()
            .expect("poisoned")
            .take()
            .expect("TapJsonlFramesSource::subscribe called more than once")
    }
}

async fn spawn_tail(path: PathBuf, tx: mpsc::Sender<Frame>) -> std::io::Result<()> {
    let mut pos = read_tail(&path, 0, &tx).await?;

    let (notify_tx, mut notify_rx) = mpsc::channel::<()>(64);
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(event) = res
            && matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_))
        {
            let _ = notify_tx.try_send(());
        }
    })
    .map_err(io_err)?;
    let watch_target = path.parent().unwrap_or(&path).to_path_buf();
    watcher
        .watch(&watch_target, RecursiveMode::NonRecursive)
        .map_err(io_err)?;

    let path_clone = path.clone();
    tokio::spawn(async move {
        let _watcher = watcher;
        let mut poll_tick = tokio::time::interval(std::time::Duration::from_millis(500));
        poll_tick.tick().await;
        loop {
            tokio::select! {
                _ = notify_rx.recv() => {}
                _ = poll_tick.tick() => {}
            }
            match read_tail(&path_clone, pos, &tx).await {
                Ok(new_pos) => pos = new_pos,
                Err(e) => {
                    tracing::warn!(?e, path = %path_clone.display(), "tap-frames tail: read failed");
                }
            }
        }
    });

    Ok(())
}

async fn read_tail(
    path: &PathBuf,
    start_pos: u64,
    tx: &mpsc::Sender<Frame>,
) -> std::io::Result<u64> {
    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(start_pos),
        Err(e) => return Err(e),
    };

    let len = file.metadata().await?.len();
    let seek_to = if len < start_pos { 0 } else { start_pos };
    file.seek(SeekFrom::Start(seek_to)).await?;

    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut pos = seek_to;
    loop {
        line.clear();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            break;
        }
        pos += read as u64;
        if !line.ends_with('\n') {
            pos -= read as u64;
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        for frame in synth_frames_from_line(trimmed) {
            if tx.send(frame).await.is_err() {
                return Ok(pos);
            }
        }
    }
    Ok(pos)
}

/// Parse one `tap.jsonl` line and return the frame stream it implies.
///
/// Yields nothing for request records, response records that lack
/// `events[]`, or lines that don't parse — those are debug noise the
/// frames view doesn't need. Returns a `Vec` rather than streaming
/// directly because all events for one response live in a single
/// line, so there's no benefit to async streaming inside the parse.
fn synth_frames_from_line(line: &str) -> Vec<Frame> {
    let Ok(rec) = serde_json::from_str::<serde_json::Value>(line) else {
        tracing::warn!("tap-frames tail: skipping unparseable line");
        return Vec::new();
    };
    if rec.get("direction").and_then(|v| v.as_str()) != Some("response") {
        return Vec::new();
    }
    let Some(events) = rec.get("events").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let request_id = rec
        .get("event_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let base_ms = rec
        .get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(rfc3339_to_unix_ms)
        .unwrap_or(0);
    let mut out = Vec::with_capacity(events.len());
    for (i, ev) in events.iter().enumerate() {
        let ts_offset_ms = ev
            .get("ts_offset_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let ts_unix_ms = base_ms.saturating_add(ts_offset_ms);
        let event = ev.get("type").and_then(|v| v.as_str()).map(str::to_owned);
        out.push(Frame {
            request_id: request_id.clone(),
            frame_index: u32::try_from(i).unwrap_or(u32::MAX),
            timestamp: unix_ms_to_rfc3339(ts_unix_ms),
            ts_unix_ms,
            event,
            data: ev.clone(),
        });
    }
    out
}

/// Parse an RFC 3339 / ISO-8601 UTC string into unix epoch
/// milliseconds. Returns `None` when the string isn't parseable —
/// upstream falls back to `0` and the frame still ships with a usable
/// `ts_offset_ms`. Implemented locally because `chrono` is not in
/// this crate's dependency set and one timestamp parse doesn't earn
/// it; the input format is always `format_rfc3339_nano`'s output.
fn rfc3339_to_unix_ms(s: &str) -> Option<u64> {
    // Format from `noodle_tap::timestamp::format_rfc3339_nano`:
    //   YYYY-MM-DDTHH:MM:SS.fffffffffZ
    // We accept any fractional precision (0..=9 digits) and require
    // a trailing `Z`. Anything else returns None.
    let bytes = s.as_bytes();
    if bytes.len() < 20 || bytes[bytes.len() - 1] != b'Z' {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let hour: u32 = s.get(11..13)?.parse().ok()?;
    let min: u32 = s.get(14..16)?.parse().ok()?;
    let sec: u32 = s.get(17..19)?.parse().ok()?;
    let frac_ms: u64 = if bytes.len() > 20 && bytes[19] == b'.' {
        let frac = &s[20..s.len() - 1];
        // Take up to 3 leading digits as milliseconds; pad with 0s.
        let mut ms = 0u64;
        for i in 0..3 {
            ms = ms * 10
                + u64::from(
                    frac.as_bytes()
                        .get(i)
                        .copied()
                        .filter(u8::is_ascii_digit)
                        .map_or(b'0', |b| b)
                        - b'0',
                );
        }
        ms
    } else {
        0
    };
    let days = days_from_civil(year, month, day)?;
    let secs = days
        .checked_mul(86_400)?
        .checked_add(i64::from(hour) * 3_600 + i64::from(min) * 60 + i64::from(sec))?;
    let ms = u64::try_from(secs)
        .ok()?
        .checked_mul(1_000)?
        .checked_add(frac_ms)?;
    Some(ms)
}

/// Format unix epoch milliseconds back to the same RFC 3339 shape.
fn unix_ms_to_rfc3339(ms: u64) -> String {
    let secs = ms / 1_000;
    let sub_ms = ms % 1_000;
    let (y, mo, d, h, mi, se) = civil_from_secs(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{se:02}.{sub_ms:03}Z")
}

/// Howard Hinnant's date algorithm: civil date → days since 1970-01-01.
/// Returns None on out-of-range month/day.
#[allow(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_lossless
)]
fn days_from_civil(y: i64, m: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m_adj = u64::from(if m > 2 { m - 3 } else { m + 9 });
    let doy = (153 * m_adj + 2) / 5 + u64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe as i64 - 719_468)
}

#[allow(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::many_single_char_names
)]
fn civil_from_secs(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let sod = (secs % 86_400) as u32;
    let hour = sod / 3_600;
    let minute = (sod / 60) % 60;
    let second = sod % 60;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day, hour, minute, second)
}

fn io_err(e: notify::Error) -> std::io::Error {
    std::io::Error::other(e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tokio::time::Duration;

    fn tap_response_line(event_id: &str, events: &serde_json::Value) -> String {
        serde_json::to_string(&serde_json::json!({
            "direction": "response",
            "timestamp": "2026-05-11T12:00:00.000Z",
            "event_id": event_id,
            "status": 200,
            "events": events,
        }))
        .unwrap()
    }

    fn tap_request_line(event_id: &str) -> String {
        serde_json::to_string(&serde_json::json!({
            "direction": "request",
            "timestamp": "2026-05-11T12:00:00.000Z",
            "event_id": event_id,
            "url": "https://api.anthropic.com/v1/messages",
            "method": "POST",
        }))
        .unwrap()
    }

    #[test]
    fn rfc3339_round_trip_preserves_millis() {
        let ms = rfc3339_to_unix_ms("2026-05-11T12:00:00.123Z").unwrap();
        assert_eq!(unix_ms_to_rfc3339(ms), "2026-05-11T12:00:00.123Z");
    }

    #[tokio::test]
    async fn fans_events_out_into_frames_per_response() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        let line = tap_response_line(
            "nl-1",
            &serde_json::json!([
                {"type": "message_start", "ts_offset_ms": 0,   "message": {"id": "m1"}},
                {"type": "ping",          "ts_offset_ms": 150},
                {"type": "message_stop",  "ts_offset_ms": 999}
            ]),
        );
        tokio::fs::write(&path, format!("{line}\n")).await.unwrap();

        let source = TapJsonlFramesSource::spawn(path, 16).await.unwrap();
        let mut rx = source.subscribe();

        for (i, expected_event) in ["message_start", "ping", "message_stop"]
            .into_iter()
            .enumerate()
        {
            let f = tokio::time::timeout(Duration::from_millis(500), rx.recv())
                .await
                .expect("timeout")
                .expect("frame");
            assert_eq!(f.request_id, "nl-1");
            assert_eq!(f.frame_index, u32::try_from(i).unwrap());
            assert_eq!(f.event.as_deref(), Some(expected_event));
        }
    }

    #[tokio::test]
    async fn request_records_emit_no_frames() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        let line = tap_request_line("nl-2");
        tokio::fs::write(&path, format!("{line}\n")).await.unwrap();

        let source = TapJsonlFramesSource::spawn(path, 4).await.unwrap();
        let mut rx = source.subscribe();
        let r = tokio::time::timeout(Duration::from_millis(150), rx.recv()).await;
        assert!(
            r.is_err(),
            "no frames should be emitted for request records"
        );
    }

    #[tokio::test]
    async fn response_without_events_emits_no_frames() {
        // Non-SSE responses (or anthropic v1 non-streaming) have no
        // `events[]`. The frames stream should be silent for them.
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        let line = serde_json::to_string(&serde_json::json!({
            "direction": "response",
            "timestamp": "2026-05-11T12:00:00.000Z",
            "event_id": "nl-3",
            "status": 200,
        }))
        .unwrap();
        tokio::fs::write(&path, format!("{line}\n")).await.unwrap();

        let source = TapJsonlFramesSource::spawn(path, 4).await.unwrap();
        let mut rx = source.subscribe();
        let r = tokio::time::timeout(Duration::from_millis(150), rx.recv()).await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn unparseable_lines_are_skipped_not_fatal() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        let line = tap_response_line(
            "nl-4",
            &serde_json::json!([{"type": "ping", "ts_offset_ms": 1}]),
        );
        let pre = format!("not a tap line\n{line}\n");
        tokio::fs::write(&path, pre).await.unwrap();

        let source = TapJsonlFramesSource::spawn(path, 4).await.unwrap();
        let mut rx = source.subscribe();
        let f = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("timeout")
            .expect("frame");
        assert_eq!(f.request_id, "nl-4");
        assert_eq!(f.event.as_deref(), Some("ping"));
    }

    #[tokio::test]
    async fn missing_tap_file_is_non_fatal() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        let source = TapJsonlFramesSource::spawn(path, 4).await.unwrap();
        let _rx = source.subscribe();
    }
}
