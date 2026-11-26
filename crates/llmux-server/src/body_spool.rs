//! Streaming ingress capture for request bodies.
//!
//! Small bodies stay in memory. Once buffered bytes would cross
//! `request_body_memory_bytes`, capture rolls over to a bounded blocking writer
//! so network delivery does not run ahead of disk admission.

use axum::body::{Body, Bytes};
use futures_util::StreamExt;
use llmux_types::{SpoolError, SpoolStore, SpooledBody};
use std::sync::Arc;
use tokio::{sync::mpsc, task::JoinHandle};

#[derive(Debug)]
pub(crate) enum CapturedBody {
    Memory(Bytes),
    Spooled(Arc<SpooledBody>),
}

impl CapturedBody {
    pub(crate) async fn bytes(&self) -> Result<Bytes, CaptureError> {
        match self {
            Self::Memory(body) => Ok(body.clone()),
            Self::Spooled(spool) => read_spool_bytes(spool.clone()).await,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureError {
    TooLarge,
    Read,
    BudgetExhausted,
    Io,
}

impl From<SpoolError> for CaptureError {
    fn from(error: SpoolError) -> Self {
        match error {
            SpoolError::BudgetExhausted => Self::BudgetExhausted,
            SpoolError::Io | SpoolError::Cancelled => Self::Io,
        }
    }
}

pub(crate) async fn capture_body(
    body: Body,
    store: SpoolStore,
    memory_threshold: usize,
    max_bytes: usize,
) -> Result<CapturedBody, CaptureError> {
    let mut memory = Vec::new();
    let mut disk: Option<DiskWriter> = None;
    let mut total = 0usize;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => {
                if let Some(writer) = disk.take() {
                    writer.abort().await;
                }
                return Err(CaptureError::Read);
            }
        };
        if chunk.is_empty() {
            continue;
        }
        let Some(next_total) = total
            .checked_add(chunk.len())
            .filter(|total| *total <= max_bytes)
        else {
            if let Some(writer) = disk.take() {
                writer.abort().await;
            }
            return Err(CaptureError::TooLarge);
        };
        total = next_total;
        if let Some(writer) = &mut disk {
            if writer.send(chunk).await.is_err() {
                let writer = disk.take().expect("disk writer is present");
                return Err(writer.finish_error().await);
            }
        } else if memory.len() + chunk.len() <= memory_threshold {
            memory.extend_from_slice(&chunk);
        } else {
            let mut writer = DiskWriter::start(store.clone());
            let buffered = Bytes::copy_from_slice(&memory);
            memory.clear();
            if !buffered.is_empty() && writer.send(buffered).await.is_err() {
                return Err(writer.finish_error().await);
            }
            if writer.send(chunk).await.is_err() {
                return Err(writer.finish_error().await);
            }
            disk = Some(writer);
        }
    }
    if let Some(writer) = disk {
        writer
            .finish()
            .await
            .map(|spool| CapturedBody::Spooled(Arc::new(spool)))
    } else {
        Ok(CapturedBody::Memory(Bytes::from(memory)))
    }
}

pub(crate) async fn read_spool_bytes(spool: Arc<SpooledBody>) -> Result<Bytes, CaptureError> {
    tokio::task::spawn_blocking(move || spool.read_bytes())
        .await
        .map_err(|_| CaptureError::Io)?
        .map_err(|_| CaptureError::Io)
}

pub(crate) async fn copy_to_spool(
    store: SpoolStore,
    bytes: Bytes,
) -> Result<Arc<SpooledBody>, CaptureError> {
    tokio::task::spawn_blocking(move || store.spool_bytes(&bytes))
        .await
        .map_err(|_| CaptureError::Io)?
        .map_err(CaptureError::from)
}

enum WriteCommand {
    Chunk(Bytes),
    Finish,
}

struct DiskWriter {
    tx: mpsc::Sender<WriteCommand>,
    task: JoinHandle<Result<SpooledBody, SpoolError>>,
}

impl DiskWriter {
    fn start(store: SpoolStore) -> Self {
        let (tx, rx) = mpsc::channel(2);
        let task = tokio::task::spawn_blocking(move || write_spool(store, rx));
        Self { tx, task }
    }

    async fn send(&mut self, chunk: Bytes) -> Result<(), ()> {
        self.tx
            .send(WriteCommand::Chunk(chunk))
            .await
            .map_err(|_| ())
    }

    async fn finish(self) -> Result<SpooledBody, CaptureError> {
        let _ = self.tx.send(WriteCommand::Finish).await;
        drop(self.tx);
        match self.task.await {
            Ok(Ok(spool)) => Ok(spool),
            Ok(Err(error)) => Err(error.into()),
            Err(_) => Err(CaptureError::Io),
        }
    }

    async fn finish_error(self) -> CaptureError {
        match self.finish().await {
            Ok(_) => CaptureError::Io,
            Err(error) => error,
        }
    }

    async fn abort(self) {
        drop(self.tx);
        let _ = self.task.await;
    }
}

fn write_spool(
    store: SpoolStore,
    mut rx: mpsc::Receiver<WriteCommand>,
) -> Result<SpooledBody, SpoolError> {
    let mut file = store.begin()?;
    while let Some(command) = rx.blocking_recv() {
        match command {
            WriteCommand::Chunk(chunk) => file.write(&chunk)?,
            WriteCommand::Finish => return file.finish(),
        }
    }
    Err(SpoolError::Cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use std::{convert::Infallible, fs, io, time::Duration};

    #[tokio::test]
    async fn empty_chunks_do_not_count_toward_the_body_limit() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpoolStore::open(dir.path(), 4096).unwrap();
        let chunks = vec![
            Ok::<Bytes, Infallible>(Bytes::new()),
            Ok::<Bytes, Infallible>(Bytes::from_static(b"body")),
        ];
        let body = Body::from_stream(stream::iter(chunks));
        let captured = capture_body(body, store, 8, 4).await.unwrap();
        assert_eq!(captured.bytes().await.unwrap(), Bytes::from_static(b"body"));
    }

    #[tokio::test]
    async fn a_body_read_error_removes_the_partial_spool_before_returning() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpoolStore::open(dir.path(), 4096).unwrap();
        let chunks = vec![
            Ok::<Bytes, io::Error>(Bytes::from(vec![b'x'; 64])),
            Err(io::Error::other("synthetic read failure")),
        ];
        let body = Body::from_stream(stream::iter(chunks));
        let error = capture_body(body, store.clone(), 8, 4096)
            .await
            .unwrap_err();
        assert_eq!(error, CaptureError::Read);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
        assert_eq!(store.budget().used(), 0);
    }

    #[tokio::test]
    async fn copying_to_a_spool_preserves_budget_errors() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpoolStore::open(dir.path(), 4).unwrap();
        let error = copy_to_spool(store.clone(), Bytes::from_static(b"12345"))
            .await
            .unwrap_err();
        assert_eq!(error, CaptureError::BudgetExhausted);
        assert_eq!(store.budget().used(), 0);
    }

    #[tokio::test]
    async fn oversized_partial_spool_is_removed_before_the_error_returns() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpoolStore::open(dir.path(), 4096).unwrap();
        let chunks = vec![
            Ok::<Bytes, Infallible>(Bytes::from(vec![b'x'; 64])),
            Ok::<Bytes, Infallible>(Bytes::from(vec![b'y'; 64])),
        ];
        let body = Body::from_stream(stream::iter(chunks));
        let error = capture_body(body, store.clone(), 8, 100).await.unwrap_err();
        assert_eq!(error, CaptureError::TooLarge);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
        assert_eq!(store.budget().used(), 0);
    }

    #[tokio::test]
    async fn cancelling_capture_removes_the_partial_spool() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpoolStore::open(dir.path(), 4096).unwrap();
        let first = stream::iter(vec![Ok::<Bytes, Infallible>(Bytes::from(vec![b'x'; 128]))]);
        let pending = stream::pending::<Result<Bytes, Infallible>>();
        let body = Body::from_stream(first.chain(pending));
        let capture = tokio::spawn(capture_body(body, store.clone(), 8, 4096));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if fs::read_dir(dir.path())
                    .unwrap()
                    .filter_map(Result::ok)
                    .any(|entry| {
                        entry
                            .file_name()
                            .to_str()
                            .is_some_and(|name| name.ends_with(".tmp"))
                    })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();

        capture.abort();
        let _ = capture.await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if fs::read_dir(dir.path()).unwrap().count() == 0 && store.budget().used() == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(store.budget().used(), 0);
    }
}
