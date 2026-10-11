//! Request authentication: every call (reads included) is signed by the caller's dialkey-derived
//! Ed25519 key. The identity is `transport::dialkey::native_keypair(dialkey).public_key()`, which is also
//! what `transport::dialkey::peer_id(dialkey)` returns since transport 0.2.0 (d4745b9). Up to
//! 30fdaf7 `peer_id` returned the 32-byte secret *seed* instead, so this crate never used it;
//! it is kept off the wire path (`public_key_of` calls `native_keypair`) and tests pin both to the
//! same golden vector. The seed itself must never be sent, logged or stored.
//!
//! # Wire format
//!
//! Four headers on every request, `GET /instances` included:
//!
//! | header | value |
//! |---|---|
//! | `x-relayfold-key` | the signer's public key, z-base32 (`PublicKey::to_z32`) |
//! | `x-relayfold-timestamp` | unix seconds, decimal |
//! | `x-relayfold-nonce` | 8..=64 chars of `[0-9A-Za-z_-]`, unique per request per key |
//! | `x-relayfold-signature` | lowercase hex of the 64-byte Ed25519 signature |
//!
//! The signature covers a length-prefixed encoding of
//! `(domain, method, path, sha256(body), timestamp, nonce)` — see [`request_message`]. The
//! encoding is length-prefixed rather than delimiter-joined so no field can bleed into the next.
//!
//! # Replay protection
//!
//! A request is accepted only if (in this order, each a [`Stages`] decision point):
//! 1. its timestamp is within `skew` of the injected clock **and** not before this verifier was
//!    created (`not_before`) — the nonce window is memory-only, so without the second rule a
//!    request captured before a restart could be replayed once after it;
//! 2. its signature verifies;
//! 3. only then is its nonce recorded: an unauthenticated caller can never fill the window. A
//!    nonce already in the window is a replay; a window full of still-live nonces **rejects**
//!    ([`AuthError::WindowFull`]) rather than evicting, because evicting a live nonce reopens
//!    replay inside the skew window.
//!
//! Entries leave the window when `now > ts + skew`, i.e. exactly when a request carrying that
//! timestamp would already fail step 1 — so pruning can never re-admit a replay.

use crate::clock::Clock;
use crate::stages::Stages;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use transport::signed_dns::{Keypair, PublicKey};

pub type Key = [u8; 32];

pub const H_KEY: &str = "x-relayfold-key";
pub const H_TIMESTAMP: &str = "x-relayfold-timestamp";
pub const H_NONCE: &str = "x-relayfold-nonce";
pub const H_SIGNATURE: &str = "x-relayfold-signature";

const REQUEST_DOMAIN: &[u8] = b"relayfold-control/request/v1";

// ── hex / z32 helpers ───────────────────────────────────────────────────────────────────────

pub fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 0xf) as usize] as char);
    }
    s
}

pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() & 1 == 1 || !s.is_ascii() {
        return None;
    }
    s.as_bytes()
        .chunks(2)
        .map(|p| {
            let hi = (p[0] as char).to_digit(16)?;
            let lo = (p[1] as char).to_digit(16)?;
            Some((hi * 16 + lo) as u8)
        })
        .collect()
}

/// The dialkey-derived public identity. Safe to publish.
pub fn public_key_of(dialkey: &str) -> Key {
    *transport::dialkey::native_keypair(dialkey)
        .public_key()
        .as_bytes()
}

pub fn key_to_z32(key: &Key) -> String {
    PublicKey::from_bytes(key)
        .map(|k| k.to_z32())
        .unwrap_or_default()
}

pub fn key_from_z32(s: &str) -> Option<Key> {
    PublicKey::from_z32(s).ok().map(|k| *k.as_bytes())
}

/// Real signature check: Ed25519 over `msg` under `key`.
pub fn verify_ed25519(key: &Key, msg: &[u8], sig: &[u8; 64]) -> bool {
    PublicKey::from_bytes(key)
        .map(|k| k.verify(msg, sig))
        .unwrap_or(false)
}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    hashtrinity::crypto::sha256(bytes)
}

fn put(out: &mut Vec<u8>, field: &[u8]) {
    out.extend_from_slice(&(field.len() as u32).to_be_bytes());
    out.extend_from_slice(field);
}

/// The exact bytes a request signature covers.
pub fn request_message(
    method: &str,
    path: &str,
    body_hash: &[u8; 32],
    timestamp: i64,
    nonce: &str,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(160);
    put(&mut out, REQUEST_DOMAIN);
    put(&mut out, method.as_bytes());
    put(&mut out, path.as_bytes());
    put(&mut out, body_hash);
    put(&mut out, &timestamp.to_be_bytes());
    put(&mut out, nonce.as_bytes());
    out
}

// ── the real decision points (wired into `Stages::REAL`) ───────────────────────────────────────

/// Accept `ts` iff it is within `skew` of `now` (both directions) and not before `not_before`.
pub fn timestamp_ok(now: i64, ts: i64, skew: i64, not_before: i64) -> bool {
    ts >= not_before && now.saturating_sub(ts).abs() <= skew
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NonceVerdict {
    Fresh,
    Replay,
    Full,
}

/// One remembered nonce: what the replay window holds, and what a compaction snapshot persists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NonceEntry {
    pub key: Key,
    pub nonce: String,
    pub ts: i64,
}

/// Bounded, deterministic replay window: `(key, nonce) -> timestamp`.
#[derive(Debug)]
pub struct NonceWindow {
    seen: BTreeMap<(Key, String), i64>,
    capacity: usize,
    skew: i64,
}

impl NonceWindow {
    pub fn new(capacity: usize, skew: i64) -> Self {
        NonceWindow {
            seen: BTreeMap::new(),
            capacity,
            skew,
        }
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Every entry still live at `now` (pruned first), in key order.
    pub fn live_entries(&mut self, now: i64) -> Vec<NonceEntry> {
        self.prune(now);
        self.seen
            .iter()
            .map(|((key, nonce), ts)| NonceEntry {
                key: *key,
                nonce: nonce.clone(),
                ts: *ts,
            })
            .collect()
    }

    /// Re-admit persisted entries that are still live at `now`. Never rejects one for capacity:
    /// dropping a live nonce reopens replay, so an over-full window just refuses new nonces
    /// until entries age out.
    pub fn restore(&mut self, entries: &[NonceEntry], now: i64) {
        for e in entries {
            self.seen.insert((e.key, e.nonce.clone()), e.ts);
        }
        self.prune(now);
    }

    /// Drop entries a request could no longer replay anyway (`now > ts + skew`).
    pub fn prune(&mut self, now: i64) {
        let skew = self.skew;
        self.seen.retain(|_, ts| now <= ts.saturating_add(skew));
    }

    /// Record `(key, nonce)` if unseen. Never evicts a live entry.
    pub fn check_and_insert(&mut self, key: &Key, nonce: &str, ts: i64, now: i64) -> NonceVerdict {
        self.prune(now);
        let k = (*key, nonce.to_string());
        if self.seen.contains_key(&k) {
            return NonceVerdict::Replay;
        }
        if self.seen.len() >= self.capacity {
            return NonceVerdict::Full;
        }
        self.seen.insert(k, ts);
        NonceVerdict::Fresh
    }
}

/// The real stage: see [`NonceWindow::check_and_insert`].
pub fn nonce_fresh(
    window: &mut NonceWindow,
    key: &Key,
    nonce: &str,
    ts: i64,
    now: i64,
) -> NonceVerdict {
    window.check_and_insert(key, nonce, ts, now)
}

/// Real enrollment / read-access decision: is `key` on the allowlist? An empty list admits
/// nobody (closed by default).
pub fn in_allowlist(list: &[Key], key: &Key) -> bool {
    list.contains(key)
}

/// Only the key bound at register may heartbeat that id.
pub fn same_key(bound: &Key, caller: &Key) -> bool {
    bound == caller
}

// ── verifier ───────────────────────────────────────────────────────────────────────────────

/// What the headers said, before any check.
#[derive(Clone, Debug, Default)]
pub struct RawAuth {
    pub key: Option<String>,
    pub timestamp: Option<String>,
    pub nonce: Option<String>,
    pub signature: Option<String>,
}

/// One request as the verifier sees it.
#[derive(Clone, Copy, Debug)]
pub struct SignedCall<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub body: &'a [u8],
    pub auth: &'a RawAuth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthError {
    Missing(&'static str),
    Malformed(&'static str),
    BadTimestamp,
    BadSignature,
    Replay,
    WindowFull,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::Missing(h) => write!(f, "missing header {h}"),
            AuthError::Malformed(h) => write!(f, "malformed header {h}"),
            AuthError::BadTimestamp => f.write_str("timestamp outside the accepted window"),
            AuthError::BadSignature => f.write_str("signature does not verify"),
            AuthError::Replay => f.write_str("nonce already used"),
            AuthError::WindowFull => f.write_str("replay window full, retry later"),
        }
    }
}

impl std::error::Error for AuthError {}

fn valid_nonce(n: &str) -> bool {
    (8..=64).contains(&n.len())
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub struct Verifier {
    stages: Stages,
    skew: i64,
    not_before: i64,
    window: NonceWindow,
}

impl Verifier {
    /// `started_at` becomes the `not_before` floor: requests stamped earlier are rejected.
    pub fn new(stages: Stages, skew_secs: i64, nonce_capacity: usize, started_at: i64) -> Self {
        Verifier {
            stages,
            skew: skew_secs,
            not_before: started_at,
            window: NonceWindow::new(nonce_capacity, skew_secs),
        }
    }

    pub fn window(&self) -> &NonceWindow {
        &self.window
    }

    /// The nonces a request could still replay at `now`: what a snapshot must persist.
    pub fn live_nonces(&mut self, now: i64) -> Vec<NonceEntry> {
        self.window.live_entries(now)
    }

    /// Seed the window from a snapshot (see [`NonceWindow::restore`]).
    pub fn restore_nonces(&mut self, entries: &[NonceEntry], now: i64) {
        self.window.restore(entries, now);
    }

    /// Authenticate one call against `clock`; on success returns the signer's public key.
    pub fn authenticate(
        &mut self,
        clock: &dyn Clock,
        call: &SignedCall<'_>,
    ) -> Result<Key, AuthError> {
        let now = clock.now_unix();
        let a = call.auth;
        let key_s = a.key.as_deref().ok_or(AuthError::Missing(H_KEY))?;
        let ts_s = a
            .timestamp
            .as_deref()
            .ok_or(AuthError::Missing(H_TIMESTAMP))?;
        let nonce = a.nonce.as_deref().ok_or(AuthError::Missing(H_NONCE))?;
        let sig_s = a
            .signature
            .as_deref()
            .ok_or(AuthError::Missing(H_SIGNATURE))?;

        let key = key_from_z32(key_s).ok_or(AuthError::Malformed(H_KEY))?;
        let ts: i64 = ts_s
            .parse()
            .map_err(|_| AuthError::Malformed(H_TIMESTAMP))?;
        if !valid_nonce(nonce) {
            return Err(AuthError::Malformed(H_NONCE));
        }
        let sig: [u8; 64] = hex_decode(sig_s)
            .and_then(|v| v.try_into().ok())
            .ok_or(AuthError::Malformed(H_SIGNATURE))?;

        if !(self.stages.timestamp_ok)(now, ts, self.skew, self.not_before) {
            return Err(AuthError::BadTimestamp);
        }
        let body_hash = (self.stages.body_hash)(call.body);
        let msg = request_message(call.method, call.path, &body_hash, ts, nonce);
        if !(self.stages.signature_ok)(&key, &msg, &sig) {
            return Err(AuthError::BadSignature);
        }
        match (self.stages.nonce_fresh)(&mut self.window, &key, nonce, ts, now) {
            NonceVerdict::Fresh => Ok(key),
            NonceVerdict::Replay => Err(AuthError::Replay),
            NonceVerdict::Full => Err(AuthError::WindowFull),
        }
    }
}

// ── client side ────────────────────────────────────────────────────────────────────────────

/// The headers a signed request carries.
#[derive(Clone, Debug)]
pub struct AuthHeaders {
    pub key: String,
    pub timestamp: String,
    pub nonce: String,
    pub signature: String,
}

impl AuthHeaders {
    pub fn pairs(&self) -> [(&'static str, String); 4] {
        [
            (H_KEY, self.key.clone()),
            (H_TIMESTAMP, self.timestamp.clone()),
            (H_NONCE, self.nonce.clone()),
            (H_SIGNATURE, self.signature.clone()),
        ]
    }

    pub fn to_raw(&self) -> RawAuth {
        RawAuth {
            key: Some(self.key.clone()),
            timestamp: Some(self.timestamp.clone()),
            nonce: Some(self.nonce.clone()),
            signature: Some(self.signature.clone()),
        }
    }
}

/// What a relayfold instance (or an operator) uses to sign its calls to the control plane.
pub struct Signer {
    keypair: Keypair,
    public: Key,
    nonce_salt: [u8; 32],
    counter: AtomicU64,
}

impl Signer {
    pub fn from_dialkey(dialkey: &str) -> Self {
        let keypair = transport::dialkey::native_keypair(dialkey);
        let public = *keypair.public_key().as_bytes();
        // Per-process salt so two processes sharing a dialkey do not collide on counter values.
        let mut salt_in = public.to_vec();
        salt_in.extend_from_slice(&std::process::id().to_be_bytes());
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        salt_in.extend_from_slice(&nanos.to_be_bytes());
        Signer {
            keypair,
            public,
            nonce_salt: sha256(&salt_in),
            counter: AtomicU64::new(0),
        }
    }

    pub fn public_key(&self) -> Key {
        self.public
    }

    /// A nonce unique to this signer for the life of the process (and, via the salt, across
    /// processes sharing the dialkey with overwhelming probability).
    pub fn next_nonce(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let mut input = self.nonce_salt.to_vec();
        input.extend_from_slice(&n.to_be_bytes());
        hex_encode(&sha256(&input)[..16])
    }

    pub fn sign(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        timestamp: i64,
        nonce: &str,
    ) -> AuthHeaders {
        let msg = request_message(method, path, &sha256(body), timestamp, nonce);
        AuthHeaders {
            key: key_to_z32(&self.public),
            timestamp: timestamp.to_string(),
            nonce: nonce.to_string(),
            signature: hex_encode(&self.keypair.sign(&msg)),
        }
    }

    /// Sign with the next fresh nonce.
    pub fn sign_now(&self, method: &str, path: &str, body: &[u8], timestamp: i64) -> AuthHeaders {
        self.sign(method, path, body, timestamp, &self.next_nonce())
    }

    /// Sign arbitrary bytes under the same key (used by the config-push signer).
    pub fn sign_raw(&self, msg: &[u8]) -> [u8; 64] {
        self.keypair.sign(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;

    fn verifier(clock: &ManualClock) -> Verifier {
        Verifier::new(Stages::REAL, 30, 8, clock.now_unix())
    }

    fn call<'a>(raw: &'a RawAuth, body: &'a [u8]) -> SignedCall<'a> {
        SignedCall {
            method: "POST",
            path: "/register",
            body,
            auth: raw,
        }
    }

    #[test]
    fn hex_round_trips_and_rejects_garbage() {
        let bytes = [0u8, 1, 0xab, 0xff];
        assert_eq!(hex_decode(&hex_encode(&bytes)).unwrap(), bytes);
        assert!(hex_decode("abc").is_none());
        assert!(hex_decode("zz").is_none());
    }

    #[test]
    fn the_public_identity_is_not_the_secret_seed() {
        // `peer_id(dialkey)` is the PUBLIC key since transport 0.2.0. `Keypair` has no seed
        // accessor, so recompute the documented seed, SHA-256(dialkey ++ ":pkarr"), and prove it
        // IS the seed of `native_keypair`: otherwise the `assert_ne!` below would be vacuous.
        let dk = "auth-test-dialkey";
        let seed = sha256(format!("{dk}:pkarr").as_bytes());
        let public = public_key_of(dk);
        assert_eq!(
            *Keypair::from_secret_key(&seed).public_key().as_bytes(),
            public,
            "the recomputed seed is the seed of native_keypair"
        );
        let peer = transport::dialkey::peer_id(dk);
        assert_eq!(peer, public, "peer_id is the public key");
        assert_ne!(peer, seed, "peer_id is not the secret seed");
        assert_ne!(public, seed);
    }

    #[test]
    fn a_literal_test_dialkey_has_a_pinned_public_key() {
        // Golden vector (literal test dialkey, not a credential); the same one transport pins.
        const GOLDEN: &str = "30966ac5fa1bdd49aec9b9101bf63359704d01dc00597f934f7d7bca9bc5c47b";
        assert_eq!(hex_encode(&public_key_of("g17-test-dialkey")), GOLDEN);
        assert_eq!(
            hex_encode(&transport::dialkey::peer_id("g17-test-dialkey")),
            GOLDEN
        );
        assert_eq!(
            hex_encode(&Signer::from_dialkey("g17-test-dialkey").public_key()),
            GOLDEN
        );
    }

    #[test]
    fn a_signed_request_verifies_and_reveals_the_signer() {
        let clock = ManualClock::new(1_000);
        let s = Signer::from_dialkey("auth-test-a");
        let h = s.sign_now("POST", "/register", b"{}", 1_000).to_raw();
        let mut v = verifier(&clock);
        assert_eq!(v.authenticate(&clock, &call(&h, b"{}")), Ok(s.public_key()));
    }

    #[test]
    fn tampering_with_any_signed_field_breaks_the_signature() {
        let clock = ManualClock::new(1_000);
        let s = Signer::from_dialkey("auth-test-a");
        let h = s.sign_now("POST", "/register", b"{}", 1_000).to_raw();
        for (method, path, body) in [
            ("PUT", "/register", &b"{}"[..]),
            ("POST", "/heartbeat", &b"{}"[..]),
            ("POST", "/register", &b"{ }"[..]),
        ] {
            let mut v = verifier(&clock);
            let c = SignedCall {
                method,
                path,
                body,
                auth: &h,
            };
            assert_eq!(v.authenticate(&clock, &c), Err(AuthError::BadSignature));
        }
    }

    #[test]
    fn a_replayed_request_is_rejected_the_second_time() {
        let clock = ManualClock::new(1_000);
        let s = Signer::from_dialkey("auth-test-a");
        let h = s.sign_now("POST", "/register", b"{}", 1_000).to_raw();
        let mut v = verifier(&clock);
        assert!(v.authenticate(&clock, &call(&h, b"{}")).is_ok());
        assert_eq!(
            v.authenticate(&clock, &call(&h, b"{}")),
            Err(AuthError::Replay)
        );
    }

    #[test]
    fn stale_and_future_timestamps_are_rejected() {
        let clock = ManualClock::new(1_000);
        let s = Signer::from_dialkey("auth-test-a");
        let old = s.sign_now("POST", "/register", b"{}", 1_000).to_raw();
        let future = s.sign_now("POST", "/register", b"{}", 1_031).to_raw();
        let edge = s.sign_now("POST", "/register", b"{}", 1_030).to_raw();
        let mut v = verifier(&clock);
        assert!(
            v.authenticate(&clock, &call(&edge, b"{}")).is_ok(),
            "skew edge is inclusive"
        );
        assert_eq!(
            v.authenticate(&clock, &call(&future, b"{}")),
            Err(AuthError::BadTimestamp)
        );
        clock.advance(31);
        assert_eq!(
            v.authenticate(&clock, &call(&old, b"{}")),
            Err(AuthError::BadTimestamp)
        );
    }

    #[test]
    fn a_request_stamped_before_the_verifier_started_is_rejected() {
        let clock = ManualClock::new(1_000);
        let s = Signer::from_dialkey("auth-test-a");
        let pre_boot = s.sign_now("POST", "/register", b"{}", 999).to_raw();
        let mut v = verifier(&clock);
        assert_eq!(
            v.authenticate(&clock, &call(&pre_boot, b"{}")),
            Err(AuthError::BadTimestamp)
        );
    }

    #[test]
    fn missing_and_malformed_headers_are_rejected_without_touching_the_window() {
        let clock = ManualClock::new(1_000);
        let s = Signer::from_dialkey("auth-test-a");
        let mut v = verifier(&clock);
        assert_eq!(
            v.authenticate(&clock, &call(&RawAuth::default(), b"{}")),
            Err(AuthError::Missing(H_KEY))
        );
        let mut h = s.sign_now("POST", "/register", b"{}", 1_000).to_raw();
        h.nonce = Some("short".into());
        assert_eq!(
            v.authenticate(&clock, &call(&h, b"{}")),
            Err(AuthError::Malformed(H_NONCE))
        );
        assert!(v.window().is_empty());
    }

    #[test]
    fn a_forged_signature_never_enters_the_nonce_window() {
        let clock = ManualClock::new(1_000);
        let a = Signer::from_dialkey("auth-test-a");
        let b = Signer::from_dialkey("auth-test-b");
        let mut h = a.sign_now("POST", "/register", b"{}", 1_000);
        h.signature = b
            .sign("POST", "/register", b"{}", 1_000, &h.nonce)
            .signature;
        let mut v = verifier(&clock);
        assert_eq!(
            v.authenticate(&clock, &call(&h.to_raw(), b"{}")),
            Err(AuthError::BadSignature)
        );
        assert!(
            v.window().is_empty(),
            "unauthenticated callers must not fill the window"
        );
    }

    #[test]
    fn a_full_window_rejects_instead_of_evicting() {
        let clock = ManualClock::new(1_000);
        let s = Signer::from_dialkey("auth-test-a");
        let mut v = Verifier::new(Stages::REAL, 30, 3, 1_000);
        let reqs: Vec<RawAuth> = (0..4)
            .map(|_| s.sign_now("POST", "/register", b"{}", 1_000).to_raw())
            .collect();
        for r in &reqs[..3] {
            assert!(v.authenticate(&clock, &call(r, b"{}")).is_ok());
        }
        assert_eq!(
            v.authenticate(&clock, &call(&reqs[3], b"{}")),
            Err(AuthError::WindowFull)
        );
        // The first nonce is still remembered: it was not evicted to make room.
        assert_eq!(
            v.authenticate(&clock, &call(&reqs[0], b"{}")),
            Err(AuthError::Replay)
        );
    }

    #[test]
    fn the_window_prunes_exactly_when_the_timestamp_check_would_reject() {
        let clock = ManualClock::new(1_000);
        let s = Signer::from_dialkey("auth-test-a");
        let mut v = Verifier::new(Stages::REAL, 30, 3, 1_000);
        let h = s.sign_now("POST", "/register", b"{}", 1_000).to_raw();
        assert!(v.authenticate(&clock, &call(&h, b"{}")).is_ok());
        clock.advance(30);
        assert_eq!(
            v.authenticate(&clock, &call(&h, b"{}")),
            Err(AuthError::Replay),
            "ts+skew is still live"
        );
        clock.advance(1);
        assert_eq!(
            v.authenticate(&clock, &call(&h, b"{}")),
            Err(AuthError::BadTimestamp)
        );
        let fresh = s.sign_now("POST", "/register", b"{}", 1_031).to_raw();
        assert!(v.authenticate(&clock, &call(&fresh, b"{}")).is_ok());
        assert_eq!(
            v.window().len(),
            1,
            "the 1000-stamped entry was pruned, only the fresh one remains"
        );
    }
}
