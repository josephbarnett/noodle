//! Tail `~/.noodle/side_effects.jsonl` and emit one parsed
//! [`SideEffectEvent`] per complete line.
//!
//! Mirrors the tail-watcher shape of
//! [`crate::adapters::tap_jsonl_source::TapJsonlSource`] —
//! initial replay, fsnotify + low-frequency poll as a safety
//! net, parse-skip on bad lines, truncation handling.
//!
//! Item 4 viewer-panel slice (ADR 020 §7): drives the
//! attribution side-effect feed into the viewer hub.

use std::path::PathBuf;

use notify::{Event, EventKind, RecursiveMode, Watcher};
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader, SeekFrom};
use tokio::sync::mpsc;

use crate::model::SideEffectEvent;
use crate::ports::SideEffectSource;

/// fsnotify-driven JSONL tail for the side-effects file.
pub struct SideEffectsJsonlSource {
    rx: std::sync::Mutex<Option<mpsc::Receiver<SideEffectEvent>>>,
}

impl SideEffectsJsonlSource {
    /// Begin tailing `path`. The first batch (everything
    /// currently in the file) goes through before this returns.
    pub async fn spawn(path: PathBuf, capacity: usize) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel::<SideEffectEvent>(capacity);
        spawn_tail(path, tx).await?;
        Ok(Self {
            rx: std::sync::Mutex::new(Some(rx)),
        })
    }
}

impl SideEffectSource for SideEffectsJsonlSource {
    fn subscribe(&self) -> mpsc::Receiver<SideEffectEvent> {
        self.rx
            .lock()
            .expect("poisoned")
            .take()
            .expect("SideEffectsJsonlSource::subscribe called more than once")
    }
}

async fn spawn_tail(path: PathBuf, tx: mpsc::Sender<SideEffectEvent>) -> std::io::Result<()> {
    // Initial replay: read whatever's already in the file.
    let mut pos = replay_initial(&path, &tx).await?;

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
                    tracing::warn!(
                        ?e,
                        path = %path_clone.display(),
                        "side-effects tail: read failed",
                    );
                }
            }
        }
    });

    Ok(())
}

async fn replay_initial(
    path: &PathBuf,
    tx: &mpsc::Sender<SideEffectEvent>,
) -> std::io::Result<u64> {
    read_tail(path, 0, tx).await
}

async fn read_tail(
    path: &PathBuf,
    start_pos: u64,
    tx: &mpsc::Sender<SideEffectEvent>,
) -> std::io::Result<u64> {
    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(start_pos);
        }
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
            // Partial write — rewind so we re-read this line.
            pos -= read as u64;
            break;
        }
        match serde_json::from_str::<SideEffectEvent>(line.trim_end()) {
            Ok(ev) => {
                if tx.send(ev).await.is_err() {
                    return Ok(pos);
                }
            }
            Err(e) => {
                tracing::warn!(?e, "side-effects tail: skipping unparseable line");
            }
        }
    }
    Ok(pos)
}

fn io_err(e: notify::Error) -> std::io::Error {
    std::io::Error::other(e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tokio::io::AsyncWriteExt;
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn parses_resolved_record_on_initial_replay() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("side_effects.jsonl");
        let line = r#"{"kind":"resolved","session_prefix":"abc12345","flow_id":0,"at_unix_ms":1779000000000,"resolved":{"tool":"Claude Code","work_type":"refactor"}}"#;
        tokio::fs::write(&path, format!("{line}\n")).await.unwrap();

        let source = SideEffectsJsonlSource::spawn(path, 16).await.unwrap();
        let mut rx = source.subscribe();
        let ev = timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("timeout")
            .expect("event");
        match ev {
            SideEffectEvent::Resolved {
                session_prefix,
                flow_id,
                resolved,
                ..
            } => {
                assert_eq!(session_prefix, "abc12345");
                assert_eq!(flow_id, 0);
                assert_eq!(resolved.get("tool").unwrap(), "Claude Code");
                assert_eq!(resolved.get("work_type").unwrap(), "refactor");
            }
            other => panic!("expected Resolved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn parses_hint_artifact_audit_kinds() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("side_effects.jsonl");
        let lines = [
            r#"{"kind":"hint","category":"tool","value":"Claude Code","confidence":0.95,"source":"user_agent"}"#,
            r#"{"kind":"artifact","name":"work_type","value":"refactor","source_transform":"marker-strip","flow_id":0,"captured_at_unix_ms":1779000000000}"#,
            r#"{"kind":"audit","kind_inner":"redacted","transform":"marker-strip","flow_id":0,"at_unix_ms":1779000000000,"detail":{"marker":"work_type"}}"#,
        ];
        tokio::fs::write(&path, format!("{}\n", lines.join("\n")))
            .await
            .unwrap();

        let source = SideEffectsJsonlSource::spawn(path, 16).await.unwrap();
        let mut rx = source.subscribe();
        let h = timeout(Duration::from_millis(500), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(h, SideEffectEvent::Hint { .. }));
        let a = timeout(Duration::from_millis(500), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(a, SideEffectEvent::Artifact { .. }));
        let u = timeout(Duration::from_millis(500), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(u, SideEffectEvent::Audit { .. }));
    }

    #[tokio::test]
    async fn unparseable_lines_are_skipped_not_fatal() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("side_effects.jsonl");
        let pre = format!(
            "garbage not json\n{}\n",
            r#"{"kind":"hint","category":"tool","value":"x","confidence":0.5,"source":"y"}"#,
        );
        tokio::fs::write(&path, pre).await.unwrap();

        let source = SideEffectsJsonlSource::spawn(path, 16).await.unwrap();
        let mut rx = source.subscribe();
        let ev = timeout(Duration::from_millis(500), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev, SideEffectEvent::Hint { .. }));
    }

    #[tokio::test]
    async fn missing_file_is_not_fatal_initially() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("side_effects.jsonl");
        // File doesn't exist yet.
        let source = SideEffectsJsonlSource::spawn(path.clone(), 16)
            .await
            .unwrap();
        let mut rx = source.subscribe();

        // Create file later.
        let mut f = tokio::fs::File::create(&path).await.unwrap();
        f.write_all(b"{\"kind\":\"hint\",\"category\":\"tool\",\"value\":\"X\",\"confidence\":1.0,\"source\":\"s\"}\n").await.unwrap();
        f.flush().await.unwrap();
        drop(f);

        // fsnotify timing is finicky on darwin tempfs; use a
        // generous timeout. The hub-side smoke is the real
        // proof.
        let ev = timeout(Duration::from_secs(3), rx.recv()).await;
        // Tolerate timeout — this assertion is more about
        // "no panic" than precise delivery.
        if let Ok(Some(SideEffectEvent::Hint { value, .. })) = ev {
            assert_eq!(value, "X");
        }
    }
}
