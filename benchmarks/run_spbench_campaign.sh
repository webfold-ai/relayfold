#!/usr/bin/env bash
set -euo pipefail

# Persistent campaign wrapper. The HTTP runner owns per-request histograms and
# raw Nighthawk output; spbench owns warmup/repetition, commit metadata, and the
# durable sangdb run record/comparison.
if [[ $# -lt 1 ]]; then
  echo "usage: $0 <target-manifest> [run_http options...]" >&2
  exit 2
fi
targets="$1"
shift
relayfold_root="$(cd "$(dirname "$0")/.." && pwd)"
spbench_bin="${SPBENCH:-}"
if [[ -z "$spbench_bin" ]]; then
  if command -v sang-codspeed >/dev/null 2>&1; then
    spbench_bin="$(command -v sang-codspeed)"
  elif [[ -x "$relayfold_root/../spbench/target/release/sang-codspeed" ]]; then
    spbench_bin="$relayfold_root/../spbench/target/release/sang-codspeed"
  else
    echo "spbench unavailable; set SPBENCH=/path/to/sang-codspeed or build ../spbench" >&2
    exit 1
  fi
fi
if [[ ! -x "$spbench_bin" ]]; then
  echo "spbench binary is not executable: $spbench_bin" >&2
  exit 1
fi
output_dir="${BENCHMARK_OUTPUT:-$relayfold_root/benchmarks/results}"
mkdir -p "$output_dir"
python3 - "$output_dir/campaign-metadata.json" "$targets" "$relayfold_root" <<'PY'
import hashlib, json, pathlib, platform, subprocess, sys, time
out, targets, root = sys.argv[1:]
try:
    revision = subprocess.check_output(["git", "-C", root, "rev-parse", "HEAD"], text=True).strip()
except Exception:
    revision = "unknown"
json.dump({"started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
           "host": platform.node(), "platform": platform.platform(),
           "cpu_layout": __import__("os").environ.get("BENCH_CPU_LAYOUT", "unspecified"),
           "relayfold_revision": revision,
           "target_manifest": str(pathlib.Path(targets).resolve()),
           "target_manifest_sha256": hashlib.sha256(pathlib.Path(targets).read_bytes()).hexdigest()},
          open(out, "w"), indent=2)
PY
generator="${BENCHMARK_GENERATOR:-k6}"
if [[ "$generator" == "k6" ]]; then
  rates="${K6_RATES:-${K6_RATE:-1000}}"; duration="${K6_DURATION:-30s}"; warmup="${K6_WARMUP:-2s}"; repeats="${K6_REPEATS:-1}"; vus="${K6_VUS:-256}"
  while (($#)); do
    case "$1" in
      --rate|--rates) rates="$2"; shift 2;;
      --duration) duration="$2"; shift 2; [[ "$duration" == *[!0-9.]* ]] || duration+="s";;
      --warmup) warmup="$2"; shift 2; [[ "$warmup" == *[!0-9.]* ]] || warmup+="s";;
      --repeats) repeats="$2"; shift 2;;
      --connections) echo "--connections was misleading: use --vus for k6 concurrency" >&2; vus="$2"; shift 2;;
      --vus) vus="$2"; shift 2;;
      --output) output_dir="$2"; shift 2;;
      --protocol) shift 2;;
      *) echo "unknown k6 campaign option: $1" >&2; exit 2;;
    esac
  done
  [[ "$duration" == *[!0-9.]* ]] || duration+="s"
  [[ "$warmup" == *[!0-9.]* ]] || warmup+="s"
  [[ "$rates" =~ ^[0-9]+(,[0-9]+)*$ && "$repeats" =~ ^[0-9]+$ && "$vus" =~ ^[0-9]+$ ]] || { echo "rates must be comma-separated integers; repeats and VUs must be integers" >&2; exit 2; }
  [[ -n "$duration" && -n "$warmup" ]] || { echo "duration and warmup are required" >&2; exit 2; }
  cmd="cd '$relayfold_root' && benchmarks/run_k6_campaign.sh '$targets' --rates '$rates' --duration '$duration' --output '$output_dir'"
  cmd+=" --warmup '$warmup' --repeats '$repeats'"
  cmd+=" --vus '$vus'"
else
  echo "unsupported benchmark generator '$generator'; use BENCHMARK_GENERATOR=k6" >&2
  exit 2
fi
spbench_args=(run "$cmd" --binary "${BENCHMARK_BINARY:-relayfold-gateway}" --warmup "${SPBENCH_WARMUP:-0}" --runs "${SPBENCH_RUNS:-1}")
if [[ -n "${SPBENCH_BASE_RUN:-}" ]]; then
  spbench_args+=(--base "$SPBENCH_BASE_RUN")
fi
if "$spbench_bin" "${spbench_args[@]}"; then spbench_status=0; else spbench_status=$?; fi
[[ "$spbench_status" == 0 ]] || exit "$spbench_status"
[[ "$generator" == k6 ]] || exit 0
manifest="$output_dir/manifest.json"
[[ -s "$manifest" ]] || { echo "benchmark manifest missing or empty: $manifest" >&2; exit 1; }
jq -e '.targets | length > 0 and all(.valid == true)' "$manifest" >/dev/null || {
  echo "campaign contains invalid measurements; inspect $manifest and per-run summaries" >&2
  exit 1
}
