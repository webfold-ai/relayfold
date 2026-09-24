#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
cfg="$PWD/benchmarks/configs"
targets="${BENCH_TARGET_MANIFEST:-benchmarks/http-targets.local-matrix.json}"
if [[ -z "${BENCHMARK_OUTPUT:-}" ]]; then
  BENCHMARK_OUTPUT="$PWD/benchmarks/results/$(date -u +%Y%m%dT%H%M%SZ)-local"
  export BENCHMARK_OUTPUT
fi
mkdir -p "$BENCHMARK_OUTPUT"
started=()
upstream_pid=""
proxy_pid=""

# Keep the open-loop load generator off the gateway's CPUs on shared-host
# development runs. The split is recorded by the campaign wrapper via env.
readarray -t cpu_sets < <(python3 - <<'PY'
import os
cpus=sorted(os.sched_getaffinity(0))
load_count=max(1, len(cpus)//2)
upstream_count=max(1, len(cpus)//4)
load=cpus[:load_count]
upstream=cpus[-upstream_count:] if len(cpus) >= 4 else cpus[-1:]
gateway=[c for c in cpus if c not in load and c not in upstream] or cpus
print(','.join(map(str, load)))
print(','.join(map(str, gateway)))
print(','.join(map(str, upstream)))
PY
)
export K6_CPUSET="${K6_CPUSET:-${cpu_sets[0]}}"
gateway_cpus="${cpu_sets[1]}"
upstream_cpus="${cpu_sets[2]}"
export BENCH_CPU_LAYOUT="k6=${K6_CPUSET};gateway=${gateway_cpus};upstream=${upstream_cpus}"

cleanup() {
  if ((${#started[@]})); then
    # Bound captured overload logs so a bad run cannot fill the work disk.
    for name in "${started[@]}"; do docker logs --tail 5000 "$name" >"$BENCHMARK_OUTPUT/$name.container.log" 2>&1 || true; done
    docker stop "${started[@]}" >/dev/null 2>&1 || true
  fi
  if [[ -n "$proxy_pid" ]]; then kill "$proxy_pid" 2>/dev/null || true; wait "$proxy_pid" 2>/dev/null || true; fi
  if [[ -n "$upstream_pid" ]]; then kill "$upstream_pid" 2>/dev/null || true; wait "$upstream_pid" 2>/dev/null || true; fi
}
trap cleanup EXIT

python3 - <<'PY'
import socket
for port in (18080, 18081, 18085, 18086, 18087, 18088, 18089, 18090):
    with socket.socket() as sock:
        sock.settimeout(0.2)
        if sock.connect_ex(("127.0.0.1", port)) == 0:
            raise SystemExit(f"port {port} is already in use; local matrix requires free ports")
PY

start_container() {
  local name="$1"
  shift
  # Docker commonly defaults to 1024 FDs, below the configured gateway caps.
  docker run -d --rm --name "$name" --network host --cpuset-cpus "$gateway_cpus" \
    --ulimit nofile=1048576:1048576 "$@" >/dev/null
  started+=("$name")
}

cargo build --release --manifest-path benchmarks/biota-proxy/Cargo.toml >"$BENCHMARK_OUTPUT/build.log" 2>&1 || {
  tail -100 "$BENCHMARK_OUTPUT/build.log" >&2
  exit 1
}
taskset -c "$upstream_cpus" "${CARGO_TARGET_DIR:-benchmarks/biota-proxy/target}/release/bench_upstream" >/dev/null 2>&1 &
upstream_pid=$!
BENCH_BIND=127.0.0.1:18080 BENCH_UPSTREAM_PORT=18081 \
  taskset -c "$gateway_cpus" "${CARGO_TARGET_DIR:-benchmarks/biota-proxy/target}/release/relayfold-biota-bench-proxy" >/dev/null 2>&1 &
proxy_pid=$!
export BENCH_PROXY_PID="$proxy_pid"
export BENCH_UPSTREAM_PID="$upstream_pid"

start_container relayfold-bench-nginx \
  -v "$cfg/nginx.conf:/etc/nginx/nginx.conf:ro" \
  nginx@sha256:516475cc129da42866742567714ddc681e5eed7b9ee0b9e9c015e464b4221a00
start_container relayfold-bench-haproxy \
  -v "$cfg/haproxy.cfg:/usr/local/etc/haproxy/haproxy.cfg:ro" \
  haproxy@sha256:3e29449a6beed63262e36104adf531b4e41b359f61937303f5ea8607987b3748
start_container relayfold-bench-envoy \
  -v "$cfg/envoy.yaml:/etc/envoy/envoy.yaml:ro" \
  envoyproxy/envoy@sha256:caa5b411be1633b90023592a34a7e010c933d6e60206c758f631485e53006865
start_container relayfold-bench-zentinel \
  -v "$cfg/zentinel.kdl:/etc/zentinel/zentinel.kdl:ro" \
  ghcr.io/zentinelproxy/zentinel@sha256:6611f8894d8672b6ef05f3dd69caf512bfa104f6e35ce53e8446c4f44b88c91c
start_container relayfold-bench-pingap \
  -v "$cfg/pingap.toml:/etc/pingap/pingap.toml:ro" \
  --entrypoint pingap \
  vicanso/pingap@sha256:02bfe3cdcdbbef33c8aa810a515a5b80aff01d1afab1685d36ff10aeaa38367c \
  --conf /etc/pingap/pingap.toml
start_container relayfold-bench-traefik \
  -v "$cfg/traefik.yml:/etc/traefik/traefik.yml:ro" \
  -v "$cfg/traefik-dynamic.yml:/etc/traefik/dynamic.yml:ro" \
  traefik:v3.3

python3 - "$targets" <<'PY'
import json, sys, time, urllib.request
targets = json.load(open(sys.argv[1]))['targets']
for target in targets:
    for attempt in range(50):
        try:
            with urllib.request.urlopen(target['url'], timeout=2) as response:
                if response.status == 200 and len(response.read()) == 1024:
                    break
        except Exception:
            pass
        time.sleep(0.2)
    else:
        raise SystemExit(f"preflight failed: {target['name']} {target['url']}")
print(f"Preflight passed: {len(targets)} paths returned HTTP 200 and 1024 bytes")
PY

if [[ "${PERSISTENT:-0}" == "1" ]]; then
  : "${SPBENCH:?PERSISTENT=1 requires SPBENCH=/path/to/sang-codspeed}"
  benchmarks/run_spbench_campaign.sh "$targets" "$@"
  exit $?
fi
python3 benchmarks/run_http.py "$targets" "$@"
