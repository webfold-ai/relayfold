//! The auth / replay / expiry / config-verification checker.
//!
//! [`check_all`] is a function **over the implementation**: it builds a real [`ControlPlane`] from
//! the [`Stages`] it is given, drives it with real signed requests under an injected
//! [`ManualClock`], and reports every [`Property`] the implementation violates. The same function
//! judges `Stages::REAL` (expected: no violations) and each mutant — `Stages { signature_ok:
//! |..| true, ..Stages::REAL }` and friends (see `tests/negative_controls.rs`) — so a mutant is
//! rejected for a property the checker actually probes, not because a hand-written broken
//! function asserts its own brokenness.
//!
//! Every property has a **positive** probe too (a valid signed request is accepted, an instance is
//! healthy inside its TTL, a valid config verifies), so "reject everything" and "never healthy"
//! mutants fail the checker as well. The replay probe resends a byte-identical request.
//!
//! Scope: the checker drives [`ControlPlane`], the layer that owns every decision. The HTTP layer
//! (header extraction, status mapping) is exercised separately over real sockets in
//! `tests/http.rs`.
//!
//! Enrollment, the signed read path and compaction are probed the same way. The compaction
//! scenarios run two planes through one history (one compacts mid-way) and compare them after a
//! restart, replay a pre-compaction request, and — the crash scenario — stop the compaction after
//! every prefix of its step plan, once as a process kill and once as the worst-case power loss
//! (`compaction::LossyFs`), then require the directory to open to exactly the pre-compaction
//! state and to keep accepting durable writes.

use crate::auth::{key_to_z32, public_key_of, AuthHeaders, Key, RawAuth, SignedCall, Signer};
use crate::clock::{Clock, ManualClock};
use crate::compaction::LossyFs;
use crate::config::{sign_config, verify_config_with, SignedConfig};
use crate::dto::{ConfigPushRequest, HeartbeatRequest, InstanceView, RegisterRequest};
use crate::json::Json;
use crate::plane::{ControlConfig, ControlError, ControlPlane, PreparedPush};
use crate::stages::Stages;
use crate::store::{self, RecordSink};
use std::path::Path;
use std::sync::Arc;
use transport::signed_dns::Keypair;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Property {
    ValidRequestAccepted,
    UnsignedRejected,
    ForgedSignatureRejected,
    TamperedBodyRejected,
    ReplayRejected,
    StaleTimestampRejected,
    FutureTimestampRejected,
    PreBootRequestRejected,
    OwnerHeartbeatAccepted,
    WrongKeyHeartbeatRejected,
    RebindRejected,
    HealthyInsideTtl,
    ExpiresAfterTtl,
    HeartbeatRevives,
    UnhealthyReportHonored,
    NonceWindowBounded,
    LiveNonceNeverEvicted,
    NonAdminPushRejected,
    ConfigValidAccepted,
    ConfigUnsignedRejected,
    ConfigForgedSignatureRejected,
    ConfigWrongKeyRejected,
    ConfigTamperedRejected,
    ConfigWrongTargetRejected,
    ConfigReplayRejected,
    ConfigVersionMonotonic,
    IdentitySurvivesRestart,
    ConfigVersionSurvivesRestart,
    StorageErrorNotApplied,
    PeerIdIsPublicKey,
    IdentityIsNotTheSecretSeed,
    PublicKeyFollowsDerivation,
    IdentityDistinctPerDialkey,
    IdentityMatchesGolden,
    // Enrollment (gap 1) and the signed read path (gap 2).
    UnenrolledRegisterRejected,
    EmptyAllowlistIsClosed,
    RejectionIsLogged,
    ListUnsignedRejected,
    ListForgedRejected,
    ListUnenrolledRejected,
    ListEnrolledAccepted,
    ListReplayRejected,
    // Compaction (gap 3).
    CompactionPreservesState,
    CompactionShrinksLog,
    CompactionKeepsIdentity,
    CompactionKeepsConfigVersion,
    CompactionKeepsAudit,
    CompactionKeepsReplayProtection,
    CompactionCrashSafe,
}

#[derive(Clone, Debug)]
pub struct Violation {
    pub property: Property,
    pub detail: String,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.property, self.detail)
    }
}

impl std::error::Error for Violation {}

fn v(property: Property, detail: impl Into<String>) -> Violation {
    Violation {
        property,
        detail: detail.into(),
    }
}

// Test-only dialkeys: labelled literals, never real credentials.
const DK_A: &str = "conformance-instance-a";
const DK_B: &str = "conformance-instance-b";
const DK_ADMIN: &str = "conformance-admin";
const DK_CONTROL: &str = "conformance-control";
/// Validly signs everything it sends, but is on no allowlist.
const DK_OUTSIDER: &str = "conformance-outsider";

const START: i64 = 1_000_000;
const TTL: i64 = 30;
const SKEW: i64 = 30;

struct Rig {
    plane: ControlPlane,
    clock: ManualClock,
    dir: std::path::PathBuf,
    stages: Stages,
    cfg: ControlConfig,
}

fn base_cfg() -> ControlConfig {
    let mut cfg = ControlConfig::new(DK_CONTROL, vec![public_key_of(DK_ADMIN)])
        .with_enrolled(vec![public_key_of(DK_A), public_key_of(DK_B)]);
    cfg.ttl_secs = TTL;
    cfg.skew_secs = SKEW;
    // Compaction is driven explicitly by the scenarios that probe it.
    cfg.compact_after = None;
    cfg
}

impl Rig {
    fn new(stages: Stages, dir: &Path, name: &str, cfg: ControlConfig) -> Result<Rig, Violation> {
        let dir = dir.join(name);
        let clock = ManualClock::new(START);
        let plane = open_plane(&dir, &cfg, &clock, stages)?;
        Ok(Rig {
            plane,
            clock,
            dir,
            stages,
            cfg,
        })
    }

    /// Drop the plane and open a new one on the same directory (a process restart).
    fn restart(&mut self) -> Result<(), Violation> {
        let clock = self.clock.clone();
        // Replace first so the old log handle is dropped before the new one opens.
        let placeholder = ControlPlane::from_parts(
            self.cfg.clone(),
            Arc::new(clock.clone()),
            self.stages,
            store::Replayed::default(),
            Box::new(FailingSink),
        );
        drop(std::mem::replace(&mut self.plane, placeholder));
        self.plane = open_plane(&self.dir, &self.cfg, &clock, self.stages)?;
        Ok(())
    }

    fn now(&self) -> i64 {
        self.clock.now_unix()
    }

    fn sign(&self, s: &Signer, path: &str, body: &[u8], ts: i64) -> AuthHeaders {
        s.sign_now("POST", path, body, ts)
    }

    fn register_with(
        &mut self,
        auth: &RawAuth,
        body: &[u8],
        id: &str,
    ) -> Result<InstanceView, ControlError> {
        let call = SignedCall {
            method: "POST",
            path: "/register",
            body,
            auth,
        };
        self.plane.register(&call, register_req(id))
    }

    fn register(&mut self, s: &Signer, id: &str) -> Result<InstanceView, ControlError> {
        let body = Json(register_req(id)).encode();
        let auth = self.sign(s, "/register", &body, self.now()).to_raw();
        self.register_with(&auth, &body, id)
    }

    /// Register `id` at `addr` (a re-register by the bound key changes what the log holds).
    fn register_at(
        &mut self,
        s: &Signer,
        id: &str,
        addr: &str,
    ) -> Result<InstanceView, ControlError> {
        let req = RegisterRequest {
            addr: addr.to_string(),
            ..register_req(id)
        };
        let body = Json(req.clone()).encode();
        let auth = self.sign(s, "/register", &body, self.now()).to_raw();
        let call = SignedCall {
            method: "POST",
            path: "/register",
            body: &body,
            auth: &auth,
        };
        self.plane.register(&call, req)
    }

    /// `GET /instances` as `s` would send it.
    fn list_signed_as(&mut self, s: &Signer) -> (RawAuth, Result<Vec<InstanceView>, ControlError>) {
        let auth = s.sign_now("GET", "/instances", b"", self.now()).to_raw();
        let res = self.list_with(&auth);
        (auth, res)
    }

    fn list_with(&mut self, auth: &RawAuth) -> Result<Vec<InstanceView>, ControlError> {
        let call = SignedCall {
            method: "GET",
            path: "/instances",
            body: b"",
            auth,
        };
        self.plane.list_signed(&call)
    }

    fn log_len(&self) -> u64 {
        std::fs::metadata(self.dir.join(store::LOG_FILE))
            .map(|m| m.len())
            .unwrap_or(0)
    }

    fn heartbeat(
        &mut self,
        s: &Signer,
        id: &str,
        healthy: bool,
    ) -> Result<InstanceView, ControlError> {
        let req = HeartbeatRequest {
            id: id.to_string(),
            healthy,
            config_version: "v0".into(),
        };
        let body = Json(req.clone()).encode();
        let auth = self.sign(s, "/heartbeat", &body, self.now()).to_raw();
        let call = SignedCall {
            method: "POST",
            path: "/heartbeat",
            body: &body,
            auth: &auth,
        };
        self.plane.heartbeat(&call, req)
    }

    fn push(
        &mut self,
        s: &Signer,
        target: &str,
        config: &str,
    ) -> Result<PreparedPush, ControlError> {
        let req = ConfigPushRequest {
            target: target.to_string(),
            config: config.to_string(),
        };
        let body = Json(req.clone()).encode();
        let auth = self.sign(s, "/config/push", &body, self.now()).to_raw();
        let call = SignedCall {
            method: "POST",
            path: "/config/push",
            body: &body,
            auth: &auth,
        };
        self.plane.push_config(&call, &req)
    }

    fn healthy(&self, id: &str) -> Option<bool> {
        self.plane
            .list()
            .into_iter()
            .find(|i| i.id == id)
            .map(|i| i.healthy)
    }
}

fn open_plane(
    dir: &Path,
    cfg: &ControlConfig,
    clock: &ManualClock,
    stages: Stages,
) -> Result<ControlPlane, Violation> {
    ControlPlane::open(dir, cfg.clone(), Arc::new(clock.clone()), stages).map_err(|e| {
        v(
            Property::IdentitySurvivesRestart,
            format!("could not open the control plane at {}: {e}", dir.display()),
        )
    })
}

fn register_req(id: &str) -> RegisterRequest {
    RegisterRequest {
        id: id.to_string(),
        addr: "http://127.0.0.1:1".into(),
        version: "0.1.0".into(),
    }
}

/// A sink whose disk is always full.
struct FailingSink;

impl RecordSink for FailingSink {
    fn append(&mut self, _payload: &[u8]) -> std::io::Result<()> {
        Err(std::io::Error::other("no space left on device (injected)"))
    }
}

/// Collects violations. A *probe* records a violation and lets the scenario continue, so one
/// wrong decision point cannot hide behind another probe that fails first; a failed *setup* step
/// (something the scenario needs in order to probe further) records and ends the scenario.
#[derive(Default)]
struct Report(Vec<Violation>);

impl Report {
    fn fail(&mut self, property: Property, detail: impl Into<String>) {
        self.0.push(v(property, detail));
    }

    /// A setup step that must succeed.
    fn setup<T>(
        &mut self,
        property: Property,
        what: &str,
        r: Result<T, ControlError>,
    ) -> Option<T> {
        match r {
            Ok(t) => Some(t),
            Err(e) => {
                self.fail(property, format!("{what} was rejected: {e}"));
                None
            }
        }
    }

    /// Probe: must be refused, for any reason.
    fn rejected<T>(&mut self, property: Property, what: &str, r: Result<T, ControlError>) {
        if r.is_ok() {
            self.fail(property, format!("{what} was accepted"));
        }
    }

    /// Probe: must be refused as *unauthenticated* (the 401 class).
    fn unauthenticated<T>(&mut self, property: Property, what: &str, r: Result<T, ControlError>) {
        match r {
            Err(ControlError::Unauthenticated(_)) => {}
            Err(other) => self.fail(
                property,
                format!("{what} failed, but not as unauthenticated: {other}"),
            ),
            Ok(_) => self.fail(property, format!("{what} was accepted")),
        }
    }

    fn ensure(&mut self, ok: bool, property: Property, detail: impl Into<String>) {
        if !ok {
            self.fail(property, detail);
        }
    }
}

fn rig(
    rep: &mut Report,
    stages: Stages,
    dir: &Path,
    name: &str,
    cfg: ControlConfig,
) -> Option<Rig> {
    match Rig::new(stages, dir, name, cfg) {
        Ok(r) => Some(r),
        Err(e) => {
            rep.0.push(e);
            None
        }
    }
}

// ── scenarios ──────────────────────────────────────────────────────────────────────────────

type Scenario = fn(Stages, &Path, &mut Report);

fn valid_flow(stages: Stages, dir: &Path, rep: &mut Report) {
    let Some(mut r) = rig(rep, stages, dir, "valid_flow", base_cfg()) else {
        return;
    };
    let a = Signer::from_dialkey(DK_A);
    let reg = r.register(&a, "i1");
    let Some(view) = rep.setup(
        Property::ValidRequestAccepted,
        "a valid signed register",
        reg,
    ) else {
        return;
    };
    rep.ensure(
        view.healthy && view.public_key == key_to_z32(&a.public_key()),
        Property::ValidRequestAccepted,
        format!("register view wrong: {view:?}"),
    );
    let hb = r.heartbeat(&a, "i1", true);
    rep.setup(
        Property::OwnerHeartbeatAccepted,
        "the bound key's heartbeat",
        hb,
    );
}

fn unsigned(stages: Stages, dir: &Path, rep: &mut Report) {
    let Some(mut r) = rig(rep, stages, dir, "unsigned", base_cfg()) else {
        return;
    };
    let body = Json(register_req("i1")).encode();
    let res = r.register_with(&RawAuth::default(), &body, "i1");
    rep.unauthenticated(
        Property::UnsignedRejected,
        "register with no credentials",
        res,
    );
    rep.ensure(
        r.plane.list().is_empty(),
        Property::UnsignedRejected,
        "an unauthenticated register created an instance",
    );
}

fn forged_signature(stages: Stages, dir: &Path, rep: &mut Report) {
    let Some(mut r) = rig(rep, stages, dir, "forged", base_cfg()) else {
        return;
    };
    let (a, b) = (Signer::from_dialkey(DK_A), Signer::from_dialkey(DK_B));
    let body = Json(register_req("i1")).encode();
    // Claims to be A, but the signature is B's.
    let mut h = r.sign(&a, "/register", &body, r.now());
    h.signature = r.sign(&b, "/register", &body, r.now()).signature;
    h.nonce = "forged-nonce-0001".into();
    let res = r.register_with(&h.to_raw(), &body, "i1");
    rep.unauthenticated(
        Property::ForgedSignatureRejected,
        "register signed by a different key than it claims",
        res,
    );
    // Right key, garbage signature.
    let mut g = r.sign(&a, "/register", &body, r.now());
    g.signature = "00".repeat(64);
    g.nonce = "forged-nonce-0002".into();
    let res = r.register_with(&g.to_raw(), &body, "i1");
    rep.unauthenticated(
        Property::ForgedSignatureRejected,
        "register with a garbage signature",
        res,
    );
}

fn tampered_body(stages: Stages, dir: &Path, rep: &mut Report) {
    let Some(mut r) = rig(rep, stages, dir, "tampered", base_cfg()) else {
        return;
    };
    let a = Signer::from_dialkey(DK_A);
    let signed_body = Json(register_req("i1")).encode();
    let sent_body = Json(register_req("i2-attacker-chosen")).encode();
    let auth = r.sign(&a, "/register", &signed_body, r.now()).to_raw();
    let res = r.register_with(&auth, &sent_body, "i2-attacker-chosen");
    rep.unauthenticated(
        Property::TamperedBodyRejected,
        "register whose body differs from the signed body",
        res,
    );
}

fn replay(stages: Stages, dir: &Path, rep: &mut Report) {
    let Some(mut r) = rig(rep, stages, dir, "replay", base_cfg()) else {
        return;
    };
    let a = Signer::from_dialkey(DK_A);
    let body = Json(register_req("i1")).encode();
    let auth = r.sign(&a, "/register", &body, r.now()).to_raw();
    let first = r.register_with(&auth, &body, "i1");
    if rep
        .setup(
            Property::ValidRequestAccepted,
            "the first send of a valid request",
            first,
        )
        .is_none()
    {
        return;
    }
    // Byte-identical resend, inside the skew window.
    r.clock.advance(1);
    let again = r.register_with(&auth, &body, "i1");
    rep.unauthenticated(
        Property::ReplayRejected,
        "byte-identical replay of an accepted request",
        again,
    );
}

fn timestamps(stages: Stages, dir: &Path, rep: &mut Report) {
    let Some(mut r) = rig(rep, stages, dir, "timestamps", base_cfg()) else {
        return;
    };
    let a = Signer::from_dialkey(DK_A);
    let body = Json(register_req("i1")).encode();

    // Within skew of "now" but before this process started: could be a pre-restart capture.
    // (Probed first, while START - 1 is still inside the skew window, so only the not-before
    // floor can reject it.)
    let pre_boot = r.sign(&a, "/register", &body, START - 1).to_raw();
    let res = r.register_with(&pre_boot, &body, "i1");
    rep.unauthenticated(
        Property::PreBootRequestRejected,
        "request stamped before the control plane started",
        res,
    );

    let old = r.sign(&a, "/register", &body, r.now()).to_raw();
    r.clock.advance(SKEW + 1);
    let res = r.register_with(&old, &body, "i1");
    rep.unauthenticated(
        Property::StaleTimestampRejected,
        "request older than the skew limit",
        res,
    );

    let future = r.sign(&a, "/register", &body, r.now() + SKEW + 1).to_raw();
    let res = r.register_with(&future, &body, "i1");
    rep.unauthenticated(
        Property::FutureTimestampRejected,
        "request dated beyond the skew limit in the future",
        res,
    );
}

fn ownership(stages: Stages, dir: &Path, rep: &mut Report) {
    let Some(mut r) = rig(rep, stages, dir, "ownership", base_cfg()) else {
        return;
    };
    let (a, b) = (Signer::from_dialkey(DK_A), Signer::from_dialkey(DK_B));
    let reg = r.register(&a, "i1");
    if rep
        .setup(Property::ValidRequestAccepted, "register", reg)
        .is_none()
    {
        return;
    }
    let res = r.heartbeat(&b, "i1", true);
    rep.rejected(
        Property::WrongKeyHeartbeatRejected,
        "a validly signed heartbeat from a key the id is not bound to",
        res,
    );
    let res = r.heartbeat(&a, "i1", true);
    rep.setup(
        Property::OwnerHeartbeatAccepted,
        "the bound key's heartbeat",
        res,
    );
    let res = r.register(&b, "i1");
    rep.rejected(
        Property::RebindRejected,
        "register of a bound id by a second key",
        res,
    );
    rep.ensure(
        r.plane.bound_key("i1") == Some(a.public_key()),
        Property::RebindRejected,
        "the id is no longer bound to its first key",
    );
    let res = r.register(&a, "i1");
    rep.setup(
        Property::ValidRequestAccepted,
        "re-register by the bound key",
        res,
    );
}

fn expiry(stages: Stages, dir: &Path, rep: &mut Report) {
    let Some(mut r) = rig(rep, stages, dir, "expiry", base_cfg()) else {
        return;
    };
    let a = Signer::from_dialkey(DK_A);
    let reg = r.register(&a, "i1");
    if rep
        .setup(Property::ValidRequestAccepted, "register", reg)
        .is_none()
    {
        return;
    }
    rep.ensure(
        r.healthy("i1") == Some(true),
        Property::HealthyInsideTtl,
        "a just-registered instance is not healthy",
    );
    r.clock.advance(TTL - 1);
    rep.ensure(
        r.healthy("i1") == Some(true),
        Property::HealthyInsideTtl,
        format!(
            "not healthy {} s after its last sign of life (ttl {TTL})",
            TTL - 1
        ),
    );
    r.clock.advance(1);
    rep.ensure(
        r.healthy("i1") == Some(false),
        Property::ExpiresAfterTtl,
        format!("still healthy {TTL} s after its last sign of life (ttl {TTL})"),
    );
    let hb = r.heartbeat(&a, "i1", true);
    rep.setup(
        Property::OwnerHeartbeatAccepted,
        "a heartbeat after expiry",
        hb,
    );
    rep.ensure(
        r.healthy("i1") == Some(true),
        Property::HeartbeatRevives,
        "a heartbeat did not revive an expired instance",
    );
    let hb = r.heartbeat(&a, "i1", false);
    rep.setup(
        Property::OwnerHeartbeatAccepted,
        "an unhealthy heartbeat",
        hb,
    );
    rep.ensure(
        r.healthy("i1") == Some(false),
        Property::UnhealthyReportHonored,
        "healthy=false in a heartbeat was ignored",
    );
    r.clock.advance(10 * TTL);
    rep.ensure(
        r.healthy("i1") == Some(false),
        Property::ExpiresAfterTtl,
        "healthy long after the last heartbeat",
    );
}

fn nonce_window(stages: Stages, dir: &Path, rep: &mut Report) {
    let mut cfg = base_cfg();
    cfg.nonce_capacity = 3;
    let Some(mut r) = rig(rep, stages, dir, "nonce_window", cfg) else {
        return;
    };
    let a = Signer::from_dialkey(DK_A);
    let body = Json(register_req("i1")).encode();
    let reqs: Vec<RawAuth> = (0..4)
        .map(|_| r.sign(&a, "/register", &body, r.now()).to_raw())
        .collect();
    for (n, auth) in reqs[..3].iter().enumerate() {
        let res = r.register_with(auth, &body, "i1");
        if rep
            .setup(
                Property::ValidRequestAccepted,
                &format!("request {n} inside the window"),
                res,
            )
            .is_none()
        {
            return;
        }
    }
    let res = r.register_with(&reqs[3], &body, "i1");
    rep.rejected(
        Property::NonceWindowBounded,
        "a 4th live nonce in a window of capacity 3",
        res,
    );
    let res = r.register_with(&reqs[0], &body, "i1");
    rep.unauthenticated(
        Property::LiveNonceNeverEvicted,
        "replay of the oldest live nonce after the window filled",
        res,
    );
}

fn config_push_and_verification(stages: Stages, dir: &Path, rep: &mut Report) {
    let Some(mut r) = rig(rep, stages, dir, "config", base_cfg()) else {
        return;
    };
    let (a, b, admin) = (
        Signer::from_dialkey(DK_A),
        Signer::from_dialkey(DK_B),
        Signer::from_dialkey(DK_ADMIN),
    );
    let reg = r.register(&a, "i1");
    if rep
        .setup(Property::ValidRequestAccepted, "register i1", reg)
        .is_none()
    {
        return;
    }
    let reg = r.register(&b, "i2");
    if rep
        .setup(Property::ValidRequestAccepted, "register i2", reg)
        .is_none()
    {
        return;
    }

    let res = r.push(&a, "i1", "evil: true");
    rep.rejected(
        Property::NonAdminPushRejected,
        "a config push signed by a registered non-admin key",
        res,
    );
    rep.ensure(
        r.plane.config_version() == 0,
        Property::NonAdminPushRejected,
        "a rejected push consumed a config_version",
    );

    let first = r.push(&admin, "i1", "upstream: a");
    let Some(first) = rep.setup(Property::ValidRequestAccepted, "an admin push", first) else {
        return;
    };
    let second = r.push(&admin, "i1", "upstream: b");
    let Some(second) = rep.setup(
        Property::ValidRequestAccepted,
        "a second admin push",
        second,
    ) else {
        return;
    };
    rep.ensure(
        first.config_version != 0 && second.config_version == first.config_version + 1,
        Property::ConfigVersionMonotonic,
        format!(
            "versions {} then {}",
            first.config_version, second.config_version
        ),
    );

    let control = r.plane.control_public_key();
    let body = second.deliveries[0].body.clone();
    let ver = second.config_version;
    let verify = |body: &[u8], key: &Key, instance: &str, last: u64| {
        verify_config_with(&stages, body, key, instance, last)
    };

    let valid = verify(&body, &control, "i1", ver - 1);
    rep.ensure(
        valid.is_ok(),
        Property::ConfigValidAccepted,
        format!("a validly signed config was rejected: {:?}", valid.err()),
    );

    let env = match Json::<SignedConfig>::from_bytes(&body) {
        Ok(Json(env)) => env,
        Err(e) => {
            rep.fail(
                Property::ConfigValidAccepted,
                format!("pushed body is not an envelope: {e}"),
            );
            return;
        }
    };
    // Unsigned: empty signature, and a bare (pre-envelope) config.
    let unsigned = Json(SignedConfig {
        signature: String::new(),
        ..env.clone()
    })
    .encode();
    rep.ensure(
        verify(&unsigned, &control, "i1", 0).is_err()
            && verify(b"upstream: b", &control, "i1", 0).is_err(),
        Property::ConfigUnsignedRejected,
        "an unsigned config was accepted",
    );
    // Forged: right shape, signature bytes that are not the control plane's.
    let forged = Json(SignedConfig {
        signature: "11".repeat(64),
        ..env.clone()
    })
    .encode();
    rep.ensure(
        verify(&forged, &control, "i1", 0).is_err(),
        Property::ConfigForgedSignatureRejected,
        "a config with a made-up signature was accepted",
    );
    // Wrong key: a well-formed config signed by someone else.
    let imposter = sign_config(
        &Signer::from_dialkey("conformance-imposter"),
        "i1",
        ver,
        "upstream: b",
    );
    rep.ensure(
        verify(&imposter, &control, "i1", 0).is_err(),
        Property::ConfigWrongKeyRejected,
        "a config signed by a different key was accepted",
    );
    // Tampered: signature intact, content changed.
    let tampered = Json(SignedConfig {
        config: "upstream: attacker".into(),
        ..env.clone()
    })
    .encode();
    rep.ensure(
        verify(&tampered, &control, "i1", 0).is_err(),
        Property::ConfigTamperedRejected,
        "a config whose text was altered after signing was accepted",
    );
    // Wrong target: i1's config presented to i2.
    rep.ensure(
        verify(&body, &control, "i2", 0).is_err(),
        Property::ConfigWrongTargetRejected,
        "a config signed for i1 was accepted by i2",
    );
    // Replay / downgrade.
    rep.ensure(
        verify(&body, &control, "i1", ver).is_err(),
        Property::ConfigReplayRejected,
        "the config version just applied was accepted again",
    );
    let old = &first.deliveries[0].body;
    rep.ensure(
        verify(old, &control, "i1", ver).is_err(),
        Property::ConfigReplayRejected,
        "an older config was accepted after a newer one was applied",
    );
}

fn durability(stages: Stages, dir: &Path, rep: &mut Report) {
    let Some(mut r) = rig(rep, stages, dir, "durability", base_cfg()) else {
        return;
    };
    let (a, b, admin) = (
        Signer::from_dialkey(DK_A),
        Signer::from_dialkey(DK_B),
        Signer::from_dialkey(DK_ADMIN),
    );
    let reg = r.register(&a, "i1");
    if rep
        .setup(Property::ValidRequestAccepted, "register", reg)
        .is_none()
    {
        return;
    }
    for config in ["one", "two"] {
        let res = r.push(&admin, "i1", config);
        if rep
            .setup(Property::ValidRequestAccepted, "an admin push", res)
            .is_none()
        {
            return;
        }
    }
    if let Err(e) = r.restart() {
        rep.0.push(e);
        return;
    }

    rep.ensure(
        r.plane.bound_key("i1") == Some(a.public_key()),
        Property::IdentitySurvivesRestart,
        "the id -> key binding was lost across a restart",
    );
    rep.ensure(
        r.plane.config_version() == 2,
        Property::ConfigVersionSurvivesRestart,
        format!(
            "config_version is {} after restart, expected 2",
            r.plane.config_version()
        ),
    );
    let res = r.heartbeat(&b, "i1", true);
    rep.rejected(
        Property::IdentitySurvivesRestart,
        "a heartbeat by a foreign key after restart",
        res,
    );
    let res = r.register(&b, "i1");
    rep.rejected(
        Property::IdentitySurvivesRestart,
        "a re-register by a foreign key after restart",
        res,
    );
    rep.ensure(
        r.healthy("i1") == Some(false),
        Property::ExpiresAfterTtl,
        "an instance not heard from since restart is reported healthy",
    );
    let res = r.heartbeat(&a, "i1", true);
    rep.setup(
        Property::OwnerHeartbeatAccepted,
        "the bound key's heartbeat after restart",
        res,
    );
    match r.push(&admin, "i1", "three") {
        Ok(next) => rep.ensure(
            next.config_version == 3,
            Property::ConfigVersionSurvivesRestart,
            format!(
                "first push after restart carries version {}, expected 3",
                next.config_version
            ),
        ),
        Err(e) => rep.fail(
            Property::ConfigVersionSurvivesRestart,
            format!("push rejected after restart: {e}"),
        ),
    }
}

fn storage_failure(stages: Stages, _dir: &Path, rep: &mut Report) {
    let clock = ManualClock::new(START);
    let mut plane = ControlPlane::from_parts(
        base_cfg(),
        Arc::new(clock.clone()),
        stages,
        store::Replayed::default(),
        Box::new(FailingSink),
    );
    let a = Signer::from_dialkey(DK_A);
    let body = Json(register_req("i1")).encode();
    let auth = a.sign_now("POST", "/register", &body, START).to_raw();
    let call = SignedCall {
        method: "POST",
        path: "/register",
        body: &body,
        auth: &auth,
    };
    match plane.register(&call, register_req("i1")) {
        Err(ControlError::Storage(_)) => {}
        other => rep.fail(
            Property::StorageErrorNotApplied,
            format!("a failed append was not reported as a storage error: {other:?}"),
        ),
    }
    rep.ensure(
        plane.list().is_empty() && plane.bound_key("i1").is_none(),
        Property::StorageErrorNotApplied,
        "a register whose append failed was applied in memory anyway",
    );
}

// ── enrollment, the signed read path, compaction ───────────────────────────────────────────

/// A refusal must be exactly [`ControlError::NotEnrolled`] (the 403 class), not merely "an error".
fn not_enrolled<T>(rep: &mut Report, property: Property, what: &str, r: Result<T, ControlError>) {
    match r {
        Err(ControlError::NotEnrolled) => {}
        Err(other) => rep.fail(
            property,
            format!("{what} was refused, but not as NotEnrolled: {other}"),
        ),
        Ok(_) => rep.fail(property, format!("{what} was accepted")),
    }
}

fn enrollment(stages: Stages, dir: &Path, rep: &mut Report) {
    let Some(mut r) = rig(rep, stages, dir, "enrollment", base_cfg()) else {
        return;
    };
    let (a, outsider) = (
        Signer::from_dialkey(DK_A),
        Signer::from_dialkey(DK_OUTSIDER),
    );
    let reg = r.register(&a, "i1");
    if rep
        .setup(
            Property::ValidRequestAccepted,
            "register by an enrolled key",
            reg,
        )
        .is_none()
    {
        return;
    }

    // The refusal is exactly `NotEnrolled` (the 403 class). Its wire text, NOT_ENROLLED, is a
    // constant of the error type, not a decision point, so it is asserted over real sockets in
    // `tests/http.rs`, not here.
    let res = r.register(&outsider, "intruder-1");
    not_enrolled(
        rep,
        Property::UnenrolledRegisterRejected,
        "register by a validly signing key on no allowlist",
        res,
    );
    rep.ensure(
        r.plane.bound_key("intruder-1").is_none(),
        Property::UnenrolledRegisterRejected,
        "an unenrolled register bound an id",
    );
    let outsider_key = outsider.public_key();
    let logged = move |p: &ControlPlane| {
        p.rejections()
            .iter()
            .any(|x| x.key == outsider_key && x.id == "intruder-1")
    };
    rep.ensure(
        logged(&r.plane),
        Property::RejectionIsLogged,
        "the refused register was not recorded",
    );
    if let Err(e) = r.restart() {
        rep.0.push(e);
        return;
    }
    rep.ensure(
        logged(&r.plane),
        Property::RejectionIsLogged,
        "the refused register did not survive a restart",
    );

    // Removing a key from the allowlist closes the door on it at the next start.
    r.cfg.enrolled_keys.retain(|k| *k != a.public_key());
    if let Err(e) = r.restart() {
        rep.0.push(e);
        return;
    }
    let res = r.register(&a, "i1");
    not_enrolled(
        rep,
        Property::UnenrolledRegisterRejected,
        "register by a key removed from the allowlist",
        res,
    );
}

fn empty_allowlist(stages: Stages, dir: &Path, rep: &mut Report) {
    let mut cfg = base_cfg();
    cfg.enrolled_keys.clear();
    let Some(mut r) = rig(rep, stages, dir, "closed", cfg) else {
        return;
    };
    let a = Signer::from_dialkey(DK_A);
    let res = r.register(&a, "i1");
    rep.rejected(
        Property::EmptyAllowlistIsClosed,
        "register with an empty allowlist",
        res,
    );
    let (_, res) = r.list_signed_as(&a);
    rep.rejected(
        Property::EmptyAllowlistIsClosed,
        "list with an empty allowlist",
        res,
    );
}

fn signed_list(stages: Stages, dir: &Path, rep: &mut Report) {
    let Some(mut r) = rig(rep, stages, dir, "list", base_cfg()) else {
        return;
    };
    let (a, b) = (Signer::from_dialkey(DK_A), Signer::from_dialkey(DK_B));
    let (outsider, admin) = (
        Signer::from_dialkey(DK_OUTSIDER),
        Signer::from_dialkey(DK_ADMIN),
    );
    let reg = r.register(&a, "i1");
    if rep
        .setup(Property::ValidRequestAccepted, "register i1", reg)
        .is_none()
    {
        return;
    }

    let res = r.list_with(&RawAuth::default());
    rep.unauthenticated(
        Property::ListUnsignedRejected,
        "list with no credentials",
        res,
    );
    // Claims to be A, signed by B.
    let now = r.now();
    let mut forged = a.sign_now("GET", "/instances", b"", now);
    forged.signature = b
        .sign("GET", "/instances", b"", now, &forged.nonce)
        .signature;
    let res = r.list_with(&forged.to_raw());
    rep.unauthenticated(
        Property::ListForgedRejected,
        "list signed by a different key than it claims",
        res,
    );
    // A genuine signature of A's, made for a different call.
    let other = a.sign_now("POST", "/heartbeat", b"{}", now).to_raw();
    let res = r.list_with(&other);
    rep.unauthenticated(
        Property::ListForgedRejected,
        "list carrying a signature made for another call",
        res,
    );
    for (who, s) in [("an outsider", &outsider), ("the admin key", &admin)] {
        let (_, res) = r.list_signed_as(s);
        not_enrolled(
            rep,
            Property::ListUnenrolledRejected,
            &format!("a validly signed list by {who} (enrolled nowhere)"),
            res,
        );
    }

    let (auth, res) = r.list_signed_as(&a);
    if let Some(views) = rep.setup(
        Property::ListEnrolledAccepted,
        "a signed list by an enrolled key",
        res,
    ) {
        rep.ensure(
            views.len() == 1 && views[0].id == "i1",
            Property::ListEnrolledAccepted,
            format!("the list is {views:?}"),
        );
    }
    let again = r.list_with(&auth);
    rep.unauthenticated(
        Property::ListReplayRejected,
        "byte-identical replay of an accepted list",
        again,
    );
}

struct Cast {
    a: Signer,
    b: Signer,
    admin: Signer,
    outsider: Signer,
}

fn cast() -> Cast {
    Cast {
        a: Signer::from_dialkey(DK_A),
        b: Signer::from_dialkey(DK_B),
        admin: Signer::from_dialkey(DK_ADMIN),
        outsider: Signer::from_dialkey(DK_OUTSIDER),
    }
}

/// The history compaction is judged on, part 1: churn on one id, three instances, pushes and
/// refusals. `false` = a step that must succeed was refused (already recorded).
fn script_head(r: &mut Rig, rep: &mut Report, c: &Cast) -> bool {
    for (s, id, addr) in [
        (&c.a, "i1", "http://10.0.0.1:1"),
        (&c.b, "i2", "http://10.0.0.2:1"),
        (&c.a, "i3", "http://10.0.0.3:1"),
        (&c.a, "i1", "http://10.0.0.1:2"),
        (&c.a, "i1", "http://10.0.0.1:3"),
    ] {
        let res = r.register_at(s, id, addr);
        if rep
            .setup(
                Property::ValidRequestAccepted,
                &format!("register {id}"),
                res,
            )
            .is_none()
        {
            return false;
        }
    }
    for config in ["one", "two"] {
        let res = r.push(&c.admin, "*", config);
        if rep
            .setup(Property::ValidRequestAccepted, "an admin push", res)
            .is_none()
        {
            return false;
        }
    }
    for id in ["evil-1", "evil-2"] {
        let _ = r.register(&c.outsider, id);
    }
    true
}

/// Part 2: what lands in the log *after* a compaction (the tail). It has no config push on
/// purpose: a later `ConfigPushed` would re-establish `config_version` by itself and hide a
/// snapshot that forgot it. (The randomized history in `tests/compaction.rs` mixes pushes into
/// the tail.)
fn script_tail(r: &mut Rig, rep: &mut Report, c: &Cast) -> bool {
    for (s, id, addr) in [
        (&c.b, "i4", "http://10.0.0.4:1"),
        (&c.b, "i2", "http://10.0.0.2:9"),
    ] {
        let res = r.register_at(s, id, addr);
        if rep
            .setup(
                Property::ValidRequestAccepted,
                &format!("register {id}"),
                res,
            )
            .is_none()
        {
            return false;
        }
    }
    let _ = r.register(&c.outsider, "evil-3");
    true
}

/// A restart that must open; a failure to open is the property's violation.
fn reopen(r: &mut Rig, rep: &mut Report, property: Property) -> bool {
    match r.restart() {
        Ok(()) => true,
        Err(mut e) => {
            e.property = property;
            rep.0.push(e);
            false
        }
    }
}

/// `replay(snapshot + tail) == replay(full log)`: two rigs run the same history, one is
/// compacted in the middle; after a restart they must agree on everything the log holds, and the
/// compacted one must keep refusing a request accepted before the compaction.
fn compaction_equivalence(stages: Stages, dir: &Path, rep: &mut Report) {
    let Some(mut a) = rig(rep, stages, dir, "cmp-compacted", base_cfg()) else {
        return;
    };
    let Some(mut b) = rig(rep, stages, dir, "cmp-plain", base_cfg()) else {
        return;
    };
    let c = cast();
    if !script_head(&mut a, rep, &c) || !script_head(&mut b, rep, &c) {
        return;
    }

    // A request accepted before the compaction, to be replayed after it.
    let req = RegisterRequest {
        addr: "http://10.0.0.1:3".into(),
        ..register_req("i1")
    };
    let body = Json(req.clone()).encode();
    let captured = a.sign(&c.a, "/register", &body, a.now()).to_raw();
    let first = a.plane.register(
        &SignedCall {
            method: "POST",
            path: "/register",
            body: &body,
            auth: &captured,
        },
        req.clone(),
    );
    if rep
        .setup(
            Property::ValidRequestAccepted,
            "the request that will be replayed",
            first,
        )
        .is_none()
    {
        return;
    }

    let before_state = a.plane.state();
    let before_len = a.log_len();
    let compacted = a.plane.compact();
    if rep
        .setup(Property::CompactionPreservesState, "compaction", compacted)
        .is_none()
    {
        return;
    }
    rep.ensure(
        a.log_len() < before_len,
        Property::CompactionShrinksLog,
        format!(
            "the log is {} bytes after compaction, it was {before_len}",
            a.log_len()
        ),
    );
    rep.ensure(
        a.plane.state() == before_state,
        Property::CompactionPreservesState,
        "compaction changed the in-memory state",
    );
    let again = a.plane.register(
        &SignedCall {
            method: "POST",
            path: "/register",
            body: &body,
            auth: &captured,
        },
        req.clone(),
    );
    rep.unauthenticated(
        Property::CompactionKeepsReplayProtection,
        "replay of a pre-compaction request, same process",
        again,
    );

    if !script_tail(&mut a, rep, &c) || !script_tail(&mut b, rep, &c) {
        return;
    }
    if !reopen(&mut a, rep, Property::CompactionPreservesState)
        || !reopen(&mut b, rep, Property::CompactionPreservesState)
    {
        return;
    }

    let (sa, sb) = (a.plane.state(), b.plane.state());
    rep.ensure(
        sa.0 == sb.0,
        Property::CompactionKeepsIdentity,
        "the instance table (ids, addresses, bound keys) differs from the full-log replay",
    );
    rep.ensure(
        sa.1 == sb.1,
        Property::CompactionKeepsConfigVersion,
        format!(
            "config_version is {} after snapshot + tail, {} after the full log",
            sa.1, sb.1
        ),
    );
    rep.ensure(
        sa.2 == sb.2,
        Property::CompactionKeepsAudit,
        "the enrollment audit trail differs from the full-log replay",
    );
    rep.ensure(
        sa == sb,
        Property::CompactionPreservesState,
        "replay(snapshot + tail) != replay(full log)",
    );

    // Identity is still enforced on the compacted store.
    let res = a.heartbeat(&c.b, "i1", true);
    rep.rejected(
        Property::CompactionKeepsIdentity,
        "a heartbeat for i1 by a key it is not bound to, after compaction and restart",
        res,
    );
    let res = a.register(&c.b, "i1");
    rep.rejected(
        Property::CompactionKeepsIdentity,
        "a re-register of i1 by a second key, after compaction and restart",
        res,
    );
    // The config_version counter continues, it does not restart.
    if let (Ok(pa), Ok(pb)) = (a.push(&c.admin, "*", "four"), b.push(&c.admin, "*", "four")) {
        rep.ensure(
            pa.config_version == pb.config_version && pa.config_version == 3,
            Property::CompactionKeepsConfigVersion,
            format!(
                "the next push is version {} (full-log twin: {}), expected 3",
                pa.config_version, pb.config_version
            ),
        );
    }
    // Replay protection survives compaction + restart.
    let again = a.plane.register(
        &SignedCall {
            method: "POST",
            path: "/register",
            body: &body,
            auth: &captured,
        },
        req,
    );
    rep.unauthenticated(
        Property::CompactionKeepsReplayProtection,
        "replay of a pre-compaction request after compaction and restart",
        again,
    );
}

/// Kill the process (and, separately, lose power) after every prefix of the compaction plan.
/// Whatever the disk holds then must open to exactly the pre-compaction state, and stay writable.
fn compaction_crash(stages: Stages, dir: &Path, rep: &mut Report) {
    let plan_len = (stages.compaction_plan)().len();
    // Seed one directory with the head history; every crash point starts from a copy of it
    // (the copy is a quiescent, fully fsynced log: exactly what a compaction starts from).
    let Some(mut seed) = rig(rep, stages, dir, "crash-seed", base_cfg()) else {
        return;
    };
    if !script_head(&mut seed, rep, &cast()) {
        return;
    }
    let expected = seed.plane.state();
    let seed_dir = seed.dir.clone();
    drop(seed);
    for power_loss in [false, true] {
        for k in 0..=plan_len {
            let case = CrashCase {
                k,
                plan_len,
                power_loss,
            };
            crash_once(stages, dir, &seed_dir, &expected, case, rep);
        }
    }
}

/// Crash after `k` of the plan's `plan_len` operations, with or without the power-loss model.
#[derive(Clone, Copy)]
struct CrashCase {
    k: usize,
    plan_len: usize,
    power_loss: bool,
}

fn crash_once(
    stages: Stages,
    dir: &Path,
    seed_dir: &Path,
    expected: &store::Canonical,
    case: CrashCase,
    rep: &mut Report,
) {
    let CrashCase {
        k,
        plan_len,
        power_loss,
    } = case;
    let model = if power_loss { "power loss" } else { "kill" };
    let label = format!("{model} after {k} of {plan_len} steps");
    let dir = dir.join(format!(
        "crash-{}-{k}",
        if power_loss { "power" } else { "kill" }
    ));
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|()| {
        std::fs::copy(seed_dir.join(store::LOG_FILE), dir.join(store::LOG_FILE)).map(|_| ())
    }) {
        rep.fail(
            Property::CompactionCrashSafe,
            format!("could not copy the seeded log: {e}"),
        );
        return;
    }
    let clock = ManualClock::new(START);
    let cfg = base_cfg();
    let lossy = LossyFs::new(Some(k));
    let plane = match ControlPlane::open_with_fs(
        &dir,
        cfg.clone(),
        Arc::new(clock.clone()),
        stages,
        Box::new(lossy.clone()),
    ) {
        Ok(p) => p,
        Err(e) => {
            rep.fail(Property::CompactionCrashSafe, format!("open failed: {e}"));
            return;
        }
    };
    let mut r = Rig {
        plane,
        clock: clock.clone(),
        dir: dir.clone(),
        stages,
        cfg: cfg.clone(),
    };
    let c = cast();
    let outcome = r.plane.compact();
    if k >= plan_len {
        if let Err(e) = &outcome {
            rep.fail(
                Property::CompactionCrashSafe,
                format!("compaction failed with no fault injected ({label}): {e}"),
            );
        }
    }
    drop(r);
    if power_loss {
        if let Err(e) = lossy.power_loss() {
            rep.fail(
                Property::CompactionCrashSafe,
                format!("could not apply the power-loss model: {e}"),
            );
            return;
        }
    }

    let plane = match ControlPlane::open(&dir, cfg.clone(), Arc::new(clock.clone()), stages) {
        Ok(p) => p,
        Err(e) => {
            rep.fail(
                Property::CompactionCrashSafe,
                format!("the store cannot be opened after {label}: {e}"),
            );
            return;
        }
    };
    let mut r = Rig {
        plane,
        clock,
        dir,
        stages,
        cfg,
    };
    rep.ensure(
        r.plane.state() == *expected,
        Property::CompactionCrashSafe,
        format!("the state recovered after {label} differs from the state before the compaction"),
    );
    // Recovery leaves a store that accepts writes, and those writes are durable.
    let res = r.register_at(&c.b, "after-crash", "http://10.0.0.9:1");
    if rep
        .setup(
            Property::CompactionCrashSafe,
            &format!("a register after recovery from {label}"),
            res,
        )
        .is_none()
    {
        return;
    }
    if !reopen(&mut r, rep, Property::CompactionCrashSafe) {
        return;
    }
    rep.ensure(
        r.plane.bound_key("after-crash") == Some(c.b.public_key()),
        Property::CompactionCrashSafe,
        format!("a record written after recovery from {label} was lost by the next restart"),
    );
}

const SCENARIOS: &[Scenario] = &[
    valid_flow,
    unsigned,
    forged_signature,
    tampered_body,
    replay,
    timestamps,
    ownership,
    expiry,
    nonce_window,
    config_push_and_verification,
    durability,
    storage_failure,
    enrollment,
    empty_allowlist,
    signed_list,
    compaction_equivalence,
    compaction_crash,
];

/// Run every scenario against an implementation built from `stages`; return every violated
/// property (empty = conforms). `dir` must be an empty scratch directory.
pub fn check_all(stages: Stages, dir: &Path) -> Vec<Violation> {
    let mut report = Report::default();
    for scenario in SCENARIOS {
        scenario(stages, dir, &mut report);
    }
    report.0
}

/// [`check_all`], as a pass/fail: `Err` carries the first violation.
pub fn check(stages: Stages, dir: &Path) -> Result<(), Violation> {
    match check_all(stages, dir).into_iter().next() {
        None => Ok(()),
        Some(first) => Err(first),
    }
}

// ---------------------------------------------------------------------------------------------
// Identity derivation: `public_key_of` and `transport::dialkey::peer_id`
// ---------------------------------------------------------------------------------------------

/// The pair of functions that define "the identity of a dialkey". `Identity::REAL` is what ships;
/// the mutants in `tests/identity_controls.rs` wrap or alter the real functions.
#[derive(Clone, Copy)]
pub struct Identity {
    /// What this crate puts on the wire as the signer (`auth::public_key_of`).
    pub public_key_of: fn(&str) -> Key,
    /// What `transport` calls the dialkey's `PeerId`. Public key since transport 0.2.0; before
    /// that it was the secret seed.
    pub peer_id: fn(&str) -> Key,
}

impl Identity {
    pub const REAL: Identity = Identity {
        public_key_of,
        peer_id: transport::dialkey::peer_id,
    };
}

/// Literal test dialkey (not a credential) whose public key is pinned below. The same vector is
/// pinned in transport's own tests at d4745b9.
pub const GOLDEN_DIALKEY: &str = "g17-test-dialkey";
pub const GOLDEN_PUBLIC_HEX: &str =
    "30966ac5fa1bdd49aec9b9101bf63359704d01dc00597f934f7d7bca9bc5c47b";

/// The documented secret seed, `SHA-256(dialkey ++ ":pkarr")`. transport keeps its own accessor
/// crate-private, so the seed is recomputed here; [`check_identity`] ties it to the real keypair
/// (`Keypair::from_secret_key(seed).public_key() == public_key_of`), so a wrong recomputation
/// fails the checker instead of making the "not the seed" probe vacuous.
pub fn documented_seed(dialkey: &str) -> Key {
    crate::auth::sha256(format!("{dialkey}:pkarr").as_bytes())
}

/// Judge an [`Identity`]: every violated property, empty = conforms.
pub fn check_identity(id: Identity) -> Vec<Violation> {
    use crate::auth::hex_encode;
    let mut out = Vec::new();
    let dialkeys = [GOLDEN_DIALKEY, DK_A, DK_B];
    for dk in dialkeys {
        let public = (id.public_key_of)(dk);
        let peer = (id.peer_id)(dk);
        let seed = documented_seed(dk);
        if peer != public {
            out.push(v(
                Property::PeerIdIsPublicKey,
                format!("peer_id({dk}) != public_key_of({dk})"),
            ));
        }
        if peer == seed || public == seed {
            out.push(v(
                Property::IdentityIsNotTheSecretSeed,
                format!(
                    "the identity of {dk} equals SHA-256(dialkey ++ \":pkarr\"), the secret seed"
                ),
            ));
        }
        let derived = *Keypair::from_secret_key(&seed).public_key().as_bytes();
        if public != derived {
            out.push(v(
                Property::PublicKeyFollowsDerivation,
                format!("public_key_of({dk}) is not the public half of the documented seed"),
            ));
        }
    }
    if (id.public_key_of)(DK_A) == (id.public_key_of)(DK_B) {
        out.push(v(
            Property::IdentityDistinctPerDialkey,
            "two different dialkeys share one identity",
        ));
    }
    for (name, f) in [("public_key_of", id.public_key_of), ("peer_id", id.peer_id)] {
        let got = hex_encode(&f(GOLDEN_DIALKEY));
        if got != GOLDEN_PUBLIC_HEX {
            out.push(v(
                Property::IdentityMatchesGolden,
                format!("{name}({GOLDEN_DIALKEY}) = {got}, expected {GOLDEN_PUBLIC_HEX}"),
            ));
        }
    }
    out
}

/// [`check_identity`], as a pass/fail: `Err` carries the first violation.
pub fn check_identity_ok(id: Identity) -> Result<(), Violation> {
    match check_identity(id).into_iter().next() {
        None => Ok(()),
        Some(first) => Err(first),
    }
}
