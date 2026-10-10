# relayfold-control

A fleet-management control plane for relayfold-based proxy instances — the Rust equivalent of
zentinel's `zentinel-control-plane` (Elixir/Phoenix, self-declared Beta). No Elixir/Phoenix here:
HTTP is `servelet` (`servelet-h1`/`-router`/`-client`), durability is `sangdb-log`, identity is the
dialkey Ed25519 keypair from `transport`.

## What it does (tested end-to-end)

Every **mutating** call carries four headers: `x-relayfold-key` (the signer's public key,
z-base32), `x-relayfold-timestamp`, `x-relayfold-nonce`, `x-relayfold-signature` (hex Ed25519 over
a length-prefixed `(method, path, sha256(body), timestamp, nonce)`). The key is
`transport::dialkey::native_keypair(dialkey).public_key()` — see `auth::Signer` for the client
side. (`transport::dialkey::peer_id` returns the same public key since transport 0.2.0, the
pinned rev; up to 30fdaf7 it returned the secret seed.)

| Call | What it does | Rejections |
|---|---|---|
| `POST /register {id, addr, version}` | binds `id -> signer's public key`; **first register wins** | no/forged/tampered/stale/replayed signature **401**; an id bound to another key **403** |
| `POST /heartbeat {id, healthy, config_version}` | liveness; only the key bound at register may heartbeat | as above; wrong key **403**; unknown id **404** (never creates) |
| `GET /instances` | every instance; `healthy` = registered ∧ heartbeat within TTL ∧ last report healthy | — (unauthenticated, read-only) |
| `POST /config/push {target, config}` | admin-only; allocates the next `config_version`, signs one config per target with the control-plane key, delivers to `{addr}/relayfold-control/config` | non-admin key **403** |

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
  restart. A failed append is a 500 and nothing is applied.
* **Checkable**: every decision point is a field of `stages::Stages`; `conformance::check_all` is
  one function that judges `Stages::REAL` and any mutant of it (`tests/negative_controls.rs`).

## What's out of scope (disclosed, not hidden)

* **No enrollment policy.** Any keypair can register an *unclaimed* id; first-register-wins
  protects a claimed id, not the namespace.
* **`GET /instances` is unauthenticated.** **Plain HTTP**: requests are authenticated and configs
  integrity-protected, but nothing is encrypted — no TLS/mTLS.
* **The nonce window is memory-only** (restart covered by the not-before floor). An authenticated
  key can fill the bounded window (429s) until entries age out of the skew window.
* **No log compaction** (the log holds registrations and pushes only; `sangdb-log` has `compact`
  but it is not wired).
* **Not built on `fleet-lease`** — see `liveness.rs` for the exact reason.
* Multi-tenant dashboards, canary/staged rollout, an RBAC'd admin UI, auto-discovery.

## Run it

```
RELAYFOLD_CONTROL_DIALKEY=<secret> RELAYFOLD_ADMIN_KEYS=<z32 pubkey>[,...] \
  cargo run --bin relayfold-control -- 0.0.0.0:8088 ./data
```

`RELAYFOLD_TTL_SECS` overrides the 30 s heartbeat TTL. `RELAYFOLD_CONTROL_DIALKEY` signs pushed
configs; agents verify with `auth::public_key_of(dialkey)`. The data dir holds `control.log`; a
data dir written by the previous (sangdb `.seed` dump) version is ignored — that version bound no
keys, so there is nothing to migrate.

## Build and test

`cargo test -p relayfold-control` from the workspace root. `servelet`'s `Cargo.toml` takes its
`hashtrinity` from the private GitLab mirror (`gitlab.tas.twn.network/twn/ptyktos/hashtrinity`),
so the machine running cargo needs git access to it (an `ssh` identity plus
`url.git@gitlab.tas.twn.network:.insteadOf https://gitlab.tas.twn.network/`). `[patch]` and source
replacement do not avoid this.
