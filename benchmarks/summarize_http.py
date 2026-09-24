#!/usr/bin/env python3
"""Summarize raw Nighthawk campaign JSON without hiding failed runs."""

import argparse
import csv
import json
import pathlib


def seconds(value):
    return float(value.removesuffix("s"))


def percentile(stat, rank):
    points = stat.get("percentiles", [])
    return round(seconds(next(point["duration"] for point in points if point["percentile"] >= rank)) * 1000, 3)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=pathlib.Path)
    args = parser.parse_args()
    manifest = json.loads(args.manifest.read_text())
    rows = []
    for run in manifest["runs"]:
        row = {"target": run["target"], "offered_rps": run["offered_rps"], "repeat": run["repeat"],
               "exit_code": run.get("exit_code", "not_run")}
        raw = args.manifest.parent / run["raw_file"]
        if raw.exists() and row["exit_code"] == 0:
            result = json.loads(raw.read_text())["results"][0]
            counters = {entry["name"]: int(entry["value"]) for entry in result["counters"]}
            stats = {entry["id"]: entry for entry in result["statistics"]}
            duration = seconds(result["execution_duration"])
            completed = sum(counters.get(f"benchmark.http_{class_}xx", 0) for class_ in range(1, 6))
            latency = stats["benchmark_http_client.request_to_response"]
            row.update(completed=completed, completed_rps=round(completed / duration, 2),
                       completion_fraction=round(completed / (run["offered_rps"] * duration), 4),
                       http_4xx=counters.get("benchmark.http_4xx", 0),
                       http_5xx=counters.get("benchmark.http_5xx", 0),
                       p50_ms=percentile(latency, 0.50), p95_ms=percentile(latency, 0.95),
                       p99_ms=percentile(latency, 0.99))
        rows.append(row)
    fields = ["target", "offered_rps", "repeat", "exit_code", "completed", "completed_rps",
              "completion_fraction", "http_4xx", "http_5xx", "p50_ms", "p95_ms", "p99_ms"]
    output = args.manifest.parent / "summary.csv"
    with output.open("w", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=fields)
        writer.writeheader()
        writer.writerows(rows)
    print(output)
    for row in rows:
        print(" ".join(f"{key}={row.get(key, '')}" for key in fields))


if __name__ == "__main__":
    main()
