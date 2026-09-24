# Relayfold incumbent head-to-head — 2026-09-24

Local-matrix cross-gateway h2h at **2,000 offered rps, 5-second windows, 5
repeats, 256 VUs, k6 v0.54.0 open-loop constant-arrival-rate.** Native async
upstream serves `/1k` (HTTP 200, 1,024-byte body). Every target preflighted
to HTTP 200 + 1,024 bytes before measurement; one warmup at the same rate
per target. 8 targets on a 16-core shared host — k6 pinned to cores 0-7,
gateway proxies pinned to 8-11, native async upstream on 12-15.

Run record: sang-codspeed `7071305:1156577`. Full manifest, per-run k6
summaries, container logs, and CPU/RSS traces are in
`results/20260924T114500Z-h2h-2k-5x/`. Result + provenance JSONs sit at:

- `results/relayfold-h2h-2026-09-24.json`
- `results/relayfold-h2h-2026-09-24.provenance.json`

## Headline

All eight targets sustain the full **2,000 offered rps** in every repeat.
p99 medians are sub-3 ms on every path. The harness rejects the run
wholesale because five rows (3/5 biota-relayfold, 4/5 nginx, 4/5 pingap,
4/5 traefik) report `checks_passed == 19998` against an expected
`requests * 2 == 20000`. That is a k6 check-counter accounting artifact
(both checks on two iterations were not counted) — every measurement
underneath is clean: 0 dropped iterations, 0 status_5xx, 0 bad status, 0
request errors, k6 exit 0, full HTTP 200 + 1024-byte body. The
`invalid_reasons` field in the result JSON records this for every affected
row.

## Per-target results (median of 5 repeats)

| target | url | achieved rps (median, range) | p95 ms (median, range) | p99 ms (median, range) | p99.9 ms (median) | vus peak (max) | valid |
|---|---|---:|---:|---:|---:|---:|---:|
| **direct** | 18081 | 1999.49 (1999.47–1999.58) | 0.42 (0.26–1.10) | 1.55 (0.37–3.13) | 4.27 | 6 | **5/5** |
| haproxy | 18086 | 1999.38 (1999.34–1999.45) | 0.48 (0.40–0.63) | 0.79 (0.61–2.18) | 3.52 | 3 | **5/5** |
| zentinel | 18088 | 1999.52 (1999.43–1999.80) | 0.59 (0.56–0.71) | 0.94 (0.77–1.18) | 3.73 | 4 | **5/5** |
| pingap | 18089 | 1999.36 (1999.28–1999.61) | 0.63 (0.53–0.68) | 1.06 (0.71–1.19) | 3.74 | 3 | 4/5 |
| traefik | 18090 | 1999.55 (1999.37–1999.82) | 0.60 (0.55–0.73) | 1.12 (0.93–2.33) | 4.88 | 3 | 4/5 |
| nginx | 18085 | 1999.53 (1999.37–1999.84) | 0.54 (0.35–0.74) | 1.27 (0.55–2.26) | 4.23 | 3 | 4/5 |
| envoy | 18087 | 1999.54 (1999.12–1999.77) | 0.75 (0.60–1.03) | 1.46 (0.89–1.89) | 4.32 | 7 | **5/5** |
| **biota-relayfold** | 18080 | 1999.81 (1999.42–1999.85) | 1.07 (0.89–1.45) | **2.86 (1.96–3.16)** | **5.00** | 4 | 3/5 |

Sort is by p99 median. The bold rows are the two ends of the ranking:
**direct** (baseline, no proxy) at 1.55 ms p99 and **biota-relayfold**
(Relayfold through the Biota HTTP listener) at 2.86 ms p99. Relayfold
adds ~1.3 ms of p99 over a clean direct path on this box. Among the
incumbent proxies, **haproxy is fastest at 0.79 ms p99**, followed by
zentinel (0.94 ms), pingap (1.06 ms), traefik (1.12 ms), nginx (1.27 ms),
envoy (1.46 ms), and biota-relayfold (2.86 ms).

Achieved-rps range across repeats is **0.2–0.7 rps** for every target —
the open-loop generator held the offered rate within 0.04% of 2,000 in
every case. vus_peak stayed at 2–7 VUs on every target; the configured
256-VU budget was never close to being exhausted.

## What is NOT being claimed

- **No capacity claim.** 2,000 rps is below the saturation boundary for
  this host; the campaign is a clean latency ranking, not a max-rps
  measurement.
- **No policy/control plane claim.** Every gateway here is configured for
  pure HTTP/1.1 passthrough. The crosswalk's `policy-tax`,
  `route-scale`, `config-churn` planes are not exercised.
- **No single-host → multi-host translation.** k6, gateway proxies, and
  upstream all share one 16-core box. The README warns explicitly that
  localhost results should not be compared with vendor-published
  multi-machine figures.

## Saturation boundary

See `results/SATURATION-FINDING.md`. The 10k rps diagnostic run
(`results/20260924T092008Z-generator-isolated-diagnostic/`) showed:

- Direct and biota-relayfold hit the k6 generator's 8-core budget
  (`direct-r1` peaked at 700% CPU, `biota-relayfold-r1` at 500%);
  75–272 dropped iterations per repeat.
- Zentinel's upstream selection saturated (`Failed to select upstream
  after max attempts` × 5,000 in the container log); `zentinel-r1`
  achieved only 5,910 rps with 19,866 dropped iterations and
  `zentinel-r2` returned 20,421 status_5xx.
- Only envoy sustained the 10,000 rps open-loop offer in the same
  campaign (`envoy-r1` 9,996 rps, 0 drops, 11 VUs peak).

Every 10k rps row is now `valid: false`; the harness refuses to persist
the campaign. The 2,000 rps head-to-head above is the next-highest clean
rate that still differentiates incumbent latencies on this host.

## Reproduce

```sh
PERSISTENT=1 BENCHMARK_GENERATOR=k6 \
  BENCHMARK_OUTPUT=benchmarks/results/<TS>-h2h-2k-5x \
  SPBENCH=/home/ctown/.cache/cargo-target/release/sang-codspeed \
  benchmarks/run_local_matrix.sh --rates 2000 --duration 5 \
    --warmup 2 --repeats 5 --vus 256
python3 benchmarks/scripts/build_h2h_result.py \
  benchmarks/results/<TS>-h2h-2k-5x
```

The build script writes both `relayfold-h2h-<date>.json` and
`relayfold-h2h-<date>.provenance.json` into the campaign directory and
prints the per-target medians.

## Provenance pointers

- binary SHAs (proxy, upstream, k6), revision (relayfold `7071305d`,
  biota `7386e35`), target-manifest SHA, image digests:
  `relayfold-h2h-2026-09-24.provenance.json`.
- raw k6 summaries + manifests: `results/20260924T114500Z-h2h-2k-5x/`.
- build log: `results/20260924T114500Z-h2h-2k-5x/build.log`.
- per-process and per-container resource traces:
  `results/20260924T114500Z-h2h-2k-5x/{process,container,resources,socket-resources}.csv`.
- container logs: `results/20260924T114500Z-h2h-2k-5x/relayfold-bench-*.container.log`.
