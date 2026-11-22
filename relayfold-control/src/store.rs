//! Durable state: an append-only log on `iotadb-log` (CRC32-framed records, `fsync` after every
//! record, torn tail truncated to the longest valid prefix on open), plus a **snapshot** that
//! [`compaction`](crate::compaction) writes so the log does not grow forever.
//!
//! # What is logged — and what deliberately is not
//!
//! * `Registered` — an id's address/version **and the public key it is bound to**. Written when an
//!   id first registers, or when a re-register by the *bound* key changes addr/version.
//! * `ConfigPushed` — the control-plane `config_version`, written **before** delivery so the
//!   counter is strictly increasing across restarts.
//! * `EnrollmentRejected` — a register by a key that is not enrolled (the audit trail the
//!   enrollment gate owes). Only the most recent [`AUDIT_CAP`] survive a replay or a compaction.
//! * `Epoch` — only ever the *first* record of a compacted log: which compaction produced it.
//! * Heartbeats are **not** logged. They are liveness, not identity: logging one per instance per
//!   interval would make the log grow with time, and a restarted control plane is right to call
//!   every instance not-healthy until it is heard from again — replay yields
//!   [`Liveness::UNSEEN`].
//!
//! # Compaction
//!
//! See [`crate::compaction`] for the protocol and the crash argument. This module owns the formats
//! (`Snapshot`, the `Epoch` header) and the recovery rule in [`open_with_fs`]:
//! `replay(snapshot + tail) == replay(full log)`, and a directory whose snapshot and log disagree
//! about the epoch refuses to open instead of forgetting identity bindings.
//!
//! # Failure semantics
//!
//! [`RecordSink::append`] returns an error; the control plane propagates it (HTTP 500) and does
//! **not** apply the change in memory. After any append error [`LogSink`] is *poisoned* and
//! refuses further appends: a failed `write` can leave a partial frame in the file, and the next
//! record would land behind it, where recovery would later truncate it away. A compaction that
//! fails *after* its snapshot became visible (or after the log was replaced) poisons it too
//! (appending to a log the snapshot already covers would lose the record); one that fails before
//! that, e.g. ENOSPC writing the snapshot tmp file, leaves nothing visible and the sink usable.
//! Restart truncates the torn tail, finishes an interrupted compaction and clears the condition.

use crate::auth::{Key, NonceEntry};
use crate::compaction::{self, CompactionReport, Fs, RealFs, Step};
use iotadb_log::{FileWriter, FsyncPolicy, Log, RecoveryReport};
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};

pub const LOG_FILE: &str = "control.log";
pub use crate::compaction::SNAPSHOT_FILE;

/// How many enrollment rejections the audit trail keeps (log replay and snapshots alike).
pub const AUDIT_CAP: usize = 256;

/// A register that was refused because the signer is not enrolled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejection {
    pub key: Key,
    /// The id the caller asked for, cut to 128 bytes (the caller is unauthenticated-ish: bound it).
    pub id: String,
    pub at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    Registered {
        id: String,
        addr: String,
        version: String,
        key: Key,
    },
    ConfigPushed {
        version: u64,
    },
    EnrollmentRejected(Rejection),
    Epoch {
        n: u64,
    },
}

const TAG_REGISTERED: u8 = 1;
const TAG_CONFIG_PUSHED: u8 = 2;
const TAG_ENROLLMENT_REJECTED: u8 = 3;
const TAG_EPOCH: u8 = 4;

#[derive(Debug, PartialEq, Eq)]
pub struct DecodeError(pub &'static str);

fn put(out: &mut Vec<u8>, field: &[u8]) {
    out.extend_from_slice(&(field.len() as u32).to_be_bytes());
    out.extend_from_slice(field);
}

fn put_u64(out: &mut Vec<u8>, n: u64) {
    put(out, &n.to_be_bytes());
}

fn put_i64(out: &mut Vec<u8>, n: i64) {
    put(out, &n.to_be_bytes());
}

struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.0.len() < n {
            return Err(DecodeError("truncated record"));
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }

    fn field(&mut self) -> Result<&'a [u8], DecodeError> {
        let len = u32::from_be_bytes(
            self.take(4)?
                .try_into()
                .map_err(|_| DecodeError("bad length"))?,
        );
        self.take(len as usize)
    }

    fn string(&mut self) -> Result<String, DecodeError> {
        String::from_utf8(self.field()?.to_vec()).map_err(|_| DecodeError("non-utf8 string"))
    }

    fn key(&mut self) -> Result<Key, DecodeError> {
        self.field()?
            .try_into()
            .map_err(|_| DecodeError("key is not 32 bytes"))
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_be_bytes(
            self.field()?
                .try_into()
                .map_err(|_| DecodeError("integer is not 8 bytes"))?,
        ))
    }

    fn i64(&mut self) -> Result<i64, DecodeError> {
        Ok(i64::from_be_bytes(
            self.field()?
                .try_into()
                .map_err(|_| DecodeError("integer is not 8 bytes"))?,
        ))
    }
}

impl Record {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Record::Registered {
                id,
                addr,
                version,
                key,
            } => {
                out.push(TAG_REGISTERED);
                put(&mut out, id.as_bytes());
                put(&mut out, addr.as_bytes());
                put(&mut out, version.as_bytes());
                put(&mut out, key);
            }
            Record::ConfigPushed { version } => {
                out.push(TAG_CONFIG_PUSHED);
                put_u64(&mut out, *version);
            }
            Record::EnrollmentRejected(r) => {
                out.push(TAG_ENROLLMENT_REJECTED);
                put(&mut out, &r.key);
                put(&mut out, r.id.as_bytes());
                put_i64(&mut out, r.at);
            }
            Record::Epoch { n } => {
                out.push(TAG_EPOCH);
                put_u64(&mut out, *n);
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Record, DecodeError> {
        let (&tag, rest) = bytes.split_first().ok_or(DecodeError("empty record"))?;
        let mut c = Cursor(rest);
        let rec = match tag {
            TAG_REGISTERED => Record::Registered {
                id: c.string()?,
                addr: c.string()?,
                version: c.string()?,
                key: c.key()?,
            },
            TAG_CONFIG_PUSHED => Record::ConfigPushed { version: c.u64()? },
            TAG_ENROLLMENT_REJECTED => Record::EnrollmentRejected(Rejection {
                key: c.key()?,
                id: c.string()?,
                at: c.i64()?,
            }),
            TAG_EPOCH => Record::Epoch { n: c.u64()? },
            _ => return Err(DecodeError("unknown record tag")),
        };
        if !c.0.is_empty() {
            return Err(DecodeError("trailing bytes"));
        }
        Ok(rec)
    }
}

/// An instance as the log knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bound {
    pub addr: String,
    pub version: String,
    pub key: Key,
}

// ── snapshot ───────────────────────────────────────────────────────────────────────────────

/// Everything a restart needs, as of the instant a compaction ran. `nonces` is the replay window:
/// the part of the replay-protection state that is otherwise memory-only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub instances: BTreeMap<String, Bound>,
    pub config_version: u64,
    /// Oldest first, at most [`AUDIT_CAP`].
    pub rejections: Vec<Rejection>,
    pub nonces: Vec<NonceEntry>,
}

const SNAPSHOT_VERSION: u8 = 1;

impl Snapshot {
    /// The payload of the snapshot file; `covers_epoch` is the log epoch it folds in.
    pub fn encode(&self, covers_epoch: u64) -> Vec<u8> {
        let mut out = vec![SNAPSHOT_VERSION];
        put_u64(&mut out, covers_epoch);
        put_u64(&mut out, self.config_version);
        put_u64(&mut out, self.instances.len() as u64);
        for (id, b) in &self.instances {
            put(&mut out, id.as_bytes());
            put(&mut out, b.addr.as_bytes());
            put(&mut out, b.version.as_bytes());
            put(&mut out, &b.key);
        }
        put_u64(&mut out, self.rejections.len() as u64);
        for r in &self.rejections {
            put(&mut out, &r.key);
            put(&mut out, r.id.as_bytes());
            put_i64(&mut out, r.at);
        }
        put_u64(&mut out, self.nonces.len() as u64);
        for n in &self.nonces {
            put(&mut out, &n.key);
            put(&mut out, n.nonce.as_bytes());
            put_i64(&mut out, n.ts);
        }
        out
    }

    /// Returns `(covers_epoch, snapshot)`.
    pub fn decode(bytes: &[u8]) -> Result<(u64, Snapshot), DecodeError> {
        let (&version, rest) = bytes.split_first().ok_or(DecodeError("empty snapshot"))?;
        if version != SNAPSHOT_VERSION {
            return Err(DecodeError("unknown snapshot version"));
        }
        let mut c = Cursor(rest);
        let covers = c.u64()?;
        let mut snap = Snapshot {
            config_version: c.u64()?,
            ..Snapshot::default()
        };
        for _ in 0..c.u64()? {
            let id = c.string()?;
            let b = Bound {
                addr: c.string()?,
                version: c.string()?,
                key: c.key()?,
            };
            snap.instances.insert(id, b);
        }
        for _ in 0..c.u64()? {
            snap.rejections.push(Rejection {
                key: c.key()?,
                id: c.string()?,
                at: c.i64()?,
            });
        }
        for _ in 0..c.u64()? {
            snap.nonces.push(NonceEntry {
                key: c.key()?,
                nonce: c.string()?,
                ts: c.i64()?,
            });
        }
        if !c.0.is_empty() {
            return Err(DecodeError("trailing bytes"));
        }
        Ok((covers, snap))
    }
}

fn frame(payload: &[u8]) -> Vec<u8> {
    iotadb_log::build_record(0, payload).encode()
}

fn unframe(bytes: &[u8]) -> Result<Vec<u8>, io::Error> {
    let (rec, used) = iotadb_log::decode_at(bytes, 0)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("snapshot frame: {e}")))?;
    if used != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "snapshot has trailing bytes",
        ));
    }
    Ok(rec.payload)
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

// ── replay ─────────────────────────────────────────────────────────────────────────────────

/// State rebuilt from the snapshot (if any) and the log on open.
#[derive(Debug, Default)]
pub struct Replayed {
    pub instances: BTreeMap<String, Bound>,
    pub config_version: u64,
    pub rejections: VecDeque<Rejection>,
    /// Replay window persisted by the last compaction (empty if never compacted).
    pub nonces: Vec<NonceEntry>,
    /// Log records applied on top of the snapshot (what the next compaction would fold).
    pub tail_records: u64,
    pub recovery: RecoveryReport,
}

/// The replay-equivalent part of a [`Replayed`]: what `replay(full log)` and
/// `replay(snapshot + tail)` must agree on. (The nonce window is not in it: a never-compacted
/// log has none to agree on.)
pub type Canonical = (BTreeMap<String, Bound>, u64, Vec<Rejection>);

impl Replayed {
    /// Fold one record. First-register-wins: a `Registered` for an id already bound to a
    /// *different* key is ignored (the live path never writes one; this guards a damaged log).
    pub fn apply(&mut self, rec: Record) {
        match rec {
            Record::Registered {
                id,
                addr,
                version,
                key,
            } => match self.instances.get_mut(&id) {
                Some(b) if b.key != key => {}
                Some(b) => {
                    b.addr = addr;
                    b.version = version;
                }
                None => {
                    self.instances.insert(id, Bound { addr, version, key });
                }
            },
            Record::ConfigPushed { version } => {
                self.config_version = self.config_version.max(version);
            }
            Record::EnrollmentRejected(r) => {
                self.rejections.push_back(r);
                while self.rejections.len() > AUDIT_CAP {
                    self.rejections.pop_front();
                }
            }
            // Only meaningful as the first record, where `open_with_fs` consumes it.
            Record::Epoch { .. } => {}
        }
    }

    pub fn canonical(&self) -> Canonical {
        (
            self.instances.clone(),
            self.config_version,
            self.rejections.iter().cloned().collect(),
        )
    }

    fn from_snapshot(snap: Snapshot) -> Replayed {
        let mut rejections: VecDeque<Rejection> = snap.rejections.into();
        while rejections.len() > AUDIT_CAP {
            rejections.pop_front();
        }
        Replayed {
            instances: snap.instances,
            config_version: snap.config_version,
            rejections,
            nonces: snap.nonces,
            ..Replayed::default()
        }
    }
}

// ── sink ───────────────────────────────────────────────────────────────────────────────────

/// Where records go. The seam that lets a test inject a failing disk.
pub trait RecordSink: Send {
    fn append(&mut self, payload: &[u8]) -> io::Result<()>;

    /// Fold the whole state into a snapshot and truncate the log (see [`crate::compaction`]).
    /// `plan` is the step order to execute. The default refuses: a sink that cannot compact is
    /// still a valid sink.
    fn compact(&mut self, _snapshot: &Snapshot, _plan: &[Step]) -> io::Result<CompactionReport> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this sink cannot compact",
        ))
    }
}

pub struct LogSink {
    log: Log<FileWriter>,
    poisoned: bool,
    path: PathBuf,
    dir: PathBuf,
    /// The epoch of the log on disk: 0 until the first compaction.
    epoch: u64,
    fs: Box<dyn Fs>,
}

impl LogSink {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn log_bytes(&self) -> u64 {
        self.log.current_len()
    }
}

impl RecordSink for LogSink {
    fn append(&mut self, payload: &[u8]) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::other(
                "log poisoned by an earlier failure; restart to recover",
            ));
        }
        self.log.append(payload).map(|_| ()).map_err(|e| {
            self.poisoned = true;
            io::Error::other(e.to_string())
        })
    }

    fn compact(&mut self, snapshot: &Snapshot, plan: &[Step]) -> io::Result<CompactionReport> {
        if self.poisoned {
            return Err(io::Error::other(
                "log poisoned by an earlier failure; restart to recover",
            ));
        }
        let snap_bytes = frame(&snapshot.encode(self.epoch));
        let header = frame(&Record::Epoch { n: self.epoch + 1 }.encode());
        let before = self.log.current_len();
        if let Err(e) = compaction::run(&mut *self.fs, &self.dir, plan, &snap_bytes, &header) {
            self.settle_after_failed_compaction();
            return Err(e);
        }
        self.poisoned = true;
        let (log, _) = open_log(&self.path)?;
        self.log = log;
        self.epoch = self.read_epoch()?;
        self.poisoned = false;
        Ok(CompactionReport {
            records_folded: 0,
            log_bytes_before: before,
            log_bytes_after: self.log.current_len(),
            snapshot_bytes: snap_bytes.len() as u64,
        })
    }
}

impl LogSink {
    /// A compaction stopped part-way (a transient ENOSPC on the snapshot tmp file, say). Appending
    /// is safe again unless the disk now says otherwise: a snapshot that already covers this log
    /// would silently drop a record appended to it, and a log that was replaced under our open
    /// handle would swallow it. In either case (or if the disk cannot be read) stay poisoned and
    /// let a restart recover; otherwise carry on — nothing the failed attempt left behind is
    /// visible to `store::open`.
    fn settle_after_failed_compaction(&mut self) {
        let snapshot_covers_log = match read_snapshot(&self.dir) {
            Ok(Some((covers, _))) => covers >= self.epoch,
            Ok(None) => false,
            Err(_) => true,
        };
        let log_replaced = match open_log(&self.path) {
            Ok((log, _)) => {
                self.log = log;
                !matches!(self.read_epoch(), Ok(e) if e == self.epoch)
            }
            Err(_) => true,
        };
        self.poisoned = snapshot_covers_log || log_replaced;
    }

    /// The epoch the log on disk declares (what a mutated plan actually produced).
    fn read_epoch(&self) -> io::Result<u64> {
        match self.log.read_from(0).next() {
            Some(Ok(first)) => match Record::decode(&first.payload) {
                Ok(Record::Epoch { n }) => Ok(n),
                _ => Ok(0),
            },
            Some(Err(e)) => Err(invalid(format!("{e:?}"))),
            None => Ok(0),
        }
    }
}

fn open_log(path: &Path) -> io::Result<(Log<FileWriter>, RecoveryReport)> {
    let writer = FileWriter::open_append(path)?;
    Log::open(writer, path.to_path_buf(), FsyncPolicy::EveryRecord)
}

fn read_snapshot(dir: &Path) -> io::Result<Option<(u64, Snapshot)>> {
    let bytes = match std::fs::read(dir.join(SNAPSHOT_FILE)) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    // A snapshot is renamed into place whole, so a bad one is damage, not a torn write: refuse to
    // start rather than quietly forget every identity binding.
    let payload = unframe(&bytes)?;
    Snapshot::decode(&payload)
        .map(Some)
        .map_err(|e| invalid(format!("snapshot: {}", e.0)))
}

/// Open (creating if absent) the state in `dir`: load the snapshot, truncate any torn log tail,
/// replay the records the snapshot does not already cover, and finish an interrupted compaction.
pub fn open(dir: &Path) -> io::Result<(Replayed, LogSink)> {
    open_with_fs(dir, Box::new(RealFs))
}

/// [`open`], with the filesystem operations compaction uses routed through `fs` (the seam the
/// crash checker uses).
pub fn open_with_fs(dir: &Path, mut fs: Box<dyn Fs>) -> io::Result<(Replayed, LogSink)> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(LOG_FILE);
    let snapshot = read_snapshot(dir)?;
    let (log, recovery) = open_log(&path)?;

    let mut records = Vec::new();
    for item in log.read_from(0) {
        let rec = item.map_err(|e| invalid(format!("{e:?}")))?;
        // A CRC-valid record we cannot decode means the log is from a different version, not
        // that it is torn. Refuse to start rather than silently forget an identity binding.
        let rec = Record::decode(&rec.payload).map_err(|e| invalid(e.0))?;
        records.push(rec);
    }
    let mut body = records.into_iter().peekable();
    let log_epoch = match body.peek() {
        Some(Record::Epoch { n }) => {
            let n = *n;
            if n == 0 {
                return Err(invalid("log declares epoch 0"));
            }
            body.next();
            n
        }
        _ => 0,
    };

    let mut finish = None;
    let mut replayed = match snapshot {
        None if log_epoch == 0 => Replayed::default(),
        None => {
            return Err(invalid(format!(
                "log is epoch {log_epoch} but there is no snapshot: it was lost"
            )))
        }
        Some((covers, snap)) if log_epoch == covers => {
            // Killed after the snapshot became durable and before the log was replaced: every
            // record in the log is already in the snapshot. Do not apply them twice.
            finish = Some(covers + 1);
            body.by_ref().for_each(drop);
            Replayed::from_snapshot(snap)
        }
        Some((covers, snap)) if log_epoch == covers + 1 => Replayed::from_snapshot(snap),
        Some((covers, _)) => {
            return Err(invalid(format!(
                "snapshot covers epoch {covers} but the log is epoch {log_epoch}"
            )))
        }
    };
    for rec in body {
        if matches!(rec, Record::Epoch { .. }) {
            return Err(invalid("epoch record in the middle of the log"));
        }
        replayed.apply(rec);
        replayed.tail_records += 1;
    }
    replayed.recovery = recovery;

    let (log, epoch) = match finish {
        Some(next) => {
            drop(log);
            let header = frame(&Record::Epoch { n: next }.encode());
            compaction::run(&mut *fs, dir, compaction::FINISH_PLAN, &[], &header)?;
            (open_log(&path)?.0, next)
        }
        None => (log, log_epoch),
    };
    Ok((
        replayed,
        LogSink {
            log,
            poisoned: false,
            path,
            dir: dir.to_path_buf(),
            epoch,
            fs,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip() {
        for rec in [
            Record::Registered {
                id: "proxy-1".into(),
                addr: "http://10.0.0.1:9".into(),
                version: "0.1.0".into(),
                key: [7; 32],
            },
            Record::ConfigPushed { version: u64::MAX },
            Record::EnrollmentRejected(Rejection {
                key: [9; 32],
                id: "intruder".into(),
                at: -5,
            }),
            Record::Epoch { n: 3 },
        ] {
            assert_eq!(Record::decode(&rec.encode()), Ok(rec));
        }
    }

    #[test]
    fn damaged_records_fail_to_decode_instead_of_decoding_to_something_else() {
        let good = Record::Registered {
            id: "a".into(),
            addr: "b".into(),
            version: "c".into(),
            key: [1; 32],
        }
        .encode();
        for n in 0..good.len() {
            assert!(Record::decode(&good[..n]).is_err(), "prefix of {n} bytes");
        }
        let mut long = good.clone();
        long.push(0);
        assert!(Record::decode(&long).is_err());
        assert!(Record::decode(&[9, 0, 0]).is_err());
    }

    #[test]
    fn replay_is_first_register_wins() {
        let mut r = Replayed::default();
        let reg = |key: u8, addr: &str| Record::Registered {
            id: "i".into(),
            addr: addr.into(),
            version: "v".into(),
            key: [key; 32],
        };
        r.apply(reg(1, "first"));
        r.apply(reg(2, "hijack"));
        r.apply(reg(1, "moved"));
        assert_eq!(r.instances["i"].key, [1; 32]);
        assert_eq!(r.instances["i"].addr, "moved");
    }

    #[test]
    fn the_audit_trail_keeps_only_the_most_recent_rejections() {
        let mut r = Replayed::default();
        for at in 0..(AUDIT_CAP as i64 + 10) {
            r.apply(Record::EnrollmentRejected(Rejection {
                key: [1; 32],
                id: "x".into(),
                at,
            }));
        }
        assert_eq!(r.rejections.len(), AUDIT_CAP);
        assert_eq!(r.rejections.front().map(|x| x.at), Some(10));
    }

    #[test]
    fn a_snapshot_round_trips_and_damage_is_detected() {
        let mut snap = Snapshot {
            config_version: 41,
            rejections: vec![Rejection {
                key: [3; 32],
                id: "r".into(),
                at: 9,
            }],
            nonces: vec![NonceEntry {
                key: [4; 32],
                nonce: "nonce-0001".into(),
                ts: 77,
            }],
            ..Snapshot::default()
        };
        snap.instances.insert(
            "i1".into(),
            Bound {
                addr: "a".into(),
                version: "v".into(),
                key: [5; 32],
            },
        );
        let payload = snap.encode(7);
        assert_eq!(Snapshot::decode(&payload).unwrap(), (7, snap));
        for n in 0..payload.len() {
            assert!(Snapshot::decode(&payload[..n]).is_err(), "prefix {n}");
        }
        let framed = frame(&payload);
        let mut flipped = framed.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 1;
        assert!(unframe(&flipped).is_err(), "a flipped bit fails the CRC");
        assert_eq!(unframe(&framed).unwrap(), payload);
    }
}
