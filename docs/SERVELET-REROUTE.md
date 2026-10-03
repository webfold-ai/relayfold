# relayfold → servelet reroute

Unit `reroute-relayfold`, date 2026-10-03, branch `yokoszn/servelet-reroute`.
Scope: every crate in this repo that imported `biota_http_server::` — `relayfold-control`,
`relayfold-helm/proxy-runtime`. Read first: `~/projects/servelet/README.md` (nine rules + crate map),
`~/projects/servelet/docs/MIGRATION.md`.

## OUTCOME: STOPPED AT BASELINE. No source or Cargo.toml change was made.

The repo does not build at baseline (relayfold-control, 2 × E0277, hashtrinity version split). Per the
reroute rules ("if the repo does not build at baseline, record the exact error and STOP at that boundary"),
the reroute was not attempted. Sections 3–5 are therefore **NOT RUN**, not "skipped for time".

A second, independent blocker was found in the census (section 2): several of the imported items have
**no servelet home yet**, so even a green baseline would not have produced a complete reroute today.

## 1. Census (`git grep -n -E 'biota_http_server::|biota-http-server'`, read-only)

53 matching lines total. Excluding the generated `.harness/` (2 lines, never committed) and prose-only
mentions in `README.md` / `relayfold-control/README.md` / `relayfold-helm/README.md` /
`relayfold/benchmarks/*.md` / `relayfold-helm/chart/relayfold-proxy/Chart.yaml` /
`.github/workflows/ci.yml` (10 lines, no code), there are **41 code references** across 4 files.

Dependency declarations (2 real):
- `relayfold-control/Cargo.toml:11` — `biota-http-server = { git = "https://github.com/Ptyktos/biota", package = "biota-http-server" }`
- `relayfold-helm/proxy-runtime/Cargo.toml:15` — `biota-http-server = { git = "https://github.com/Ptyktos/biota" }`
(`relayfold-control/Cargo.toml:5` and `relayfold-helm/proxy-runtime/Cargo.toml:5` mention the name in
`description` only; `Cargo.toml:13` mentions it in a `[patch]` comment.)

### Distinct imported items, grouped, with file:line counts

| # | item | category | occurrences | files (count) | servelet home (README crate map) | verdict |
|---|---|---|---|---|---|---|
| 1 | `Request` | Request | 3 | `app.rs:13`, `proxy-runtime/src/main.rs:15`, `tests/integration.rs:171` | `servelet-core` | **has home** — present in spine |
| 2 | `Response` (+ `::new`, `::ok`) | Response | 5 | `app.rs:13`, `dto.rs:64,73`, `proxy-runtime/src/main.rs:15`, `tests/integration.rs:176` | `servelet-core` | **has home** — present |
| 3 | `StatusCode` (+ `::BAD_REQUEST`, `::INTERNAL_SERVER_ERROR`, `::NOT_FOUND`) | Response | 9 | `app.rs:139,145,156,162,168,175,185,191`; `tests/integration.rs:140` | `servelet-core` | **has home** — present |
| 4 | `Router` | Router | 4 | `app.rs:201,202`, `tests/integration.rs:169`, `proxy-runtime/src/main.rs:15` | `servelet-router` | **has home** — present |
| 5 | `serve` | serve | 5 | `relayfold-control/src/main.rs:14`, `tests/integration.rs:21,181`, `proxy-runtime/src/main.rs:224` | `servelet-h1` | **NO EXPORTED ITEM** — see B2 |
| 6 | `serve_tls` | serve/TLS | 1 | `proxy-runtime/src/main.rs:221` | `servelet-h1` + `servelet-tls` | **NO EXPORTED ITEM** — see B2 |
| 7 | `tls_server_config` | TLS | 1 | `proxy-runtime/src/main.rs:122` | `servelet-tls` | **NOT RE-EXPORTED** — see B2 |
| 8 | `Client` (+ `Client::new`, `.get(url).send().await`, `.text().await`, `.status()`) | client | 7 | `app.rs:13`, `tests/integration.rs:50,87,132,161,228`, `proxy-runtime/src/main.rs:35` | `servelet-client` | **CRATE DOES NOT EXIST** — see B3 |
| 9 | `LoadBalance` | proxy | 1 | `proxy-runtime/src/main.rs:15` | *no crate in the map* | **NO HOME** — see B4 |
| 10 | `Proxy` | proxy | 1 | `proxy-runtime/src/main.rs:15` | *no crate in the map* | **NO HOME** — see B4 |
| 11 | `Upstream` | proxy | 1 | `proxy-runtime/src/main.rs:15` | *no crate in the map* | **NO HOME** — see B4 |
| 12 | `actix::Json` | other (extractor) | 1 | `app.rs:12` | *no crate in the map* | **NO HOME** — see B5 |
| 13 | `actix::Responder` | other (extractor) | 1 | `app.rs:12` | *no crate in the map* | **NO HOME** — see B5 |

Doc-comment-only mentions, not imports (recorded so the count is auditable):
`relayfold-control/src/dto.rs:2` (`biota_http_server::Json<T>` in a comment),
`relayfold-control/src/lib.rs:1`, `relayfold-control/tests/integration.rs:1,163` (prose).

### Census note: `relayfold-bench` does not exist in this repo

`docs/MIGRATION.md` lists "relayfold-bench (3)" as part of relayfold. At this worktree's HEAD there is no
crate, directory or `Cargo.toml` named `relayfold-bench`; `relayfold/benchmarks/` holds only markdown and
JSON fixtures (`git ls-files | grep -i bench` → 10 files, all `.md`/`.json`). Nothing to reroute there.
The census row appears stale or refers to a branch not on this origin.

## 2. Blockers found

### B1 (fatal, blocks everything): the repo does not build at baseline

`relayfold-control` fails to compile with two `E0277` errors — the `hashtrinity` sealed-trait chain
`CollapseElem` → `Pod` is not implemented for `InstanceRecord` because **two different versions of
`hashtrinity` are in the graph**:

```
error[E0277]: the trait bound `InstanceRecord: hashtrinity::codec::web::CollapseElem` is not satisfied
   --> relayfold-control/src/model.rs:23:16
    |
 23 | impl Model for InstanceRecord {
    |                ^^^^^^^^^^^^^^ unsatisfied trait bound
note: there are multiple different versions of crate `hashtrinity` in the dependency graph
   --> /home/ctown/.cargo/git/checkouts/hashtrinity-d2be98f3616648ef/c6746bd/hashtrinity/src/codec.rs:7526:5
    |                 this is the expected trait
   --> /home/ctown/.cargo/git/checkouts/hashtrinity-bba642a8e3975ddb/2c36b99/src/codec.rs:7528:5
    |                 this is the trait that was imported
error[E0277]: the trait bound `InstanceRecord: hashtrinity::codec::web::Pod` is not satisfied
   --> relayfold-control/src/model.rs:13:1
error: could not compile `relayfold-control` (lib) due to 2 previous errors
error: could not compile `relayfold-control` (lib test) due to 2 previous errors
```

Proven source of the split (`cargo tree -p relayfold-control --prefix none | sort -u`):

```
hashtrinity v0.1.0 (https://github.com/Ptyktos/hashtrinity?rev=2c36b995c82c073c71df7c28b14c62665d62cbf0#2c36b995)   <- relayfold-control's own pin
hashtrinity v0.1.0 (https://gitlab.tas.twn.network/twn/ptyktos/hashtrinity.git?branch=main#c6746bd6)                <- via sangdb
hashtrinity_derive v0.1.0 (…?rev=2c36b995…)   /   hashtrinity_derive v0.1.0 (gitlab…?branch=main#c6746bd6)
```

`sangdb` resolves `hashtrinity` from the GitLab mirror (`main`, `c6746bd6`); `relayfold-control/Cargo.toml:9`
pins the GitHub copy at rev `2c36b995`. The two `Pod` impls are in different crates, so the sealed trait
cannot be satisfied across them. This breakage predates any servelet work and is **not** servelet's doing.

**Relevant to the reroute:** servelet's own pin (spine `Cargo.toml:19`, with the comment "Hard rule 5: the
GitLab mirror, branch `main`, exactly as nexus/Cargo.toml has it — the GitHub copy does not unify with
sangdb (proven there)") is the GitLab `main` copy — i.e. the `c6746bd6` side. So a future reroute *would*
have to change relayfold-control's `hashtrinity` line from the GitHub rev to the GitLab mirror to make the
graph unify. Per the rules that pin stays exactly as the repo had it and this is **reported, not guessed**.

### B2: `servelet-h1` and `servelet-tls` are placeholders with no exports

At spine HEAD `32cc03362b6941fc0a84e250fa44d105c06b674f`:
- `crates/servelet-h1/src/lib.rs` is the single line `// placeholder`. `serve` and `serve_tls` do not
  exist as items anywhere in the crate (`wire.rs` = 308 lines of wire codec, `graceful.rs` = 94 lines,
  no `pub use`).
- `crates/servelet-tls/src/lib.rs` is also `// placeholder`. `tls_server_config` *is* written
  (`crates/servelet-tls/src/tls.rs:63`) but is **not re-exported**, so it is unreachable from outside.

So items 5, 6 and 7 of the census have a crate in the map but no usable exported item yet.

### B3: `servelet-client` does not exist

`Client` is used 6 times in this repo (`Client::new()` in 5 test sites plus `app.rs`, and an outbound
`Client::new().get(url).send().await.ok()?.text().await.ok()?` in `proxy-runtime/src/main.rs:35`).
`servelet-client` is in the README crate map but is **not** in the spine — the crate map's own "Status"
line says "Skeleton".

### B4: `Proxy`, `Upstream`, `LoadBalance` have no crate in the map at all

These three are the whole point of `relayfold-proxy-runtime` (an "UPSTREAMS backend pool + active health
checks" reverse proxy, per this repo's HEAD commit). The README crate map lists no proxy/load-balancer
crate, and `docs/EXTRACTION.md`'s leave-out list (`x402`, `graphql`, `s3_gateway`, `webhook`, `nel`,
`filter`, the five `*_masq` shims) does not mention them either. **No home has been invented here and
nothing was copied out of biota**, per the rules.

### B5: `actix::{Json, Responder}` has no crate in the map

`app.rs:12` imports an extractor pair from biota's `actix` compatibility module. No servelet crate owns a
JSON extractor / `Responder` trait in the map. This is the shape of item the census unit has to decide.

## 3. BASELINE test counts (RUN, recorded verbatim)

Environment: `CARGO_INCREMENTAL=0`, `CARGO_TARGET_DIR=/home/ctown/.cache/cargo-target-reroute-relayfold`,
`-j3`. Pre-flight `free -m` = 7482 MB available, loadavg 7.66, `df -m /` = 27111 MB free — all inside the
limits, no wait loop triggered. No lean `[profile.test]` was added: both crates have no `[profile]` table,
but the profile is a workspace-root concern and adding one at baseline would have perturbed the measurement;
the memory limits were met without it, so it was not added (**no repo Cargo.toml was edited at all**).

Command: `cargo test -j3 -p relayfold-control -p relayfold-proxy-runtime`

| test binary | result |
|---|---|
| `relayfold-control` (lib) | **BUILD FAILED** — 2 × E0277 (blocker B1). 0 tests run. |
| `relayfold-control` (lib test) | **BUILD FAILED** — 2 × E0277 (blocker B1). 0 tests run. |
| `relayfold-control` (test `integration`) | **NOT RUN** — the crate it links against did not build. |
| `relayfold-control` (bin) | **NOT RUN** — did not build. |
| `relayfold-proxy-runtime` (bin `relayfold-proxy`) | `test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s` |

So the baseline is: **one consumer of two does not compile; the other has zero tests.** There is no
pass/fail/ignored population to compare an "after" against for `relayfold-control`.

The two `warning: unused variable` hits in `proxy-runtime/src/main.rs:76,77` (`upstream_host`,
`upstream_port`) are pre-existing at baseline and were left alone.

## 4. Dependency weight, BEFORE only

`cargo tree -p <crate> --prefix none | sort -u | wc -l`, at HEAD with `biota-http-server` still present:

| crate | before | after |
|---|---|---|
| `relayfold-control` | **99** | NOT RUN (no change made) |
| `relayfold-proxy-runtime` | **93** | NOT RUN (no change made) |

`modelfold`, `sangdb`, `transport` are **present before** in `relayfold-proxy-runtime`'s graph:

```
modelfold v0.1.0 (https://github.com/Ptyktos/modelfold#a546c524)
sangdb v0.1.0 (/home/ctown/projects/sangdb)
transport v0.1.0 (https://github.com/Ptyktos/transport#58074bf1)
transport v0.1.0 (https://github.com/Ptyktos/transport#58074bf1) (*)
```

They entered through `biota-http-server`, per the comment at `Cargo.toml:13`. Whether they *leave* cannot
be answered: with B2/B3/B4 unresolved, `relayfold-proxy-runtime` still needs a proxy implementation and a
client, so the weight claim "did not grow" is **NOT PROVEN** and is not asserted.

## 5. Mutation check

**NOT RUN.** It requires a test that a handler change breaks; `relayfold-control`'s tests do not compile
and `relayfold-proxy-runtime` has zero tests. Nothing was flipped or deleted, so nothing was restored.

## 6. What a follow-up reroute needs (decisions, not code)

1. **B1**: relax or re-pin `relayfold-control`'s `hashtrinity` from GitHub `rev = 2c36b995…` to the GitLab
   `main` mirror that `sangdb` (and servelet) use, so the graph unifies. Owned by whoever owns the pin
   decision — not guessed here.
2. **B2**: `servelet-h1` must export `serve` and `serve_tls`; `servelet-tls` must re-export
   `tls_server_config` from its `lib.rs`. Until then `relayfold-control/src/main.rs` and every
   `tests/integration.rs` server start have no target.
3. **B3**: `servelet-client` must be extracted and must provide `Client::new()`, `get(url).send()`,
   `.text()`, `.status()`.
4. **B4**: the census unit must decide the home of `Proxy` / `Upstream` / `LoadBalance`, or relayfold's
   reverse proxy is retired. This is the load-bearing decision for `relayfold-proxy-runtime`.
5. **B5**: the census unit must decide the home of the JSON extractor / `Responder` (`app.rs:12`).
6. Once 1–5 land: reroute `relayfold-control` to `servelet-core` + `servelet-router` + `servelet-h1`
   (+ `servelet-client`), and `relayfold-proxy-runtime` to `servelet-core` + `servelet-router` +
   `servelet-h1` + `servelet-tls` (+ `servelet-client`), then produce the after-counts and the mutation
   check that this report cannot.

Items already confirmed to exist and match the biota call shapes, ready to reroute when the above clears:
`Request` / `Response` (`Response::new`, `Response::ok`, `.status()`, `.text()` all present in
`servelet-core/src/message.rs`) / `StatusCode` / `Router` (`new`, `get`, `post`, `route`,
`handle` in `servelet-router/src/router.rs`).
