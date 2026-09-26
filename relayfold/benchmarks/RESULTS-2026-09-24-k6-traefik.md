# Incumbent gateway campaign with Traefik — 2026-09-24

Native k6 open-loop run at 10,000 offered requests/s for 1 second; spbench stored run `7071305:843564`. All eight paths passed preflight against the native async 1 KiB upstream. Each k6 iteration checks HTTP 200 and a 1,024-byte body.

| target | requests | achieved RPS | p95 ms | dropped iterations | failed checks |
|---|---:|---:|---:|---:|---:|
| direct upstream | 10,001 | 9,987.3 | 2.15 | 0 | 0 |
| Biota / Relayfold | 9,850 | 9,841.1 | 9.16 | 150 | 0 |
| NGINX | 9,993 | 9,984.9 | 3.40 | 8 | 0 |
| HAProxy | 8,779 | 4,806.1 | 36.40 | 1,225 | 0 |
| Envoy | 9,398 | 9,384.1 | 11.03 | 605 | 0 |
| Zentinel | 6,192 | 5,890.0 | 88.40 | 3,808 | 0 |
| Pingap | 4,797 | 2,456.8 | 208.45 | 5,204 | 28 |
| Traefik | 8,701 | 8,657.0 | 29.92 | 1,300 | 0 |

This is a one-second screening run, not a stable ranking or a 1M-RPS claim. Pangolin and NetBird were not scored: they need real deployed public/overlay endpoints. The reusable endpoint harness is `run_edge_gateway_campaign.sh`; set `PANGOLIN_URL`, `NETBIRD_URL`, and/or `WIREGUARD_URL` to measure those actual paths. The WireGuard score then includes the tunnel path.

