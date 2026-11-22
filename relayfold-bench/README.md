# relayfold-bench

A workspace member of [relayfold](../README.md). Run it with `cargo run -p relayfold-bench --release -- ...`.

A real load-generation/latency harness for relayfold-based proxies, with honest real-vs-real
comparisons against installable competitors. Built to close the gap identified against
zentinel-bench (compares against Envoy/HAProxy/Nginx) and wafworth (598-test WAF quality suite,
confusion-matrix metrics) — this repo covers the load/latency half, not a WAF-quality suite.

## What this is

`src/lib.rs` — `Workload` + `run()`: spawns N OS threads, each holding one persistent HTTP/1.1
connection (`client::request_plain_on`, std only, keep-alive), firing GET requests
back-to-back for a fixed duration, collecting per-request latency. `Report` gives real
throughput (req/s) and p50/p95/p99 (nearest-rank on the sorted sample, no interpolation).

`src/main.rs` (`relayfold-bench`) — CLI: run the SAME workload against N named subjects in
sequence and print one comparison table.

`src/bin/servelet_static_server.rs` (`servelet-static-server`) — a servelet server answering one
static route, as a benchmark subject, so the harness has an in-ecosystem server to measure without
installing a competitor.

## Standing rule this repo follows

Every performance claim needs quality, speed, AND baseline columns on the same held-out data — a
number without a baseline is a wish. Numbers this repo cannot produce from a real run are never
fabricated or estimated; they are reported as "not run" with the reason.

## Real results obtained this session

> **Historical results.** The table below was measured with a `biota-http-server` subject and the
> previous `hashtrinity` client, both since replaced (`servelet-static-server` and `client.rs`).
> It cannot be reproduced with this repository as it stands, and no number has been re-measured
> with the new subject. Treat it as a record of how the harness was used, not as a result for
> servelet.

Environment: shared dev box, 16 cores, **load average ~19 at the time of the run** (other
concurrent processes on the box — see the workspace's own concurrent-agent warning) — these
numbers reflect real contention, not a clean isolated benchmark. Re-run on an idle box for a
cleaner read; the harness and methodology are real regardless.

Workload: `GET /`, 15s, 8 connections per subject, static ~2-byte body from both subjects.

| subject | req/s | errors | p50 ms | p95 ms | p99 ms |
|---|---|---|---|---|---|
| nginx (real, extracted `.deb`, no root) | 6463.0 | 0 | 0.552 | 4.976 | 8.813 |
| biota-http-server (historical subject, no longer in this repo) | 9059.2 | 0 | 0.583 | 2.687 | 6.554 |

A shorter/less-loaded 8s/4-connection run earlier in the same session (before fixing a config
issue described below) showed nginx and biota within ~1% of each other's throughput — take the
above as directional under load, not a precise multiple.

**A real methodology bug found and fixed along the way, disclosed rather than buried**: the first
run against nginx's default config showed 127 "errors" that biota didn't have. Root cause:
nginx's documented default `keepalive_requests` is 1000 — it closes a keep-alive connection after
1000 requests as designed, and this harness's worker loop counted that graceful close as an error
before reconnecting, rather than treating it as an expected renegotiation. Fixed by setting
`keepalive_requests 1000000` in the test nginx.conf for a fair comparison; the harness itself
still lumps "connection closed, reconnecting" and "real error" into one counter — a real,
disclosed limitation, not fixed in this session. A high nginx `keepalive_requests` count in a
production comparison run would need the same fix applied, or the harness's error-counting needs
to distinguish the two cases — noted as a follow-up, not silently worked around.

## haproxy: attempted, not obtained

`apt-get download haproxy` succeeds and extracts without root, but the resulting binary fails to
start: `error while loading shared libraries: libopentracing-c-wrapper.so.0` — that library has no
installable candidate in this environment's apt sources (`apt-cache show
libopentracing-c-wrapper0` returns no candidate). Not worked around; no haproxy numbers are
reported anywhere in this repo or its README. A real haproxy comparison needs an environment where
that dependency (or a haproxy build without the opentracing module) is available.

## Envoy / Pingora: not attempted

Not installed or built in this environment this session — out of time budget for this pass, not
attempted-and-failed like haproxy. A real comparison against either is a real follow-up, not
something this repo claims to cover.

## Running it yourself

```sh
cargo build --release
cargo build --release --bin servelet-static-server

./target/release/servelet-static-server 18081 &
# ... start your own comparison subject on another port, e.g. real nginx ...

./target/release/relayfold-bench / <duration_secs> <connections> name1=host:port name2=host:port
```
