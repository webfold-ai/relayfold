#!/usr/bin/env python3
"""Run the same fixed offered-load Nighthawk ladder against named HTTP targets."""

import argparse
import datetime as dt
import json
import os
import pathlib
import platform
import shutil
import subprocess
import sys
import time
import urllib.request

NIGHTHAWK_IMAGE = "envoyproxy/nighthawk-dev@sha256:41765135df56107e154743f839471bae170f14e4f1de249efd1ca2676095682b"


def client_command():
    binary = shutil.which("nighthawk_client")
    if binary:
        return [binary]
    if shutil.which("docker"):
        return ["docker", "run", "--rm", "--network", "host", "--entrypoint", "nighthawk_client", NIGHTHAWK_IMAGE]
    raise SystemExit("Nighthawk is unavailable: install nighthawk_client or Docker")


def probe(url):
    with urllib.request.urlopen(url, timeout=5) as response:
        body = response.read()
        if response.status != 200:
            raise RuntimeError(f"{url}: HTTP {response.status}")
        return {"status": response.status, "body_bytes": len(body)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("targets", type=pathlib.Path, help="JSON file with targets: [{name,url}]")
    parser.add_argument("--rates", default="100,500,1000", help="Aggregate offered requests/s; concurrency is fixed at 1")
    parser.add_argument("--duration", type=int, default=30)
    parser.add_argument("--warmup", type=int, default=5)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--connections", type=int, default=100)
    parser.add_argument("--max-active-requests", type=int, default=100000)
    parser.add_argument("--command-timeout", type=int, default=120,
                        help="hard timeout per Nighthawk run (seconds)")
    parser.add_argument("--protocol", choices=("http1", "http2"), default="http1")
    parser.add_argument("--output", type=pathlib.Path, default=pathlib.Path("benchmarks/results"))
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    if min(args.duration, args.repeats, args.connections) < 1 or args.warmup < 0:
        parser.error("duration, repeats and connections must be positive; warmup cannot be negative")
    rates = [int(value) for value in args.rates.split(",")]
    if not rates or any(rate < 1 for rate in rates):
        parser.error("rates must be positive integers")
    targets = json.loads(args.targets.read_text())["targets"]
    if not targets or any(set(target) != {"name", "url"} for target in targets):
        parser.error("targets must contain name and url only")
    if len({target["name"] for target in targets}) != len(targets) or any(
        not target["name"].replace("-", "").replace("_", "").isalnum() for target in targets
    ):
        parser.error("target names must be unique letters, digits, hyphens or underscores")
    command = client_command()
    campaign = args.output / dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    manifest = {
        "status": "dry-run" if args.dry_run else "running",
        "started_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "host": {"node": platform.node(), "platform": platform.platform(), "cpu_count": os.cpu_count()},
        "generator": command,
        "generator_version": subprocess.run(command + ["--version"], text=True, capture_output=True, check=True).stdout.strip(),
        "settings": {"rates_aggregate_rps": rates, "duration_s": args.duration, "warmup_s": args.warmup,
                     "repeats": args.repeats, "connections": args.connections, "protocol": args.protocol,
                     "concurrency": 1},
        "targets": targets,
        "runs": [],
    }
    campaign.mkdir(parents=True, exist_ok=False)
    try:
        for target in targets:
            if not args.dry_run:
                target["probe"] = probe(target["url"])
            for rate in rates:
                for repeat in range(1, args.repeats + 1):
                    stem = f"{target['name']}_{args.protocol}_{rate}rps_run{repeat}"
                    base = command + ["--open-loop", "--concurrency", "1", "--connections", str(args.connections),
                                      "--max-active-requests", str(args.max_active_requests),
                                      "--rps", str(rate), "--duration", str(args.duration),
                                      "--protocol", args.protocol, "--output-format", "json", target["url"]]
                    record = {"target": target["name"], "offered_rps": rate, "repeat": repeat,
                              "command": base, "raw_file": f"{stem}.json"}
                    manifest["runs"].append(record)
                    if args.dry_run:
                        continue
                    if args.warmup:
                        warmup = base.copy()
                        warmup[warmup.index("--duration") + 1] = str(args.warmup)
                        subprocess.run(warmup, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True)
                    started = time.monotonic()
                    try:
                        result = subprocess.run(base, text=True, capture_output=True,
                                                timeout=args.command_timeout)
                    except subprocess.TimeoutExpired as error:
                        record["exit_code"] = 124
                        record["error"] = f"Nighthawk timeout after {args.command_timeout}s"
                        (campaign / f"{stem}.stderr").write_text(str(error))
                        raise RuntimeError(f"{stem}: Nighthawk timed out; offered load exceeded the bounded run window")
                    (campaign / record["raw_file"]).write_text(result.stdout)
                    (campaign / f"{stem}.stderr").write_text(result.stderr)
                    record["elapsed_s"] = round(time.monotonic() - started, 3)
                    record["exit_code"] = result.returncode
                    if result.returncode:
                        raise RuntimeError(f"{stem}: Nighthawk exited {result.returncode}; see stderr")
                    payload = json.loads(result.stdout)
                    global_result = payload.get("results", [{}])[0]
                    counters = {item.get("name"): int(item.get("value", 0))
                                for item in global_result.get("counters", [])}
                    stats = {item.get("id"): item for item in global_result.get("statistics", [])}
                    completed = counters.get("benchmark.http_2xx", 0)
                    body_size = stats.get("benchmark_http_client.response_body_size", {})
                    raw_mean = int(body_size.get("raw_mean", 0))
                    if completed == 0 or raw_mean != 1024:
                        raise RuntimeError(
                            f"{stem}: invalid wire result: 2xx={completed}, response_body_raw_mean={raw_mean}"
                        )
                    record["completed_2xx"] = completed
                    record["response_body_bytes"] = raw_mean
                    (campaign / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        manifest["status"] = "dry-run" if args.dry_run else "complete"
    except Exception as error:
        manifest["status"] = "failed"
        manifest["error"] = str(error)
        raise
    finally:
        (campaign / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        print(campaign / "manifest.json")


if __name__ == "__main__":
    main()
