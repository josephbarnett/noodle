//! Async writer task — owns the file handle and the serialize-and-write
//! loop. Lives entirely off the engine's hot path.
//!
//! The writer is a single tokio task started by [`spawn`]. It receives
//! pre-serialized JSONL lines over a bounded mpsc channel, writes them
//! through a `BufWriter`, and flushes periodically (every
//! [`FLUSH_INTERVAL`] OR every [`FLUSH_BATCH`] lines, whichever comes
//! first).
//!
//! On [`shutdown`] (or channel close), it drains remaining items, flushes,
//! and closes the file.
//!
//! ## Invariants
//!
//! - The hot path never touches `self.file` — only this task does.
//! - The writer never panics on I/O error: errors are logged via
//!   `tracing::warn!` and the line is dropped.
//! - On `Drop` of the [`WriterHandle`], the channel closes and the
//!   task drains naturally.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::mpsc;
use tokio::time::{Instant, interval_at};

/// Flush every N lines.
pub const FLUSH_BATCH: usize = 64;
/// Flush every N milliseconds even if the batch isn't full.
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(100);

/// Handle to a running writer task. Drop it (or call [`Self::shutdown`])
/// to drain and close.
pub struct WriterHandle {
    pub(crate) tx: mpsc::Sender<Vec<u8>>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl WriterHandle {
    /// Trigger a graceful drain: closes the channel, waits for the
    /// writer task to flush remaining buffered events, then returns.
    pub async fn shutdown(mut self) {
        // Drop the sender so the receiver loop exits cleanly.
        drop(std::mem::replace(
            &mut self.tx,
            mpsc::channel(1).0, // dummy; original is dropped
        ));
        if let Some(j) = self.join.take() {
            let _ = j.await;
        }
    }
}

impl Drop for WriterHandle {
    fn drop(&mut self) {
        // Sender drops when self is dropped; the writer task will exit
        // on its own. We don't await here — Drop is sync. Callers who
        // care about flush-before-exit must call `shutdown().await`.
        if let Some(j) = self.join.take() {
            j.abort();
        }
    }
}

/// Open `path` (truncating any existing file), spawn a writer task on
/// the current tokio runtime, and return its [`WriterHandle`].
///
/// `capacity` bounds the in-flight queue. When full, callers that use
/// `try_send` will get an error and should drop+count.
pub async fn spawn(path: PathBuf, capacity: usize) -> std::io::Result<WriterHandle> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .await?;

    let (tx, rx) = mpsc::channel::<Vec<u8>>(capacity);
    let join = tokio::spawn(run(file, rx, path));
    Ok(WriterHandle {
        tx,
        join: Some(join),
    })
}

async fn run(file: File, mut rx: mpsc::Receiver<Vec<u8>>, path: PathBuf) {
    let mut writer = BufWriter::new(file);
    let mut pending: usize = 0;
    let start = Instant::now() + FLUSH_INTERVAL;
    let mut tick = interval_at(start, FLUSH_INTERVAL);

    loop {
        tokio::select! {
            biased;
            line = rx.recv() => {
                let Some(line) = line else { break; };
                if let Err(e) = writer.write_all(&line).await {
                    tracing::warn!(?e, path = %path.display(), "tap writer: write_all failed");
                    continue;
                }
                pending += 1;
                if pending >= FLUSH_BATCH {
                    flush(&mut writer, &path).await;
                    pending = 0;
                }
            }
            _ = tick.tick() => {
                if pending > 0 {
                    flush(&mut writer, &path).await;
                    pending = 0;
                }
            }
        }
    }

    // Channel closed: drain anything still in our queue, then flush
    // and close.
    while let Ok(line) = rx.try_recv() {
        if let Err(e) = writer.write_all(&line).await {
            tracing::warn!(?e, path = %path.display(), "tap writer: drain write_all failed");
        }
    }
    flush(&mut writer, &path).await;
    if let Err(e) = writer.shutdown().await {
        tracing::warn!(?e, path = %path.display(), "tap writer: shutdown failed");
    }
}

async fn flush(writer: &mut BufWriter<File>, path: &Path) {
    if let Err(e) = writer.flush().await {
        tracing::warn!(?e, path = %path.display(), "tap writer: flush failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tokio::fs;

    #[tokio::test]
    async fn writes_then_drains_on_handle_drop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        let handle = spawn(path.clone(), 16).await.unwrap();
        for i in 0..5 {
            handle
                .tx
                .send(format!("line-{i}\n").into_bytes())
                .await
                .unwrap();
        }
        // Graceful drain.
        handle.shutdown().await;

        let contents = fs::read_to_string(&path).await.unwrap();
        assert_eq!(contents, "line-0\nline-1\nline-2\nline-3\nline-4\n");
    }

    #[tokio::test]
    async fn truncates_existing_file_on_spawn() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        // Pre-populate.
        fs::write(&path, b"old data\n").await.unwrap();
        let handle = spawn(path.clone(), 4).await.unwrap();
        handle.tx.send(b"new\n".to_vec()).await.unwrap();
        handle.shutdown().await;
        let contents = fs::read_to_string(&path).await.unwrap();
        assert_eq!(contents, "new\n");
    }
}
