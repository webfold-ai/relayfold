//! Durable state: an append-only log on `sangdb-log` (CRC32-framed records, `fsync` after every
//! record, torn tail truncated to the longest valid prefix on open).
//!
//! # What is logged — and what deliberately is not
//!
//! * `Registered` — an id's address/version **and the public key it is bound to**. Written when an
//!   id first registers, or when a re-register by the *bound* key changes addr/version.
//! * `ConfigPushed` — the control-plane `config_version`, written **before** delivery so the
//!   counter is strictly increasing across restarts.
//! * Heartbeats are **not** logged. They are liveness, not identity: logging one per instance per
//!   interval would make the log grow without bound (this crate does not compact), and a restarted
//!   control plane is right to call every instance not-healthy until it is heard from again —
//!   replay yields [`Liveness::UNSEEN`].
//!
//! # Failure semantics
//!
//! [`RecordSink::append`] returns an error; the control plane propagates it (HTTP 500) and does
//! **not** apply the change in memory. After any append error [`LogSink`] is *poisoned* and
//! refuses further appends: a failed `write` can leave a partial frame in the file, and the next
//! record would land behind it, where recovery would later truncate it away. Restart truncates the
//! torn tail and clears the condition.

use crate::auth::Key;
use sangdb_log::{FileWriter, FsyncPolicy, Log, RecoveryReport};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

pub const LOG_FILE: &str = "control.log";

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
}

const TAG_REGISTERED: u8 = 1;
const TAG_CONFIG_PUSHED: u8 = 2;

#[derive(Debug, PartialEq, Eq)]
pub struct DecodeError(pub &'static str);

fn put(out: &mut Vec<u8>, field: &[u8]) {
    out.extend_from_slice(&(field.len() as u32).to_be_bytes());
    out.extend_from_slice(field);
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
                put(&mut out, &version.to_be_bytes());
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
                key: c
                    .field()?
                    .try_into()
                    .map_err(|_| DecodeError("key is not 32 bytes"))?,
            },
            TAG_CONFIG_PUSHED => Record::ConfigPushed {
                version: u64::from_be_bytes(
                    c.field()?
                        .try_into()
                        .map_err(|_| DecodeError("version is not 8 bytes"))?,
                ),
            },
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

/// State rebuilt from the log on open.
#[derive(Debug, Default)]
pub struct Replayed {
    pub instances: BTreeMap<String, Bound>,
    pub config_version: u64,
    pub recovery: RecoveryReport,
}

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
        }
    }
}

/// Where records go. The seam that lets a test inject a failing disk.
pub trait RecordSink: Send {
    fn append(&mut self, payload: &[u8]) -> io::Result<()>;
}

pub struct LogSink {
    log: Log<FileWriter>,
    poisoned: bool,
    path: PathBuf,
}

impl LogSink {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl RecordSink for LogSink {
    fn append(&mut self, payload: &[u8]) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::other(
                "log poisoned by an earlier append failure; restart to truncate the torn tail",
            ));
        }
        self.log.append(payload).map(|_| ()).map_err(|e| {
            self.poisoned = true;
            io::Error::other(e.to_string())
        })
    }
}

/// Open (creating if absent) the log in `dir`, truncate any torn tail, and replay it.
pub fn open(dir: &Path) -> io::Result<(Replayed, LogSink)> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(LOG_FILE);
    let writer = FileWriter::open_append(&path)?;
    let (log, recovery) = Log::open(writer, path.clone(), FsyncPolicy::EveryRecord)?;
    let mut replayed = Replayed {
        recovery,
        ..Replayed::default()
    };
    for item in log.read_from(0) {
        let rec = item.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")))?;
        // A CRC-valid record we cannot decode means the log is from a different version, not
        // that it is torn. Refuse to start rather than silently forget an identity binding.
        let rec = Record::decode(&rec.payload)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.0))?;
        replayed.apply(rec);
    }
    Ok((
        replayed,
        LogSink {
            log,
            poisoned: false,
            path,
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
}
