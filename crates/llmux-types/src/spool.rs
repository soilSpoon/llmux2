//! Disk-backed replay storage for large request bodies.
//!
//! A finalized spool is immutable. Its file handle is kept open for replay,
//! while its path is retained only so the owner can unlink it. Dropping the
//! last `SpooledBody` closes and removes the file and releases its disk
//! budget reservation.

use bytes::Bytes;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tempfile::{NamedTempFile, PersistError};
use thiserror::Error;

const PREFIX: &str = ".llmux-body-";
const TEMP_SUFFIX: &str = ".tmp";
const FINAL_SUFFIX: &str = "spool";

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum SpoolError {
    #[error("request body spool storage capacity exhausted")]
    BudgetExhausted,
    #[error("request body spool I/O failed")]
    Io,
    #[error("request body spool write was cancelled")]
    Cancelled,
}

/// Process-wide accounting for finalized and in-progress spooled bodies.
/// Reservations are made before bytes hit disk, so a full budget fails closed
/// instead of discovering exhaustion after a partial write.
#[derive(Debug)]
pub struct SpoolBudget {
    capacity: AtomicU64,
    used: AtomicU64,
}

impl SpoolBudget {
    pub fn new(capacity: u64) -> Self {
        Self {
            capacity: AtomicU64::new(capacity),
            used: AtomicU64::new(0),
        }
    }

    pub fn capacity(&self) -> u64 {
        self.capacity.load(Ordering::Relaxed)
    }

    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    pub fn set_capacity(&self, capacity: u64) {
        self.capacity.store(capacity, Ordering::Relaxed);
    }

    fn reserve(&self, bytes: u64) -> Result<(), SpoolError> {
        if bytes == 0 {
            return Ok(());
        }
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let capacity = self.capacity.load(Ordering::Acquire);
            let Some(next) = used.checked_add(bytes) else {
                return Err(SpoolError::BudgetExhausted);
            };
            if next > capacity {
                return Err(SpoolError::BudgetExhausted);
            }
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Ok(()),
                Err(current) => used = current,
            }
        }
    }

    fn release(&self, bytes: u64) {
        if bytes != 0 {
            self.used.fetch_sub(bytes, Ordering::AcqRel);
        }
    }
}

/// Byte reservation held by one spool file. Dropping it releases every byte
/// that was successfully reserved, including bytes reserved for a write that
/// later failed.
#[derive(Debug)]
pub struct SpoolReservation {
    budget: Arc<SpoolBudget>,
    bytes: AtomicU64,
}

impl SpoolReservation {
    fn new(budget: Arc<SpoolBudget>) -> Self {
        Self {
            budget,
            bytes: AtomicU64::new(0),
        }
    }

    fn reserve(&self, bytes: u64) -> Result<(), SpoolError> {
        self.budget.reserve(bytes)?;
        self.bytes.fetch_add(bytes, Ordering::AcqRel);
        Ok(())
    }
}

impl Drop for SpoolReservation {
    fn drop(&mut self) {
        self.budget.release(self.bytes.load(Ordering::Acquire));
    }
}

#[derive(Clone, Debug)]
pub struct SpoolStore {
    dir: PathBuf,
    budget: Arc<SpoolBudget>,
}

impl SpoolStore {
    /// Opens or creates the spool directory and sweeps orphans left by a
    /// crash. Only unlocked files with llmux's spool prefix are removed, so a
    /// live overlapping zero-downtime process keeps its files on Unix.
    pub fn open(dir: impl Into<PathBuf>, budget_bytes: u64) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        sweep(&dir)?;
        Ok(Self {
            dir,
            budget: Arc::new(SpoolBudget::new(budget_bytes)),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn budget(&self) -> &Arc<SpoolBudget> {
        &self.budget
    }

    pub fn set_budget(&self, budget_bytes: u64) {
        self.budget.set_capacity(budget_bytes);
    }

    pub fn begin(&self) -> Result<SpoolFile, SpoolError> {
        let file = tempfile::Builder::new()
            .prefix(PREFIX)
            .suffix(TEMP_SUFFIX)
            .tempfile_in(&self.dir)
            .map_err(|_| SpoolError::Io)?;
        lock_exclusive(file.as_file()).map_err(|_| SpoolError::Io)?;
        Ok(SpoolFile {
            file,
            len: 0,
            reservation: SpoolReservation::new(self.budget.clone()),
        })
    }

    pub fn spool_bytes(&self, bytes: &[u8]) -> Result<Arc<SpooledBody>, SpoolError> {
        let mut file = self.begin()?;
        file.write(bytes)?;
        file.finish().map(Arc::new)
    }
}

fn sweep(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with(PREFIX) && (name.ends_with(TEMP_SUFFIX) || name.ends_with(FINAL_SUFFIX))
        {
            remove_if_unlocked(&path);
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn lock_exclusive(file: &File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn lock_exclusive(_: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn try_lock_exclusive(file: &File) -> io::Result<bool> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn try_lock_exclusive(_: &File) -> io::Result<bool> {
    Ok(true)
}

/// Removes a spool only when no live process holds its flock. Crash orphans
/// are unlocked and disappear at the next startup; files owned by an
/// overlapping zero-downtime process are left alone.
fn remove_if_unlocked(path: &Path) {
    let Ok(file) = OpenOptions::new().read(true).open(path) else {
        return;
    };
    if let Ok(true) = try_lock_exclusive(&file) {
        let _ = fs::remove_file(path);
    }
}

pub struct SpoolFile {
    file: NamedTempFile,
    len: u64,
    reservation: SpoolReservation,
}

impl SpoolFile {
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), SpoolError> {
        let len = u64::try_from(bytes.len()).map_err(|_| SpoolError::Io)?;
        self.reservation.reserve(len)?;
        self.file.write_all(bytes).map_err(|_| SpoolError::Io)?;
        self.len = self.len.checked_add(len).ok_or(SpoolError::Io)?;
        Ok(())
    }

    /// Flushes the temporary file and atomically renames it as a finalized
    /// spool. If the process crashes first, only the `.tmp` orphan can exist.
    pub fn finish(self) -> Result<SpooledBody, SpoolError> {
        let Self {
            file,
            len,
            reservation,
        } = self;
        file.as_file().sync_all().map_err(|_| SpoolError::Io)?;
        let path = file.path().with_extension(FINAL_SUFFIX);
        let file = match file.persist(&path) {
            Ok(file) => file,
            Err(PersistError { error, .. }) => return Err(SpoolError::from_persist(error)),
        };
        Ok(SpooledBody {
            file: Mutex::new(Some(file)),
            path,
            len,
            read_count: AtomicU64::new(0),
            _reservation: reservation,
        })
    }
}

impl SpoolError {
    fn from_persist(_: io::Error) -> Self {
        Self::Io
    }
}

#[derive(Debug)]
pub struct SpooledBody {
    file: Mutex<Option<File>>,
    path: PathBuf,
    len: u64,
    read_count: AtomicU64,
    _reservation: SpoolReservation,
}

impl SpooledBody {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Number of replay reads started. Exposed for lifecycle verification and diagnostics.
    pub fn read_count(&self) -> u64 {
        self.read_count.load(Ordering::Relaxed)
    }

    /// Reads the complete immutable body through the retained file handle.
    /// Callers on an async runtime should run this in a blocking thread.
    pub fn read_bytes(&self) -> io::Result<Bytes> {
        self.read_count.fetch_add(1, Ordering::Relaxed);
        let mut guard = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(file) = guard.as_mut() else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "spool file closed"));
        };
        file.seek(SeekFrom::Start(0))?;
        let len = usize::try_from(self.len).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "spool body does not fit in memory",
            )
        })?;
        let mut body = Vec::with_capacity(len);
        file.take(len as u64).read_to_end(&mut body)?;
        if body.len() != len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "spool body is incomplete",
            ));
        }
        Ok(Bytes::from(body))
    }
}

impl Drop for SpooledBody {
    fn drop(&mut self) {
        let mut guard = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        drop(guard.take());
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_spool_roundtrips_and_exposes_store_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpoolStore::open(dir.path(), 4).unwrap();
        assert_eq!(store.dir(), dir.path());
        assert_eq!(store.budget().capacity(), 4);
        let body = store.spool_bytes(b"").unwrap();
        assert_eq!(body.read_bytes().unwrap(), Bytes::new());
        assert_eq!(store.budget().used(), 0);
    }

    #[test]
    fn reservation_addition_overflow_fails_closed() {
        let budget = SpoolBudget::new(u64::MAX);
        budget.reserve(u64::MAX).unwrap();
        assert_eq!(budget.reserve(1), Err(SpoolError::BudgetExhausted));
    }

    #[test]
    fn budget_releases_after_spool_drops() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpoolStore::open(dir.path(), 16).unwrap();
        let budget = store.budget().clone();
        let body = store.spool_bytes(b"0123456789").unwrap();
        assert_eq!(budget.used(), 10);
        assert_eq!(
            body.read_bytes().unwrap(),
            Bytes::from_static(b"0123456789")
        );
        let path = body.path().to_path_buf();
        assert!(path.exists());
        drop(body);
        assert_eq!(budget.used(), 0);
        assert!(!path.exists());
    }

    #[test]
    fn budget_capacity_can_be_reconfigured_live() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpoolStore::open(dir.path(), 4).unwrap();
        assert_eq!(
            store.spool_bytes(b"12345").unwrap_err(),
            SpoolError::BudgetExhausted
        );
        store.set_budget(8);
        let body = store.spool_bytes(b"12345").unwrap();
        assert_eq!(store.budget().used(), 5);
        drop(body);
    }

    #[test]
    fn budget_fails_closed_before_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpoolStore::open(dir.path(), 4).unwrap();
        assert_eq!(
            store.spool_bytes(b"12345").unwrap_err(),
            SpoolError::BudgetExhausted
        );
        assert_eq!(store.budget().used(), 0);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn persist_failure_cleans_the_temporary_file_and_releases_budget() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpoolStore::open(dir.path(), 16).unwrap();
        let mut file = store.begin().unwrap();
        file.write(b"12345").unwrap();
        let final_path = file.file.path().with_extension(FINAL_SUFFIX);
        fs::create_dir(&final_path).unwrap();
        assert_eq!(file.finish().unwrap_err(), SpoolError::Io);
        assert_eq!(store.budget().used(), 0);
        assert!(!fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.path().is_file()));
        fs::remove_dir(final_path).unwrap();
    }

    #[test]
    fn closed_or_truncated_spool_reads_fail_instead_of_replaying_damage() {
        let dir = tempfile::tempdir().unwrap();
        let store = SpoolStore::open(dir.path(), 64).unwrap();
        let closed = store.spool_bytes(b"closed").unwrap();
        closed.file.lock().unwrap().take();
        assert_eq!(
            closed.read_bytes().unwrap_err().kind(),
            io::ErrorKind::NotFound
        );

        let truncated = store.spool_bytes(b"complete").unwrap();
        let file = OpenOptions::new()
            .write(true)
            .open(truncated.path())
            .unwrap();
        file.set_len(2).unwrap();
        drop(file);
        assert_eq!(
            truncated.read_bytes().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn startup_sweep_preserves_locked_overlap_files() {
        let dir = tempfile::tempdir().unwrap();
        let active = SpoolStore::open(dir.path(), 64).unwrap();
        let spooled = active.spool_bytes(b"live").unwrap();
        let live_path = spooled.path().to_path_buf();
        let mut writing = active.begin().unwrap();
        writing.write(b"partial").unwrap();
        let temp_path = writing.file.path().to_path_buf();

        SpoolStore::open(dir.path(), 64).unwrap();
        assert!(live_path.exists());
        assert!(temp_path.exists());
        assert_eq!(spooled.read_bytes().unwrap(), Bytes::from_static(b"live"));
        drop(writing);
        assert!(!temp_path.exists());
    }

    #[test]
    fn startup_sweep_removes_only_llmux_spool_names() {
        let dir = tempfile::tempdir().unwrap();
        let orphan = dir.path().join(".llmux-body-old.spool");
        let temp = dir.path().join(".llmux-body-old.tmp");
        let unrelated = dir.path().join("other.spool");
        fs::write(&orphan, b"x").unwrap();
        fs::write(&temp, b"x").unwrap();
        fs::write(&unrelated, b"x").unwrap();
        let directory = dir.path().join(".llmux-body-dir.spool");
        fs::create_dir(&directory).unwrap();
        SpoolStore::open(dir.path(), 16).unwrap();
        assert!(!orphan.exists());
        assert!(!temp.exists());
        assert!(unrelated.exists());
        assert!(directory.is_dir());
    }
}
