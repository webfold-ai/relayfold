# Incumbent gateway campaign — 2026-09-24

Command: `PERSISTENT=1 BENCHMARK_GENERATOR=k6 ... run_local_matrix.sh --rates 10000 --duration 1 --warmup 0 --repeats 1 --connections 1000`

The generator was the native k6 binary. Each iteration required HTTP 200 and exactly 1,024 response bytes. `requests` is completed request count; `achieved_rps` is k6's measured request rate; `p95_ms` is the reported request-duration p95; `dropped_iterations` records load the target could not start. This is a bounded capacity probe, not a 1M-RPS claim.

| target | requests | achieved RPS | p95 ms | dropped iterations | checks failed |
|---|---:|---:|---:|---:|---:|
| direct upstream | 10,001 | 9,994.0 | 3.36 | 1 | 0 |
| biota / relayfold | 9,904 | 9,892.0 | 6.43 | 97 | 0 |
| nginx | 9,887 | 9,874.0 | 3.86 | 114 | 0 |
| haproxy | 8,103 | 4,556.9 | 70.93 | 1,897 | 0 |
| envoy | 9,141 | 9,130.4 | 6.74 | 860 | 0 |
| zentinel | 7,040 | 6,819.8 | 73.14 | 2,961 | 0 |
| pingap | 5,315 | 2,862.9 | 189.99 | 4,686 | 22 |

Run record: spbench `7071305:808762`. Raw k6 summaries and the manifest were captured under `/tmp/k6-incumbents-20260924T080649Z` during execution. The run was deliberately bounded and its temporary files were removed after recording this report because the root filesystem had less than 1 GB free.
