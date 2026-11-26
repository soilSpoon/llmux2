//! Streaming ingress capture for request bodies.
//!
//! Small bodies stay in memory. Once buffered bytes would cross
//! `request_body_memory_bytes`, capture rolls over to a bounded blocking writer
//! so network delivery does not run ahead of disk admission.

use axum::body::{Body, Bytes};
use futures_util::StreamExt;
use llmux_types::SpoolFile;
use llmux_types::{SpoolError, SpoolStore, SpooledBody};
use std::sync::Arc;

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
            Err(_) => return Err(CaptureError::Read),
        };
        if chunk.is_empty() {
            continue;
        }
        let Some(next_total) = total
            .checked_add(chunk.len())
            .filter(|total| *total <= max_bytes)
        else {
            return Err(CaptureError::TooLarge);
        };
        total = next_total;
        if let Some(writer) = &mut disk {
            writer.write(chunk).await?;
        } else if memory.len() + chunk.len() <= memory_threshold {
            memory.extend_from_slice(&chunk);
        } else {
            let mut writer = DiskWriter::start(store.clone()).await?;
            let buffered = Bytes::from(std::mem::take(&mut memory));
            if !buffered.is_empty() {
                writer.write(buffered).await?;
            }
            writer.write(chunk).await?;
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

struct DiskWriter {
    file: Option<SpoolFile>,
}

impl DiskWriter {
    async fn start(store: SpoolStore) -> Result<Self, CaptureError> {
        let file = tokio::task::spawn_blocking(move || store.begin())
            .await
            .map_err(|_| CaptureError::Io)?
            .map_err(CaptureError::from)?;
        Ok(Self { file: Some(file) })
    }

    async fn write(&mut self, chunk: Bytes) -> Result<(), CaptureError> {
        let mut file = self.file.take().expect("disk writer file is present");
        let (file, result) = tokio::task::spawn_blocking(move || {
            let result = file.write(&chunk);
            (file, result)
        })
        .await
        .map_err(|_| CaptureError::Io)?;
        result.map_err(CaptureError::from)?;
        self.file = Some(file);
        Ok(())
    }

    async fn finish(mut self) -> Result<SpooledBody, CaptureError> {
        let file = self.file.take().expect("disk writer file is present");
        tokio::task::spawn_blocking(move || file.finish())
            .await
            .map_err(|_| CaptureError::Io)?
            .map_err(CaptureError::from)
    }
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

    #[test]
    fn stalled_spooled_ingress_releases_blocking_workers_between_writes() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
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

            let probe =
                tokio::time::timeout(Duration::from_secs(1), tokio::task::spawn_blocking(|| 42))
                    .await
                    .expect("stalled ingress must not occupy the only blocking worker")
                    .unwrap();
            assert_eq!(probe, 42);

            capture.abort();
            let _ = capture.await;
        });
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
