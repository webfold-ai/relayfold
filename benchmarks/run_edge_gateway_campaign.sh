#!/usr/bin/env bash
set -euo pipefail

# Run real, externally configured Pangolin/NetBird reverse-proxy endpoints.
# No endpoint is invented: unset variables are omitted from the manifest.
: "${K6_BIN:=k6}"
: "${RATE:=10000}"
: "${DURATION:=1s}"
: "${OUTPUT:=benchmarks/results/edge-$(date -u +%Y%m%dT%H%M%SZ)}"
mkdir -p "$OUTPUT"
manifest="$OUTPUT/manifest.json"

entries=()
[[ -n "${PANGOLIN_URL:-}" ]] && entries+=("{\"name\":\"pangolin\",\"url\":\"${PANGOLIN_URL%/}/1k\"}")
[[ -n "${NETBIRD_URL:-}" ]] && entries+=("{\"name\":\"netbird-reverse-proxy\",\"url\":\"${NETBIRD_URL%/}/1k\"}")
[[ -n "${WIREGUARD_URL:-}" ]] && entries+=("{\"name\":\"wireguard-path\",\"url\":\"${WIREGUARD_URL%/}/1k\"}")
((${#entries[@]})) || { echo "set PANGOLIN_URL, NETBIRD_URL, or WIREGUARD_URL" >&2; exit 2; }
printf '%s\n' "${entries[@]}" | jq -s '{targets: .}' > "$manifest"
BENCHMARK_OUTPUT="$OUTPUT" "$PWD/benchmarks/run_k6_campaign.sh" "$manifest" --rate "$RATE" --duration "$DURATION" --output "$OUTPUT"
