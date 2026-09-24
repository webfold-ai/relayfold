# Edge gateway and tunnel cases

Traefik is included in the local matrix on port `18090` and uses the same native 1 KiB upstream as every other target.

Pangolin and NetBird are deployment systems, not interchangeable single-process HTTP proxies. Their benchmark path must exercise a real deployed endpoint. `run_edge_gateway_campaign.sh` accepts `PANGOLIN_URL`, `NETBIRD_URL`, and/or `WIREGUARD_URL`; unset endpoints are omitted. `WIREGUARD_URL` is the endpoint reached through the WireGuard route, so the result includes tunnel and reverse-proxy overhead. Every request still requires HTTP 200 and exactly 1,024 response bytes.

Example:

```sh
PANGOLIN_URL=https://gateway.example \
NETBIRD_URL=https://netbird-proxy.example \
WIREGUARD_URL=http://10.10.0.8:8080 \
RATE=10000 DURATION=10s \
bash benchmarks/run_edge_gateway_campaign.sh
```

The script persists k6 summaries and a manifest under `benchmarks/results/` (or `OUTPUT`). It does not claim Pangolin or NetBird results until those real URLs are supplied.
