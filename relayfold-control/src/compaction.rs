//! Crash-safe log compaction: write a snapshot of the whole state, then replace the log with an
//! empty one whose first record names the next *epoch*.
//!
//! # Protocol
//!
//! ```text
//! control.snapshot   CRC-framed Snapshot { covers_epoch: E, state }       (atomic: tmp + rename)
//! control.log        [Epoch{E}]? record record record ...                  (legacy log: no header = E 0)
//!
//! compact:  1. write control.snapshot.tmp   2. fsync it   3. rename -> control.snapshot
//!           4. fsync the directory          (the snapshot is now durable and covers every log record)
//!           5. write control.log.tmp = [Epoch{E+1}]   6. fsync it   7. rename -> control.log
//!           8. fsync the directory          (the truncation is now durable)
//! ```
//!
//! `store::open` decides what a directory holds from `(snapshot.covers_epoch, log epoch)`:
//!
//! | log epoch vs snapshot | meaning | state |
//! |---|---|---|
//! | no snapshot, log epoch 0 | never compacted | replay the log |
//! | log == covers | killed between 4 and 8: the log is still covered | snapshot only, then finish steps 5-8 |
//! | log == covers + 1 | compacted | snapshot + the log records after the header |
//! | anything else | a snapshot or log was lost / swapped | **refuse to start** |
//!
//! so a kill at *any* point between two steps recovers to exactly the pre-compaction state, and
//! a lost snapshot can never be mistaken for an empty fleet.
//!
//! # The plan is data
//!
//! The order of the eight steps *is* the crash-safety argument, so it is a value
//! ([`REAL_PLAN`]) the executor [`run`] interprets and `Stages::compaction_plan` can swap. The
//! mutants in `tests/negative_controls.rs` reorder or drop steps (e.g. truncate before the
//! snapshot is fsynced) and the same checker must reject them.
//!
//! # Fault injection
//!
//! [`run`] talks to the disk only through [`Fs`]. [`LossyFs`] wraps real files and can (a) kill
//! the process after `k` operations and (b) apply the **worst-case power loss** afterwards: every
//! file written but not fsynced is truncated to zero, every rename not covered by a directory
//! fsync is undone. That is a model of POSIX durability (the classic zero-length-file-after-rename
//! failure), not a filesystem; real hardware is not exercised.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const SNAPSHOT_FILE: &str = "control.snapshot";
pub const SNAPSHOT_TMP: &str = "control.snapshot.tmp";
pub const LOG_TMP: &str = "control.log.tmp";

/// One step of the compaction protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    WriteSnapshot,
    /// fsync the snapshot file (at its tmp name, or at its final name once renamed).
    FsyncSnapshot,
    RenameSnapshot,
    WriteLog,
    FsyncLog,
    /// The truncation: atomically replace the log with the new, empty-epoch log.
    RenameLog,
    FsyncDir,
}

/// What ships: snapshot durable (fsync, rename, dir fsync) **before** the log is replaced.
pub const REAL_PLAN: &[Step] = &[
    Step::WriteSnapshot,
    Step::FsyncSnapshot,
    Step::RenameSnapshot,
    Step::FsyncDir,
    Step::WriteLog,
    Step::FsyncLog,
    Step::RenameLog,
    Step::FsyncDir,
];

/// Recovery's completion of an interrupted compaction: the snapshot is already durable.
pub const FINISH_PLAN: &[Step] = &[
    Step::WriteLog,
    Step::FsyncLog,
    Step::RenameLog,
    Step::FsyncDir,
];

/// The disk operations compaction uses, one call = one crash point.
pub trait Fs: Send {
    /// Create (or truncate) `path` and write `bytes`; NOT durable until `fsync_file`.
    fn write_new(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    fn fsync_file(&mut self, path: &Path) -> io::Result<()>;
    /// Atomically replace `to` with `from`; NOT durable until `fsync_dir`.
    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()>;
    fn fsync_dir(&mut self, dir: &Path) -> io::Result<()>;
}

pub struct RealFs;

impl Fs for RealFs {
    fn write_new(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let mut f = fs::File::create(path)?;
        f.write_all(bytes)
    }

    fn fsync_file(&mut self, path: &Path) -> io::Result<()> {
        fs::File::open(path)?.sync_all()
    }

    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    fn fsync_dir(&mut self, dir: &Path) -> io::Result<()> {
        fs::File::open(dir)?.sync_all()
    }
}

/// Interpret `plan` against `fs` in `dir`. `snapshot` / `log_header` are the already-framed bytes
/// of the new snapshot file and of the new log. Stops at the first error, leaving the disk as the
/// steps so far left it (that is the crash).
pub fn run(
    fs: &mut dyn Fs,
    dir: &Path,
    plan: &[Step],
    snapshot: &[u8],
    log_header: &[u8],
) -> io::Result<()> {
    let snap_tmp = dir.join(SNAPSHOT_TMP);
    let snap = dir.join(SNAPSHOT_FILE);
    let log_tmp = dir.join(LOG_TMP);
    let log = dir.join(crate::store::LOG_FILE);
    let mut snap_at = snap_tmp.clone();
    let mut log_at = log_tmp.clone();
    for step in plan {
        match step {
            Step::WriteSnapshot => fs.write_new(&snap_tmp, snapshot)?,
            Step::FsyncSnapshot => fs.fsync_file(&snap_at)?,
            Step::RenameSnapshot => {
                fs.rename(&snap_tmp, &snap)?;
                snap_at = snap.clone();
            }
            Step::WriteLog => fs.write_new(&log_tmp, log_header)?,
            Step::FsyncLog => fs.fsync_file(&log_at)?,
            Step::RenameLog => {
                fs.rename(&log_tmp, &log)?;
                log_at = log.clone();
            }
            Step::FsyncDir => fs.fsync_dir(dir)?,
        }
    }
    Ok(())
}

// ── fault injection ────────────────────────────────────────────────────────────────────────

struct PendingRename {
    from: PathBuf,
    to: PathBuf,
    /// What `to` held before the rename (None: it did not exist).
    displaced: Option<Vec<u8>>,
    dir: PathBuf,
}

#[derive(Default)]
struct LossyState {
    /// `Some(n)`: `n` more operations succeed, the next one fails as "killed" without running.
    ops_left: Option<usize>,
    ops_done: usize,
    /// Files written and not yet fsynced.
    unsynced: BTreeSet<PathBuf>,
    /// Renames not yet covered by a directory fsync.
    pending: Vec<PendingRename>,
}

/// Real files plus a crash model; cheap to clone (shared state), so a test can keep a handle
/// after giving one to the code under test.
#[derive(Clone, Default)]
pub struct LossyFs(Arc<Mutex<LossyState>>);

fn killed() -> io::Error {
    io::Error::other("process killed (injected)")
}

impl LossyFs {
    /// `kill_after_ops = Some(k)`: the first `k` operations run, the `k+1`-th "kills the process".
    pub fn new(kill_after_ops: Option<usize>) -> Self {
        LossyFs(Arc::new(Mutex::new(LossyState {
            ops_left: kill_after_ops,
            ..LossyState::default()
        })))
    }

    pub fn ops_done(&self) -> usize {
        self.0.lock().map(|s| s.ops_done).unwrap_or(0)
    }

    fn tick(&self) -> io::Result<()> {
        let mut s = self.0.lock().map_err(|_| killed())?;
        match s.ops_left {
            Some(0) => Err(killed()),
            Some(n) => {
                s.ops_left = Some(n - 1);
                s.ops_done += 1;
                Ok(())
            }
            None => {
                s.ops_done += 1;
                Ok(())
            }
        }
    }

    /// Apply the worst-case power loss to the real directory: undo every rename no directory
    /// fsync covered, then truncate to zero every file whose data no fsync covered.
    pub fn power_loss(&self) -> io::Result<()> {
        let mut s = self.0.lock().map_err(|_| killed())?;
        let pending = std::mem::take(&mut s.pending);
        for p in pending.iter().rev() {
            let moved = fs::read(&p.to).ok();
            match &p.displaced {
                Some(old) => fs::write(&p.to, old)?,
                None => {
                    let _ = fs::remove_file(&p.to);
                }
            }
            if let Some(content) = moved {
                fs::write(&p.from, content)?;
            }
            if s.unsynced.remove(&p.to) {
                s.unsynced.insert(p.from.clone());
            }
        }
        for path in std::mem::take(&mut s.unsynced) {
            if path.exists() {
                fs::File::create(&path)?;
            }
        }
        Ok(())
    }
}

impl Fs for LossyFs {
    fn write_new(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.tick()?;
        RealFs.write_new(path, bytes)?;
        if let Ok(mut s) = self.0.lock() {
            s.unsynced.insert(path.to_path_buf());
        }
        Ok(())
    }

    fn fsync_file(&mut self, path: &Path) -> io::Result<()> {
        self.tick()?;
        RealFs.fsync_file(path)?;
        if let Ok(mut s) = self.0.lock() {
            s.unsynced.remove(path);
        }
        Ok(())
    }

    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        self.tick()?;
        let displaced = fs::read(to).ok();
        RealFs.rename(from, to)?;
        if let Ok(mut s) = self.0.lock() {
            if s.unsynced.remove(from) {
                s.unsynced.insert(to.to_path_buf());
            }
            s.pending.push(PendingRename {
                from: from.to_path_buf(),
                to: to.to_path_buf(),
                displaced,
                dir: to.parent().unwrap_or(Path::new(".")).to_path_buf(),
            });
        }
        Ok(())
    }

    fn fsync_dir(&mut self, dir: &Path) -> io::Result<()> {
        self.tick()?;
        RealFs.fsync_dir(dir)?;
        if let Ok(mut s) = self.0.lock() {
            s.pending.retain(|p| p.dir != dir);
        }
        Ok(())
    }
}

/// What a compaction did, for logs and the benchmark.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompactionReport {
    /// Log records folded into the snapshot (those appended since the last compaction).
    pub records_folded: u64,
    pub log_bytes_before: u64,
    pub log_bytes_after: u64,
    pub snapshot_bytes: u64,
}
