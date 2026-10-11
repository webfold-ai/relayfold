//! The decision points of the auth / liveness / config-verification pipeline, collected so each
//! can be swapped for a deliberately wrong one.
//!
//! `Stages::REAL` is what ships. A *mutant* is `Stages { signature_ok: |..| true, ..Stages::REAL }`:
//! the real pipeline (`auth::Verifier`, `liveness::fold`, `config::verify_config_with`, the
//! control plane) runs unchanged around one altered decision. The conformance checker
//! (`conformance::check`) is generic over `Stages`, so the same function judges the real
//! implementation and every mutant — see `tests/negative_controls.rs`.
//!
//! These are plain `fn` pointers (no captures, no trait objects), so `Stages` is `Copy` and a
//! mutant is a non-capturing closure.

use crate::auth::{self, AuthError, Key, NonceVerdict, NonceWindow};
use crate::compaction::{self, Step};
use crate::config;
use crate::liveness::{self, Liveness, LivenessEvent};
use crate::store::Snapshot;

#[derive(Clone, Copy)]
pub struct Stages {
    /// `(now, request_ts, skew, not_before) -> accept?`
    pub timestamp_ok: fn(i64, i64, i64, i64) -> bool,
    /// Body digest folded into the signed message.
    pub body_hash: fn(&[u8]) -> [u8; 32],
    /// Request signature check: `(signer, message, signature) -> valid?`
    pub signature_ok: fn(&Key, &[u8], &[u8; 64]) -> bool,
    /// Replay window: `(window, signer, nonce, request_ts, now)`.
    pub nonce_fresh: fn(&mut NonceWindow, &Key, &str, i64, i64) -> NonceVerdict,
    /// May `caller` heartbeat an id bound to `bound`?
    pub may_heartbeat: fn(&Key, &Key) -> bool,
    /// May `caller` re-register an id already bound to `bound`?
    pub may_reregister: fn(&Key, &Key) -> bool,
    /// Instance liveness fold: `(state, event, ttl) -> state`.
    pub liveness_fold: fn(Liveness, LivenessEvent, i64) -> Liveness,
    /// Config signature check: `(control key, message, signature) -> valid?`
    pub config_signature_ok: fn(&Key, &[u8], &[u8; 64]) -> bool,
    /// Config version: `(pushed, last_applied) -> accept?`
    pub config_version_ok: fn(u64, u64) -> bool,
    /// Config target: `(signed_for, this_instance) -> accept?`
    pub config_target_ok: fn(&str, &str) -> bool,
    /// Enrollment gate on `/register`: `(enrolled keys, signer) -> may register?`
    pub enrolled: fn(&[Key], &Key) -> bool,
    /// Read gate on `GET /instances`: `(enrolled keys, signer) -> may list?`
    pub may_list: fn(&[Key], &Key) -> bool,
    /// What the read path does with the verifier's verdict on a `GET /instances` request, which
    /// signer it goes on to authorise (REAL: the verdict itself). A mutant that turns an error into
    /// an identity re-creates a read path that does not authenticate.
    pub read_auth: fn(Result<Key, AuthError>) -> Result<Key, AuthError>,
    /// Is a refused enrollment written to the durable audit trail? `(signer, requested id)`
    pub record_rejection: fn(&Key, &str) -> bool,
    /// What a compaction snapshot keeps of the state it is about to write (REAL: all of it).
    pub snapshot_shape: fn(Snapshot) -> Snapshot,
    /// The order of the compaction steps (REAL: [`compaction::REAL_PLAN`]).
    pub compaction_plan: fn() -> &'static [Step],
}

impl Stages {
    pub const REAL: Stages = Stages {
        timestamp_ok: auth::timestamp_ok,
        body_hash: auth::sha256,
        signature_ok: auth::verify_ed25519,
        nonce_fresh: auth::nonce_fresh,
        may_heartbeat: auth::same_key,
        may_reregister: auth::same_key,
        liveness_fold: liveness::fold,
        config_signature_ok: auth::verify_ed25519,
        config_version_ok: config::version_is_newer,
        config_target_ok: config::target_matches,
        enrolled: auth::in_allowlist,
        may_list: auth::in_allowlist,
        read_auth: |verdict| verdict,
        record_rejection: |_, _| true,
        snapshot_shape: |s| s,
        compaction_plan: || compaction::REAL_PLAN,
    };
}

impl Default for Stages {
    fn default() -> Self {
        Stages::REAL
    }
}
