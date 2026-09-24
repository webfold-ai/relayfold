#!/usr/bin/env bash
set -euo pipefail

targets="${1:?usage: run_k6_campaign.sh <target-manifest> [--rate N] [--duration 30s] [--warmup 2s] [--repeats N] [--vus N] [--output DIR]}"
shift
rates=1000
duration=30s
warmup=2s
repeats=1
vus="${K6_VUS:-256}"
output="${BENCHMARK_OUTPUT:-$(pwd)/benchmarks/results/k6-$(date -u +%Y%m%dT%H%M%SZ)}"
while (($#)); do
  case "$1" in
    --rate) rates="$2"; shift 2;;
    --rates) rates="$2"; shift 2;;
    --duration) duration="$2"; shift 2;;
    --warmup) warmup="$2"; shift 2;;
    --repeats) repeats="$2"; shift 2;;
    --connections) echo "--connections was misleading: k6 models concurrent iterations as VUs; use --vus" >&2; vus="$2"; shift 2;;
    --vus) vus="$2"; shift 2;;
    --output) output="$2"; shift 2;;
    *) echo "unknown option: $1" >&2; exit 2;;
  esac
done
k6_bin="${K6_BIN:-$(command -v k6 || true)}"
if [[ -z "$k6_bin" && -x /home/ctown/projects/image-servers-benchmark/k6 ]]; then k6_bin=/home/ctown/projects/image-servers-benchmark/k6; fi
[[ -x "$k6_bin" ]] || { echo "k6 unavailable; set K6_BIN" >&2; exit 1; }
mkdir -p "$output"
target_count=$(jq -r '.targets | length' "$targets")
[[ "$target_count" -gt 0 ]] || { echo "target manifest contains no targets: $targets" >&2; exit 2; }
[[ "$rates" =~ ^[0-9]+(,[0-9]+)*$ ]] || { echo "rates must be comma-separated positive integers" >&2; exit 2; }
manifest="$output/manifest.json"
resource_csv="$output/resources.csv"
container_csv="$output/container-resources.csv"
process_csv="$output/process-resources.csv"
socket_csv="$output/socket-resources.csv"
rates_json=$(jq -cn --arg rates "$rates" '$rates | split(",") | map(tonumber)')
printf '{"generator":"k6","rates":%s,"max_vus":%s,"duration":"%s","warmup":"%s","repeats":%s,"targets":[' "$rates_json" "$vus" "$duration" "$warmup" "$repeats" >"$manifest"
printf 'timestamp_unix_ns,target,process_cpu_percent,process_rss_kb,process_open_fds\n' >"$resource_csv"
printf 'timestamp_unix_ns,target,component,pid,cpu_percent,rss_kb,open_fds\n' >"$process_csv"
printf 'timestamp_unix_ns,target,listen_port,established_sockets\n' >"$socket_csv"
first=1
run_failed=0
while IFS=$'\t' read -r name url; do
  [[ -n "$name" ]] || continue
  safe="${name//[^A-Za-z0-9_.-]/_}"
  effective_vus="$vus"

  run_one() {
    local label="$1" run_rate="$2" run_duration="$3" summary="$4" stdout="$5" stderr="$6"
    local listen_port listen_hex
    listen_port="${url#*://}"; listen_port="${listen_port%%/*}"; listen_port="${listen_port##*:}"
    listen_hex=$(printf '%04X' "$listen_port")
    local cmd=("$k6_bin" run --summary-export "$summary" benchmarks/k6_gateway.js)
    if [[ -n "${K6_CPUSET:-}" ]] && command -v taskset >/dev/null 2>&1; then cmd=(taskset -c "$K6_CPUSET" "${cmd[@]}"); fi
    TARGET_URL="$url" TARGET_NAME="$name" RUN_LABEL="$label" RATE="$run_rate" DURATION="$run_duration" VUS="$effective_vus" \
      "${cmd[@]}" >"$stdout" 2>"$stderr" &
    local pid=$! sample_ns process_cpu process_rss process_fds
    while kill -0 "$pid" 2>/dev/null; do
      sample_ns=$(date +%s%N)
      if [[ -r "/proc/$pid/status" ]]; then
        process_cpu=$(ps -p "$pid" -o %cpu= 2>/dev/null | tr -d ' ')
        process_rss=$(awk '/^VmRSS:/ {print $2}' "/proc/$pid/status")
        process_fds=$(find "/proc/$pid/fd" -mindepth 1 -maxdepth 1 -type l 2>/dev/null | wc -l)
      printf '%s,%s,%s,%s,%s\n' "$sample_ns" "$label" "$process_cpu" "$process_rss" "$process_fds" >>"$resource_csv"
    fi
      for component in biota_proxy upstream; do
        component_pid="${BENCH_PROXY_PID:-}"
        [[ "$component" == upstream ]] && component_pid="${BENCH_UPSTREAM_PID:-}"
        if [[ -n "$component_pid" && -r "/proc/$component_pid/status" ]]; then
          component_cpu=$(ps -p "$component_pid" -o %cpu= 2>/dev/null | tr -d ' ')
          component_rss=$(awk '/^VmRSS:/ {print $2}' "/proc/$component_pid/status")
          component_fds=$(find "/proc/$component_pid/fd" -mindepth 1 -maxdepth 1 -type l 2>/dev/null | wc -l)
          printf '%s,%s,%s,%s,%s,%s,%s\n' "$sample_ns" "$label" "$component" "$component_pid" "$component_cpu" "$component_rss" "$component_fds" >>"$process_csv"
        fi
      done
      socket_count=$(awk -v port="$listen_hex" '$2 ~ (":" port "$" ) && $4 == "01" {n++} END {print n+0}' /proc/net/tcp /proc/net/tcp6)
      printf '%s,%s,%s,%s\n' "$sample_ns" "$label" "$listen_port" "$socket_count" >>"$socket_csv"
      if command -v docker >/dev/null 2>&1; then
        if [[ ! -e "$container_csv" ]]; then printf 'timestamp_unix_ns,target,container,cpu_percent,memory_usage\n' >"$container_csv"; fi
        docker stats --no-stream --format "{{.Name}},{{.CPUPerc}},{{.MemUsage}}" 2>/dev/null | awk -F, -v ts="$sample_ns" -v target="$label" '$1 ~ /^relayfold-bench-/ {print ts "," target "," $1 "," $2 "," $3}' >>"$container_csv" || true
      fi
      sleep 1
    done
    local exit_code=0
    set +e
    wait "$pid"
    exit_code=$?
    set -e
    return "$exit_code"
  }

  IFS=, read -r -a rate_points <<< "$rates"
  for rate in "${rate_points[@]}"; do
  if [[ "$warmup" != "0" && "$warmup" != "0s" ]]; then
    warmup_rate=1000
    (( rate < warmup_rate )) && warmup_rate="$rate"
    run_one "$name-q${rate}-warmup" "$warmup_rate" "$warmup" "$output/${safe}.q${rate}.warmup.summary.json" "$output/${safe}.q${rate}.warmup.stdout" "$output/${safe}.q${rate}.warmup.stderr" || run_failed=1
    warmup_summary="$output/${safe}.q${rate}.warmup.summary.json"
    if [[ ! -s "$warmup_summary" ]] || ! jq -e '(.metrics.checks.fails // 0) == 0 and (.metrics.dropped_iterations.count // 0) == 0' "$warmup_summary" >/dev/null; then
      echo "invalid warmup for $name; see $warmup_summary" >&2
      run_failed=1
    fi
    sleep 1
  fi
  for ((iteration=1; iteration<=repeats; iteration++)); do
    label="$name-q${rate}-r$iteration"
    summary="$output/${safe}.q${rate}.r$iteration.summary.json"
    run_exit=0
    run_one "$label" "$rate" "$duration" "$summary" "$output/${safe}.q${rate}.r$iteration.stdout" "$output/${safe}.q${rate}.r$iteration.stderr" || run_exit=$?
    [[ -f "$summary" ]] || { echo "k6 produced no summary for $label (exit=$run_exit)" >&2; run_failed=1; continue; }
    if [[ "$first" != 1 ]]; then printf ',' >>"$manifest"; fi
    first=0
    checks=$(jq -r '.metrics.checks.passes // 0' "$summary")
    failed=$(jq -r '.metrics.checks.fails // 0' "$summary")
    requests=$(jq -r '.metrics.http_reqs.count // 0' "$summary")
    dropped=$(jq -r '.metrics.dropped_iterations.count // 0' "$summary")
    errors=$(jq -r '.metrics.request_errors.count // 0' "$summary")
    status5xx=$(jq -r '.metrics.status_5xx.count // 0' "$summary")
    valid=true
    expected_checks=$((requests * 2))
    [[ "$checks" == "$expected_checks" && "$failed" == 0 && "$dropped" == 0 && "$errors" == 0 && "$status5xx" == 0 && "$run_exit" == 0 ]] || valid=false
    jq -cn --arg name "$name" --arg run "$label" --arg url "$url" --argjson rate "$rate" --argjson requests "$requests" --argjson checks_passed "$checks" --argjson checks_failed "$failed" --argjson vus "$effective_vus" --argjson exit_code "$run_exit" --argjson valid "$valid" --slurpfile s "$summary" \
      '{name:$name,run:$run,url:$url,offered_rps:$rate,requests:$requests,checks_passed:$checks_passed,checks_failed:$checks_failed,k6_exit_code:$exit_code,valid:$valid,achieved_rps:$s[0].metrics.http_reqs.rate,p95_ms:$s[0].metrics.http_req_duration["p(95)"],p99_ms:$s[0].metrics.http_req_duration["p(99)"],p999_ms:$s[0].metrics.http_req_duration["p(99.9)"],generator_blocked_p95_ms:$s[0].metrics.http_req_blocked["p(95)"],connect_time_p95_ms:$s[0].metrics.http_req_connecting["p(95)"],http_failed_rate:$s[0].metrics.http_req_failed.value,http_failed_count:(($s[0].metrics.http_req_failed.value // 0) * $requests | round),dropped_iterations:($s[0].metrics.dropped_iterations.count // 0),vus_peak:($s[0].metrics.vus.max // 0),vus_configured:$vus,bad_status:($s[0].metrics.bad_status.count // 0),status_0:($s[0].metrics.status_0.count // 0),status_4xx:($s[0].metrics.status_4xx.count // 0),status_5xx:($s[0].metrics.status_5xx.count // 0),bad_body_bytes:($s[0].metrics.bad_body_bytes.count // 0),request_errors:($s[0].metrics.request_errors.count // 0)}' >>"$manifest"
    [[ "$valid" == true ]] || run_failed=1
    sleep 1
  done
  done
done < <(jq -r '.targets[] | [.name,.url] | @tsv' "$targets")
printf ']}\n' >>"$manifest"
cat "$manifest"
exit "${run_failed:-0}"
