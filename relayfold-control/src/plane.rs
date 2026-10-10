//! The control plane proper, synchronous and I/O-free except for its [`RecordSink`]: identity
//! binding, heartbeat expiry, signed-config preparation. `app.rs` wraps it in HTTP; the
//! conformance checker drives it directly with an injected clock.
//!
//! Every mutation is one critical section — *authenticate, check, append to the log, then apply
//! in memory* — so a storage error returns [`ControlError::Storage`] with the in-memory state
//! untouched, and two callers racing to register the same id cannot both win.

use crate::auth::{key_to_z32, AuthError, Key, SignedCall, Signer, Verifier};
use crate::clock::Clock;
use crate::dto::{ConfigPushRequest, HeartbeatRequest, InstanceView, RegisterRequest};
use crate::liveness::{Liveness, LivenessEvent};
use crate::stages::Stages;
use crate::store::{self, Record, RecordSink, Replayed};
use sangdb_log::RecoveryReport;
use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone)]
pub struct ControlConfig {
    /// Dialkey of the control plane. Its derived key signs every pushed config; agents verify
    /// with [`crate::auth::public_key_of`]. A secret: never logged (see the `Debug` impl).
    pub control_dialkey: String,
    /// Public keys allowed to call `/config/push`. Empty = nobody may push.
    pub admin_keys: Vec<Key>,
    /// An instance last seen at `t` is healthy while `now - t < ttl_secs`.
    pub ttl_secs: i64,
    /// Accepted distance between a request's timestamp and the server clock.
    pub skew_secs: i64,
    /// Replay window size; a full window rejects rather than evicts.
    pub nonce_capacity: usize,
}

impl ControlConfig {
    pub fn new(control_dialkey: impl Into<String>, admin_keys: Vec<Key>) -> Self {
        ControlConfig {
            control_dialkey: control_dialkey.into(),
            admin_keys,
            ttl_secs: 30,
            skew_secs: 30,
            nonce_capacity: 4096,
        }
    }
}

impl std::fmt::Debug for ControlConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlConfig")
            .field("control_dialkey", &"<redacted>")
            .field("admin_keys", &self.admin_keys.len())
            .field("ttl_secs", &self.ttl_secs)
            .field("skew_secs", &self.skew_secs)
            .field("nonce_capacity", &self.nonce_capacity)
            .finish()
    }
}

#[derive(Debug)]
pub enum ControlError {
    /// No/invalid/stale/replayed credentials. HTTP 401 (429 for [`AuthError::WindowFull`]).
    Unauthenticated(AuthError),
    /// Authenticated, but not allowed to do this. HTTP 403.
    Forbidden(&'static str),
    BadRequest(&'static str),
    NotFound,
    /// The log refused the write; nothing was applied. HTTP 500.
    Storage(io::Error),
}

impl From<AuthError> for ControlError {
    fn from(e: AuthError) -> Self {
        ControlError::Unauthenticated(e)
    }
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControlError::Unauthenticated(e) => write!(f, "unauthenticated: {e}"),
            ControlError::Forbidden(why) => write!(f, "forbidden: {why}"),
            ControlError::BadRequest(why) => write!(f, "bad request: {why}"),
            ControlError::NotFound => f.write_str("no such instance"),
            ControlError::Storage(e) => write!(f, "storage error: {e}"),
        }
    }
}

impl std::error::Error for ControlError {}

struct Entry {
    addr: String,
    version: String,
    key: Key,
    liveness: Liveness,
    /// What the instance last reported as its applied config version (free-form).
    reported_config: String,
}

/// One signed config, addressed to one instance.
#[derive(Clone, Debug)]
pub struct Delivery {
    pub id: String,
    pub addr: String,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct PreparedPush {
    /// The version every delivery carries. Unchanged (no version consumed) when `deliveries` is
    /// empty.
    pub config_version: u64,
    pub deliveries: Vec<Delivery>,
}

pub struct ControlPlane {
    stages: Stages,
    clock: Arc<dyn Clock>,
    ttl_secs: i64,
    verifier: Verifier,
    instances: BTreeMap<String, Entry>,
    config_version: u64,
    admin_keys: Vec<Key>,
    control: Signer,
    sink: Box<dyn RecordSink>,
    recovery: RecoveryReport,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.chars().all(|c| !c.is_control())
}

impl ControlPlane {
    /// Open the durable log in `dir` and rebuild state from it.
    pub fn open(
        dir: &Path,
        cfg: ControlConfig,
        clock: Arc<dyn Clock>,
        stages: Stages,
    ) -> io::Result<Self> {
        let (replayed, sink) = store::open(dir)?;
        Ok(Self::from_parts(
            cfg,
            clock,
            stages,
            replayed,
            Box::new(sink),
        ))
    }

    pub fn from_parts(
        cfg: ControlConfig,
        clock: Arc<dyn Clock>,
        stages: Stages,
        replayed: Replayed,
        sink: Box<dyn RecordSink>,
    ) -> Self {
        let started_at = clock.now_unix();
        let instances = replayed
            .instances
            .into_iter()
            .map(|(id, b)| {
                (
                    id,
                    Entry {
                        addr: b.addr,
                        version: b.version,
                        key: b.key,
                        liveness: Liveness::UNSEEN,
                        reported_config: String::new(),
                    },
                )
            })
            .collect();
        ControlPlane {
            stages,
            ttl_secs: cfg.ttl_secs,
            verifier: Verifier::new(stages, cfg.skew_secs, cfg.nonce_capacity, started_at),
            clock,
            instances,
            config_version: replayed.config_version,
            admin_keys: cfg.admin_keys,
            control: Signer::from_dialkey(&cfg.control_dialkey),
            sink,
            recovery: replayed.recovery,
        }
    }

    pub fn config_version(&self) -> u64 {
        self.config_version
    }

    /// What opening the log found: `bytes_dropped > 0` means a torn tail was truncated.
    pub fn recovery(&self) -> RecoveryReport {
        self.recovery
    }

    pub fn control_public_key(&self) -> Key {
        self.control.public_key()
    }

    /// The key an id is bound to, if registered.
    pub fn bound_key(&self, id: &str) -> Option<Key> {
        self.instances.get(id).map(|e| e.key)
    }

    fn persist(&mut self, rec: &Record) -> Result<(), ControlError> {
        self.sink
            .append(&rec.encode())
            .map_err(ControlError::Storage)
    }

    fn view(&self, id: &str, e: &Entry, now: i64) -> InstanceView {
        let live =
            (self.stages.liveness_fold)(e.liveness, LivenessEvent::Expired { now }, self.ttl_secs);
        InstanceView {
            id: id.to_string(),
            addr: e.addr.clone(),
            version: e.version.clone(),
            healthy: live.is_healthy(),
            last_heartbeat_unix: live.last_seen.unwrap_or(0),
            config_version: e.reported_config.clone(),
            public_key: key_to_z32(&e.key),
        }
    }

    /// Bind `req.id` to the signer's key (first register wins) or refresh an existing binding.
    pub fn register(
        &mut self,
        call: &SignedCall<'_>,
        req: RegisterRequest,
    ) -> Result<InstanceView, ControlError> {
        let caller = self.verifier.authenticate(&*self.clock, call)?;
        let now = self.clock.now_unix();
        if !valid_id(&req.id) {
            return Err(ControlError::BadRequest(
                "id must be 1..=128 non-control chars",
            ));
        }
        if req.addr.len() > 512 || req.version.len() > 128 {
            return Err(ControlError::BadRequest("addr/version too long"));
        }
        let needs_write = match self.instances.get(&req.id) {
            Some(e) => {
                if !(self.stages.may_reregister)(&e.key, &caller) {
                    return Err(ControlError::Forbidden("id is bound to a different key"));
                }
                e.addr != req.addr || e.version != req.version || e.key != caller
            }
            None => true,
        };
        if needs_write {
            self.persist(&Record::Registered {
                id: req.id.clone(),
                addr: req.addr.clone(),
                version: req.version.clone(),
                key: caller,
            })?;
        }
        let fold = self.stages.liveness_fold;
        let ttl = self.ttl_secs;
        let entry = self
            .instances
            .entry(req.id.clone())
            .or_insert_with(|| Entry {
                addr: String::new(),
                version: String::new(),
                key: caller,
                liveness: Liveness::UNSEEN,
                reported_config: String::new(),
            });
        entry.addr = req.addr;
        entry.version = req.version;
        entry.key = caller;
        entry.liveness = fold(entry.liveness, LivenessEvent::Registered { at: now }, ttl);
        let entry = &self.instances[&req.id];
        Ok(self.view(&req.id, entry, now))
    }

    /// Record a heartbeat. Only the key bound at register may heartbeat an id.
    pub fn heartbeat(
        &mut self,
        call: &SignedCall<'_>,
        req: HeartbeatRequest,
    ) -> Result<InstanceView, ControlError> {
        let caller = self.verifier.authenticate(&*self.clock, call)?;
        let now = self.clock.now_unix();
        let fold = self.stages.liveness_fold;
        let ttl = self.ttl_secs;
        let may = self.stages.may_heartbeat;
        let entry = self
            .instances
            .get_mut(&req.id)
            .ok_or(ControlError::NotFound)?;
        if !may(&entry.key, &caller) {
            return Err(ControlError::Forbidden("id is bound to a different key"));
        }
        entry.liveness = fold(
            entry.liveness,
            LivenessEvent::Heartbeat {
                at: now,
                healthy: req.healthy,
            },
            ttl,
        );
        entry.reported_config = req.config_version;
        let entry = &self.instances[&req.id];
        Ok(self.view(&req.id, entry, now))
    }

    /// Every registered instance; `healthy` is expired against the clock at read time.
    pub fn list(&self) -> Vec<InstanceView> {
        let now = self.clock.now_unix();
        self.instances
            .iter()
            .map(|(id, e)| self.view(id, e, now))
            .collect()
    }

    /// Authenticate an admin, consume the next `config_version` (durably), and sign one config
    /// per target. Delivery is the caller's job and happens outside this critical section.
    pub fn push_config(
        &mut self,
        call: &SignedCall<'_>,
        req: &ConfigPushRequest,
    ) -> Result<PreparedPush, ControlError> {
        let caller = self.verifier.authenticate(&*self.clock, call)?;
        if !self.admin_keys.contains(&caller) {
            return Err(ControlError::Forbidden("key is not an admin key"));
        }
        let targets: Vec<(String, String)> = self
            .instances
            .iter()
            .filter(|(id, _)| req.target == "*" || **id == req.target)
            .map(|(id, e)| (id.clone(), e.addr.clone()))
            .collect();
        if targets.is_empty() {
            return Ok(PreparedPush {
                config_version: self.config_version,
                deliveries: Vec::new(),
            });
        }
        let version = self
            .config_version
            .checked_add(1)
            .ok_or(ControlError::BadRequest("config_version exhausted"))?;
        self.persist(&Record::ConfigPushed { version })?;
        self.config_version = version;
        let deliveries = targets
            .into_iter()
            .map(|(id, addr)| {
                let body = crate::config::sign_config(&self.control, &id, version, &req.config);
                Delivery { id, addr, body }
            })
            .collect();
        Ok(PreparedPush {
            config_version: version,
            deliveries,
        })
    }
}
