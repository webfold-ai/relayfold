//! Reload triggers: SIGHUP and a config-file watch.
//!
//! Both funnel into the same one-line call — "this file changed, re-read it" — because the
//! two triggers differ only in *when* they fire, never in what happens next.
//!
//! **The watch is a 250 ms stat poll (mtime + length + inode), not inotify.** The requirement
//! is a 250 ms debounce, and a poll at the debounce interval satisfies it exactly while also
//! seeing the two ways this file actually changes in production: an editor's
//! write-then-rename, and a Kubernetes ConfigMap update, which is a *symlink swap* that
//! inotify on the watched path does not report at all. A poll that stats the resolved path
//! catches both. The cost is four `stat`s a second on one file.
//!
//! A reload that fails to parse or validate reports the error and changes NOTHING — the
//! last-good table keeps serving. A typo in the config must not take the proxy down.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Why a reload fired, for the log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    Sighup,
    FileChanged,
    /// The very first load, before anything is being served.
    Startup,
}

impl Trigger {
    pub fn label(self) -> &'static str {
        match self {
            Trigger::Sighup => "SIGHUP",
            Trigger::FileChanged => "config file changed",
            Trigger::Startup => "startup",
        }
    }
}

/// What to stat, and what "changed" means. Tracked separately from the poll so the change
/// DETECTION is testable without sleeping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    /// Modification time, truncated to seconds — some filesystems have coarse mtime, so
    /// length and inode carry the rest of the signal.
    mtime_secs: i64,
    mtime_nanos: u32,
    len: u64,
    /// Zero when the file is absent.
    ino: u64,
}

impl Fingerprint {
    /// `None` when the file does not exist — a distinct state, because a ConfigMap briefly
    /// removes the old file while swapping in the new one and that must not read as
    /// "unchanged" forever.
    pub fn of(path: &Path) -> Option<Self> {
        let md = std::fs::metadata(path).ok()?;
        let mtime = md.modified().ok()?;
        let dur = mtime.duration_since(SystemTime::UNIX_EPOCH).ok()?;
        Some(Fingerprint {
            mtime_secs: dur.as_secs() as i64,
            mtime_nanos: dur.subsec_nanos(),
            len: md.len(),
            #[cfg(unix)]
            ino: {
                use std::os::unix::fs::MetadataExt;
                md.ino()
            },
            #[cfg(not(unix))]
            ino: 0,
        })
    }
}

/// Spawn the SIGHUP listener. Returns `Err` where the platform has no SIGHUP (Windows),
/// which the caller logs rather than treating as fatal — the file watch still works.
#[cfg(unix)]
pub fn spawn_sighup(
    tx: tokio::sync::mpsc::UnboundedSender<Trigger>,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sig = signal(SignalKind::hangup())?;
    Ok(tokio::spawn(async move {
        while sig.recv().await.is_some() {
            // A closed receiver means the runtime is shutting down; stop rather than spin.
            if tx.send(Trigger::Sighup).is_err() {
                return;
            }
        }
    }))
}

#[cfg(not(unix))]
pub fn spawn_sighup(
    _tx: tokio::sync::mpsc::UnboundedSender<Trigger>,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "SIGHUP is not available on this platform",
    ))
}

/// The debounce interval. 250 ms is the requirement, and it is also the right granularity for
/// the thing being debounced: a ConfigMap update lands as a symlink swap a few milliseconds
/// apart, and an operator's editor writes the file and renames it in the same tick.
pub const DEBOUNCE: Duration = Duration::from_millis(250);

/// Spawn the config-file poll. The first poll establishes the baseline WITHOUT firing: the
/// file was just read at startup, and re-reading it immediately would report a no-op reload
/// on every boot.
pub fn spawn_file_watch(
    path: PathBuf,
    poll: Duration,
    tx: tokio::sync::mpsc::UnboundedSender<Trigger>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Baseline from the file as it is RIGHT NOW, so booting does not report a change the
        // operator did not make.
        let mut last = Fingerprint::of(&path);
        loop {
            tokio::time::sleep(poll).await;
            let now = Fingerprint::of(&path);
            if now == last {
                continue;
            }
            let present = now.is_some();
            last = now;
            // Fire on ANY transition to a present file: modified in place (mtime/length),
            // replaced by an atomic rename (inode), or reappeared after a ConfigMap swap.
            // Do NOT fire on a transition to absent — a file the editor briefly unlinks is not
            // a config, and reloading it would only produce an error the operator did not
            // cause. The next transition back to present fires.
            if present && tx.send(Trigger::FileChanged).is_err() {
                return;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        // Scratch lives under ~/projects/.agent-work/scratch/, never /tmp (world-readable,
        // no cleanup, visible in every `ls /tmp`). `CARGO_TARGET_TMPDIR` is not usable here:
        // it only exists under `cargo test`, and this file must also compile for a plain build.
        let home = std::env::var("HOME").expect("HOME");
        let dir = PathBuf::from(home).join(".agent-work/scratch/relayfold-reload-tests");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn an_unchanged_file_keeps_its_fingerprint() {
        let p = scratch("same.yaml");
        std::fs::write(&p, b"routes: []\n").unwrap();
        let a = Fingerprint::of(&p).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let b = Fingerprint::of(&p).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn a_changed_length_is_detected_even_at_coarse_mtime_resolution() {
        let p = scratch("len.yaml");
        std::fs::write(&p, b"a: 1\n").unwrap();
        let a = Fingerprint::of(&p).unwrap();
        std::fs::write(&p, b"a: 123456789\n").unwrap();
        assert_ne!(a, Fingerprint::of(&p).unwrap());
    }

    #[test]
    fn an_atomic_rename_is_detected_via_the_inode() {
        // The ConfigMap/kubectl shape: write a new file beside the old, rename over it.
        // mtime can be identical to the nanosecond on a fast filesystem; the inode cannot.
        let p = scratch("rename.yaml");
        std::fs::write(&p, b"a: 1\n").unwrap();
        let a = Fingerprint::of(&p).unwrap();
        let tmp = scratch("rename.yaml.tmp");
        std::fs::write(&tmp, b"b: 2\n").unwrap();
        std::fs::rename(&tmp, &p).unwrap();
        assert_ne!(a, Fingerprint::of(&p).unwrap());
    }

    #[test]
    fn a_vanished_file_is_distinct_from_an_unchanged_one() {
        let p = scratch("gone.yaml");
        std::fs::write(&p, b"a: 1\n").unwrap();
        let before = Fingerprint::of(&p);
        std::fs::remove_file(&p).unwrap();
        assert_ne!(before, Fingerprint::of(&p));
        assert_eq!(Fingerprint::of(&p), None);
    }

    #[test]
    fn a_file_that_appears_later_is_detected_as_a_reappearance() {
        let p = scratch("later.yaml");
        let _ = std::fs::remove_file(&p);
        assert_eq!(Fingerprint::of(&p), None);
        std::fs::write(&p, b"a: 1\n").unwrap();
        let now = Fingerprint::of(&p);
        assert_ne!(
            now, None,
            "the watcher must notice a file that was not there"
        );
    }
}
