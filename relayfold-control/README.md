# relayfold-control

A fleet-management control plane for relayfold-based proxy instances — the Rust equivalent of
zentinel's `zentinel-control-plane` (Elixir/Phoenix, self-declared Beta). No Elixir/Phoenix here:
HTTP is `servelet` (`servelet-h1`/`-router`/`-client`), durability is `sangdb-log`, identity is the
dialkey Ed25519 keypair from `transport`.

## What it does (tested end-to-end)

**Every** call, `GET /instances` included, carries four headers: `x-relayfold-key` (the signer's
public key, z-base32), `x-relayfold-timestamp`, `x-relayfold-nonce`, `x-relayfold-signature` (hex
Ed25519 over a length-prefixed `(method, path, sha256(body), timestamp, nonce)`). The key is
`transport::dialkey::native_keypair(dialkey).public_key()` — see `auth::Signer` for the client
side. (`transport::dialkey::peer_id` returns the same public key since transport 0.2.0, the
pinned rev; up to 30fdaf7 it returned the secret seed.)

| Call | What it does | Rejections |
|---|---|---|
| `POST /register {id, addr, version}` | binds `id -> signer's public key`; **first register wins**; the signer must be **enrolled** | no/forged/tampered/stale/replayed signature **401**; signer not on the enrollment allowlist **403** `forbidden: key is not enrolled` (logged durably); an id bound to another key **403** |
| `POST /heartbeat {id, healthy, config_version}` | liveness; only the key bound at register may heartbeat | as above; wrong key **403**; unknown id **404** (never creates) |
| `GET /instances` | every instance; `healthy` = registered ∧ heartbeat within TTL ∧ last report healthy; the signer must be **enrolled** | no/forged/stale/replayed signature **401**; signer not enrolled **403** (same body as above) |
| `POST /config/push {target, config}` | admin-only; allocates the next `config_version`, signs one config per target with the control-plane key, delivers to `{addr}/relayfold-control/config` | non-admin key **403** |

### Enrollment

`ControlConfig::enrolled_keys` (env `RELAYFOLD_ENROLLED_KEYS`, comma-separated z-base32 public keys)
is the allowlist. **Empty means closed**: nobody may register or list. A register by a key that
is not on it is authenticated first (an unsigned or replayed request is a 401 and is *not*
logged, so the audit trail cannot be filled by unauthenticated callers), then refused with **403**
and the stable body `forbidden: key is not enrolled`, after appending an `EnrollmentRejected`
record (signer key, requested id cut to 128 bytes, time) to the durable log. The most recent 256
refusals are kept (replay and compaction both keep exactly the last 256).

*Why an allowlist and not a signed enrollment token?* The signed-request scheme already proves
possession of a key; the only missing fact is "is this key one we admit", which is a set
membership. A token would add a second signed object, a token signer (the admin key), an expiry
and a revocation story, and a new replay surface, to answer the same question. The allowlist is
the same shape as `RELAYFOLD_ADMIN_KEYS`, needs no new wire format, and is checked by one pure
function (`auth::in_allowlist`, the `Stages::enrolled` decision point). The price: changing the
list takes a restart, and "enrolled" is all-or-nothing (no per-id scoping, no expiry).

### Compaction

The log used to grow with every registration change and push, forever. `ControlPlane::compact()`
(automatically after `compact_after` records, default 10 000, env `RELAYFOLD_COMPACT_AFTER`,
`0` = never) writes a CRC-framed `control.snapshot` of the whole state — instances with their
bound keys, `config_version`, the audit trail, and the live **replay window** — then replaces the
log with a fresh one whose first record is `Epoch{n+1}`. Both steps are write-tmp, fsync, atomic
rename, directory fsync. `store::open` recovers from where a kill left things by comparing the
snapshot's `covers_epoch` with the log's epoch (see `compaction.rs` for the table), so
`replay(snapshot + tail) == replay(full log)` and a kill between the snapshot and the truncation
neither loses nor double-applies a record. A snapshot that is missing, damaged or from another
epoch **refuses to start** instead of being mistaken for an empty fleet. Compaction runs inside the
plane's critical section, so requests wait for it (a few ms; see the measurements in the PR).

* **Replay protection**: timestamp within `skew_secs` of the (injected) clock *and* not before
  process start; bounded nonce window keyed by `(key, nonce)`, recorded only after the signature
  verifies; a full window rejects (**429**) rather than evicting.
* **Signed config**: the instance-side check is the pure function `config::verify_config(body,
  control_public_key, this_instance_id, last_applied_version)`; it rejects unsigned, wrong-key,
  tampered, wrong-target and old-`config_version` configs.
* **Liveness** is a pure fold (`liveness::fold`: `Registered | Heartbeat | Expired(now)`), TTL
  configurable, expired at read time — no sweeper. After a restart nothing is healthy until it
  heartbeats again (heartbeats are not logged).
* **Durable**: an error-returning append path on `sangdb-log` (`fsync` per record, torn tail
  truncated on open). Registered ids, their **bound keys** and the `config_version` counter survive
  restart, as does the enrollment audit trail. A failed append is a 500 and nothing is applied.
  The log is bounded by snapshot + truncate compaction (above).
* **Checkable**: every decision point is a field of `stages::Stages` (including the enrollment and
  read gates, the audit switch, what a snapshot keeps and the **order of the compaction steps**);
  `conformance::check_all` is one function that judges `Stages::REAL` and any mutant of it
  (`tests/negative_controls.rs`), and it kills the process / cuts the power after every prefix of
  the compaction plan.

## What's out of scope (disclosed, not hidden)

* **Enrollment is a static allowlist.** Changing it takes a restart; there is no per-id scoping,
  expiry or runtime revocation. Removing a key stops it registering, re-registering and listing
  after the restart, but an instance already bound to that key **keeps heartbeating** (heartbeat
  checks the binding, not the allowlist). A key can also be enrolled and read-allowed in one
  breath: there is no separate "reader" tier.
* **A flood of refused registers is durable** by design (that is the audit); it is bounded by the
  replay window rate and by compaction (256 kept), but each refusal costs an fsync.
* **Plain HTTP**: requests are authenticated and configs integrity-protected, but nothing is
  encrypted — no TLS/mTLS.
* **The nonce window is persisted only at compaction.** Nonces accepted since the last snapshot
  are memory-only; a restart is covered for those by the not-before floor, which still lets a
  request stamped in the restart's own second (or within the skew in the future) through. An
  authenticated key can fill the bounded window (429s) until entries age out of the skew window.
* **Compaction pauses the plane** for the snapshot write and four fsyncs, and snapshots are full,
  not incremental. The snapshot includes up to `nonce_capacity` (4096) replay entries.
* **A compaction that fails after its snapshot is visible poisons the log**: every later register
  or push is a 500 until a restart finishes it (a failure before that, e.g. ENOSPC writing the
  tmp file, leaves the plane usable). Automatic compaction only logs its failure to stderr.
* The crash tests model POSIX durability (`compaction::LossyFs`: unsynced file data and
  un-directory-synced renames are lost); no real power was cut and no real disk exercised.
* **Not built on `fleet-lease`** — see `liveness.rs` for the exact reason.
* Multi-tenant dashboards, canary/staged rollout, an RBAC'd admin UI, auto-discovery.

## Run it

```
RELAYFOLD_CONTROL_DIALKEY=<secret> RELAYFOLD_ADMIN_KEYS=<z32 pubkey>[,...] \
  RELAYFOLD_ENROLLED_KEYS=<z32 pubkey>[,...] \
  cargo run --bin relayfold-control -- 0.0.0.0:8088 ./data
```

`RELAYFOLD_ENROLLED_KEYS` must list every instance (and operator) key that may register or read
the fleet; unset means nobody can, and the binary says so on stderr. `RELAYFOLD_TTL_SECS`
overrides the 30 s heartbeat TTL; `RELAYFOLD_COMPACT_AFTER` the compaction threshold (default
10 000 records, `0` disables). `RELAYFOLD_CONTROL_DIALKEY` signs pushed
configs; agents verify with `auth::public_key_of(dialkey)`. The data dir holds `control.log`; a
data dir written by the previous (sangdb `.seed` dump) version is ignored — that version bound no
keys, so there is nothing to migrate.

## Build and test

`cargo test -p relayfold-control` from the workspace root. `servelet`'s `Cargo.toml` takes its
`hashtrinity` from the private GitLab mirror (`gitlab.tas.twn.network/twn/ptyktos/hashtrinity`),
so the machine running cargo needs git access to it (an `ssh` identity plus
`url.git@gitlab.tas.twn.network:.insteadOf https://gitlab.tas.twn.network/`). `[patch]` and source
replacement do not avoid this.
