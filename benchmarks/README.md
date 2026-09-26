# Relayfold benchmarks — substrate-side docs and results

This directory used to host the full bench story: harness + subjects
+ configs + orchestrator scripts + results. As of the bench-extraction
commit, the **bench machinery** lives in the dedicated
[`Ptyktos/relayfold-bench`](https://github.com/Ptyktos/relayfold-bench)
repo. What stays here is the substrate-side story:

- **Workload manifests** — `http-targets.example.json`,
  `http-targets.local-matrix.json`, `regression-cases.json`. These
  describe what to measure, versioned with the relayfold code they
  describe.
- **The crosswalk** — `gateway-crosswalk.json`. The source of truth
  for the comparison: every candidate, every plane, every workload
  family, every measurement required.
- **Strategic docs** — `PTYKTOS-GATEWAY-SWOT.md`, `EDGE-GATEWAYS.md`.
  These are part of the relayfold story, not the bench story.
- **Past results** — `RESULTS-2026-09-24*.md` and the `results/`
  directory. These document relayfold's bench history, versioned
  with the substrate.

## Where the bench machinery lives

```
Ptyktos/relayfold-bench/
├── src/                    # the Rust harness (Workload, Report, CLI)
├── subjects/
│   └── biota-proxy/        # one of the bench subjects (was here in
│                            #   relayfold/benchmarks/biota-proxy/)
├── configs/                # competitor-subject configs
│                            #   (envoy, haproxy, nginx, pingap,
│                            #    traefik, zentinel)
└── scripts/                # orchestrator scripts + load generators
                             #   (run_*.sh/py, k6_gateway.js,
                             #    summarize_*.py, probe_mcp.py,
                             #    build/build_h2h_result.py)
```

To run the bench from the relayfold repo, clone the
`relayfold-bench` repo alongside it and follow the
[`relayfold-bench` README](https://github.com/Ptyktos/relayfold-bench).

## Crosswalk first

[`gateway-crosswalk.json`](gateway-crosswalk.json) is the source of
truth for the comparison. It names every candidate, every plane,
every workload family, and the measurements required. Render it
with:

```sh
python3 /path/to/relayfold-bench/scripts/summarize_crosswalk.py \
    benchmarks/gateway-crosswalk.json
```

Use `spbench` for durable run history and comparisons. The wrapper
stores the Nighthawk campaign files under `benchmarks/results/`,
records the Relayfold revision and target-manifest hash, and stores
the wall-time campaign in spbench's configured `SANG_CODSPEED_DB`:

```sh
SPBENCH=/path/to/sang-codspeed \
SANG_CODSPEED_DB=/var/lib/relayfold-bench \
/path/to/relayfold-bench/scripts/run_spbench_campaign.sh \
    benchmarks/http-targets.local-matrix.json \
    --rates 100,300,1000,3000 --duration 30 --warmup 5 --repeats 5 --connections 100
```

## Why the split

The bench harness is meant to be **reusable** across many subjects
(biota, nginx, envoy, ha-proxy, traefik, pingap, zentinel, etc.).
It belongs in its own repo with its own Apache-2.0 license so
consumers can vendor it without the FSL-1.1-ALv2 obligation.

The substrate's bench story — workload manifests, the crosswalk,
strategic docs, past results — is **versioned with the substrate**
so `RESULTS-2026-09-24*.md` documents relayfold's progression over
time. The results stay where the code lives.

See [`/docs/PORTFOLIO.md`](https://github.com/Ptyktos/relayfold/blob/main/docs/PORTFOLIO.md)
in the parent relayfold repo for the cross-portfolio map.
