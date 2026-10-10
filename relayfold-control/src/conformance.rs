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

use crate::auth::{key_to_z32, public_key_of, AuthHeaders, Key, RawAuth, SignedCall, Signer};
use crate::clock::{Clock, ManualClock};
use crate::config::{sign_config, verify_config_with, SignedConfig};
use crate::dto::{ConfigPushRequest, HeartbeatRequest, InstanceView, RegisterRequest};
use crate::json::Json;
use crate::plane::{ControlConfig, ControlError, ControlPlane, PreparedPush};
use crate::stages::Stages;
use crate::store::{self, RecordSink};
use std::path::Path;
use std::sync::Arc;

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
    let mut cfg = ControlConfig::new(DK_CONTROL, vec![public_key_of(DK_ADMIN)]);
    cfg.ttl_secs = TTL;
    cfg.skew_secs = SKEW;
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
