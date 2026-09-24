# Saturation finding — relayfold incumbent benchmark campaign (2026-09-24)

## Summary

A 10k rps offered-rate diagnostic on the local cross-gateway matrix
(`results/20260924T092008Z-generator-isolated-diagnostic/`) does **not** qualify as a
benchmark result. It exposes two distinct saturations on this 16-core dev host:

1. **Generator saturation.** At 10k offered rps the k6 loadgen process pegs
   7 of its 8 assigned cores (peak 700% CPU on `direct-r1`, 500% on
   `biota-relayfold-r1`). `direct-r1` and `direct-r2` both drop iterations
   (75 / 55) and peak at 145 / 40 active VUs before the loadgen starts
   rejecting iterations outright. `biota-relayfold` drops 165 / 272 with
   the same VU ceiling at 153 / 39.
2. **Zentinel upstream-selection saturation.** Zentinel's container log
   (`relayfold-bench-zentinel.container.log`) shows 5,000
   `Failed to select upstream after max attempts` errors and 1,228
   `Failed to select upstream peer` warnings during the two repeats.
   `zentinel-r1` dropped 19,866 iterations and achieved 5,910 rps;
   `zentinel-r2` dropped 29,074 and surfaced 20,421 status_5xx. Envoy is
   the only target that completed the 10k rps open-loop offer without
   saturation in the same campaign (`envoy-r1` achieved 9,996 rps with
   0 dropped iterations and VUs peak 11).

The harness now flags every 10k rps row as `valid: false`:
`run_k6_campaign.sh` requires `dropped_iterations == 0`, `request_errors == 0`,
`status_5xx == 0`, `k6_exit_code == 0`, and `checks_passed == 2*requests`. None of
the four 10k rps targets meet those conditions. `run_spbench_campaign.sh` then
rejects the campaign wholesale (`jq -e '.targets | all(.valid == true)'`
fails), so a 10k rps run can no longer be persisted as a comparison.

## What this bounds

| Offered rps | Direct | Biota/Relayfold | Envoy | Zentinel | Verdict |
|---:|---|---|---|---|---|
| 1,000 | 999.7 rps, 0 drops | 999.8 rps, 0 drops | 999.9 rps, 0 drops | 999.7 rps, 0 drops | clean |
| 2,000 | 1,999.8 rps, 0 drops | 1,999.8 rps, 0 drops | 2,000 rps, 0 drops | 1,999.4 rps, 0 drops (q2000-r2 hits 232 VUs peak, surfaces the upstream-selection cliff at slightly above 2k) | clean except Zentinel-r2 |
| 10,000 | 9,979 rps, 75/55 drops, k6 at 700% CPU | 9,963 rps, 165/272 drops | 9,996 rps, 0 drops | 5,910 rps, 19,866/29,074 drops, 20,421 status_5xx | rejected by harness |

1k and 2k rps are the rates at which the harness can certify every target's
`direct` path; 2k rps is the highest clean rate that still differentiates
incumbent latencies and does not require any of the targets to saturate. The
5x repeat h2h bench uses 2k rps.

## Why this is a finding, not a bug

The diagnostic was the correct step: the previous Nighthawk-style campaign
artifact (`RESULTS-2026-09-24-k6-traefik.md`) was a one-second bounded probe that
quietly accepted Zentinel's `zentinel-r1` row as a real number despite 3,808
dropped iterations and a 5,890 rps achieved rate. That acceptance was the
anti-loop hole: a "headline" row whose loadgen and target were both
saturated. The harness fix (`run_k6_campaign.sh`'s `valid` check) closes it.
A saturated run is now a measurement of saturation, not a measurement of
the gateway under test. The 10k rps diagnostic and this document are the
saturated measurement.

## Files

- 10k rps diagnostic (rejected by harness): `results/20260924T092008Z-generator-isolated-diagnostic/`
- 1k/2k rps diagnostic (clean; carries 8 targets × 2 repeats): `results/20260924T092431Z-local-cross-gateway-curve/`
- Container log of the Zentinel upstream-selection failure at 10k rps: `results/20260924T092008Z-generator-isolated-diagnostic/relayfold-bench-zentinel.container.log`
- 5x repeat h2h result at 2k rps: `results/relayfold-h2h-2026-09-24.json` (see `relayfold-h2h-2026-09-24.md`)
