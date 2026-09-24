#!/usr/bin/env python3
"""Build the relayfold-h2h-<date>.json result and provenance JSONs from a
k6 campaign manifest. The campaign manifest is the raw row schema;
this script collapses 40 target rows into 8 target summaries and
emits the durable result + provenance.

Usage:
    build_h2h_result.py <campaign-dir> [--out-dir <dir>] [--date YYYY-MM-DD]
"""
import argparse
import datetime
import hashlib
import json
import os
import pathlib
import statistics
import subprocess
import sys


def median(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


def stdev(xs):
    xs = [x for x in xs if x is not None]
    return statistics.stdev(xs) if len(xs) > 1 else 0.0


def min_max(xs):
    xs = [x for x in xs if x is not None]
    if not xs:
        return (None, None)
    return (min(xs), max(xs))


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


def git_rev(root):
    try:
        return subprocess.check_output(
            ["git", "-C", root, "rev-parse", "HEAD"], text=True
        ).strip()
    except Exception:
        return "unknown"


def main():
    p = argparse.ArgumentParser()
    p.add_argument("campaign_dir", type=pathlib.Path)
    p.add_argument("--out-dir", type=pathlib.Path, default=None)
    p.add_argument("--date", default=datetime.datetime.utcnow().strftime("%Y-%m-%d"))
    p.add_argument(
        "--k6-bin",
        default=os.environ.get("K6_BIN", "/home/ctown/projects/image-servers-benchmark/k6"),
    )
    args = p.parse_args()

    campaign = args.campaign_dir
    manifest_path = campaign / "manifest.json"
    meta_path = campaign / "campaign-metadata.json"
    target_manifest_path = pathlib.Path(
        "/home/ctown/projects/relayfold/benchmarks/http-targets.local-matrix.json"
    )
    if not manifest_path.exists():
        sys.exit(f"manifest not found: {manifest_path}")
    if not meta_path.exists():
        sys.exit(f"campaign-metadata not found: {meta_path}")

    manifest = json.loads(manifest_path.read_text())
    meta = json.loads(meta_path.read_text())

    out_dir = args.out_dir or campaign
    out_dir.mkdir(parents=True, exist_ok=True)
    out_json = out_dir / f"relayfold-h2h-{args.date}.json"
    out_provenance = out_dir / f"relayfold-h2h-{args.date}.provenance.json"

    target_rows = sorted(
        manifest["targets"], key=lambda r: (r["name"], r["run"])
    )

    by_target = {}
    for row in target_rows:
        by_target.setdefault(row["name"], []).append(row)

    def summarize(name, rows):
        achieved = [r["achieved_rps"] for r in rows]
        p95 = [r["p95_ms"] for r in rows]
        p99 = [r["p99_ms"] for r in rows]
        p999 = [r["p999_ms"] for r in rows]
        vus_peak = [r["vus_peak"] for r in rows]
        drops = [r["dropped_iterations"] for r in rows]
        bad = [r["bad_status"] for r in rows]
        s5xx = [r["status_5xx"] for r in rows]
        errors = [r["request_errors"] for r in rows]
        valid = [r["valid"] for r in rows]
        invalid_reasons = []
        for r in rows:
            if r["valid"]:
                continue
            reasons = []
            if r["checks_passed"] != r["requests"] * 2:
                reasons.append(
                    f"checks_passed={r['checks_passed']} != {r['requests']*2}"
                )
            if r["dropped_iterations"] > 0:
                reasons.append(f"dropped_iterations={r['dropped_iterations']}")
            if r["status_5xx"] > 0:
                reasons.append(f"status_5xx={r['status_5xx']}")
            if r["request_errors"] > 0:
                reasons.append(f"request_errors={r['request_errors']}")
            if r["bad_status"] > 0:
                reasons.append(f"bad_status={r['bad_status']}")
            if r["k6_exit_code"] != 0:
                reasons.append(f"k6_exit_code={r['k6_exit_code']}")
            invalid_reasons.append(
                {"run": r["run"], "reasons": reasons or ["unknown"]}
            )
        return {
            "name": name,
            "url": rows[0]["url"],
            "repeats": len(rows),
            "valid_count": sum(1 for v in valid if v),
            "total_count": len(rows),
            "all_valid": all(valid),
            "achieved_rps_median": median(achieved),
            "achieved_rps_min": min(achieved),
            "achieved_rps_max": max(achieved),
            "achieved_rps_stdev": stdev(achieved),
            "p95_ms_median": median(p95),
            "p95_ms_min": min(p95),
            "p95_ms_max": max(p95),
            "p95_ms_stdev": stdev(p95),
            "p99_ms_median": median(p99),
            "p99_ms_min": min(p99),
            "p99_ms_max": max(p99),
            "p99_ms_stdev": stdev(p99),
            "p999_ms_median": median(p999),
            "p999_ms_min": min(p999),
            "p999_ms_max": max(p999),
            "vus_peak_max": max(vus_peak),
            "vus_peak_min": min(vus_peak),
            "vus_configured": rows[0]["vus_configured"],
            "dropped_iterations_total": sum(drops),
            "status_5xx_total": sum(s5xx),
            "bad_status_total": sum(bad),
            "request_errors_total": sum(errors),
            "invalid_reasons": invalid_reasons,
        }

    summaries = [
        summarize(name, sorted(rows, key=lambda r: r["run"]))
        for name, rows in sorted(by_target.items())
    ]

    relays_dir = pathlib.Path("/home/ctown/projects/relayfold")
    biota_dir = pathlib.Path("/home/ctown/projects/biota")

    result = {
        "description": "Local-matrix cross-gateway head-to-head at 2,000 offered rps; 5-second windows; 5 repeats; 256 VUs; k6 open-loop constant-arrival-rate. Every target preflighted to HTTP 200 + 1,024 bytes; one warmup at the same rate per target; 8 targets on a 16-core shared host (k6 pinned to cores 0-7, gateway proxies pinned to cores 8-11, native async upstream on cores 12-15).",
        "method": "k6 v0.54.0 constant-arrival-rate open-loop on a /1k route. Run is valid iff checks_passed == requests*2 AND dropped_iterations == 0 AND status_5xx == 0 AND request_errors == 0 AND bad_status == 0 AND k6_exit_code == 0. The harness (run_k6_campaign.sh) marks each row valid accordingly.",
        "offered_rps": manifest["rates"],
        "vus_configured": manifest["max_vus"],
        "duration_s": int(manifest["duration"].rstrip("s")),
        "warmup_s": int(manifest["warmup"].rstrip("s")),
        "repeats": manifest["repeats"],
        "generator": manifest["generator"],
        "started_utc": meta["started_utc"],
        "host": meta["host"],
        "platform": meta["platform"],
        "cpu_layout": meta["cpu_layout"],
        "target_manifest": meta["target_manifest"],
        "target_manifest_sha256": meta["target_manifest_sha256"],
        "relayfold_revision": meta["relayfold_revision"],
        "biota_revision": git_rev(biota_dir),
        "campaign_dir": str(campaign),
        "summary": summaries,
        "rows": target_rows,
    }

    k6_sha = sha256_file(args.k6_bin) if pathlib.Path(args.k6_bin).exists() else None
    proxy_bin = "/home/ctown/.cache/cargo-target/release/relayfold-biota-bench-proxy"
    upstream_bin = "/home/ctown/.cache/cargo-target/release/bench_upstream"
    proxy_sha = sha256_file(proxy_bin) if pathlib.Path(proxy_bin).exists() else None
    upstream_sha = sha256_file(upstream_bin) if pathlib.Path(upstream_bin).exists() else None

    image_digests = {
        "nginx": "nginx:1.25-alpine@sha256:516475cc129da42866742567714ddc681e5eed7b9ee0b9e9c015e464b4221a00",
        "haproxy": "haproxy:2.9-alpine@sha256:3e29449a6beed63262e36104adf531b4e41b359f61937303f5ea8607987b3748",
        "envoy": "envoyproxy/envoy:v1.31-latest@sha256:caa5b411be1633b90023592a34a7e010c933d6e60206c758f631485e53006865",
        "zentinel": "ghcr.io/zentinelproxy/zentinel:latest@sha256:6611f8894d8672b6ef05f3dd69caf512bfa104f6e35ce53e8446c4f44b88c91c",
        "pingap": "vicanso/pingap:latest@sha256:02bfe3cdcdbbef33c8aa810a515a5b80aff01d1afab1685d36ff10aeaa38367c",
        "traefik": "traefik:v3.3@sha256:2cd5cc75530c8d07ae0587c743d23eb30cae2436d07017a5ff78498b1a43d09f",
    }

    out_json.write_text(json.dumps(result, indent=2))
    result_sha = sha256_file(out_json)

    provenance = {
        "started_utc": meta["started_utc"],
        "host": meta["host"],
        "platform": meta["platform"],
        "cpu_layout": meta["cpu_layout"],
        "relayfold_revision": meta["relayfold_revision"],
        "relayfold_repo_root": str(relays_dir),
        "biota_revision": git_rev(biota_dir),
        "biota_repo_root": str(biota_dir),
        "target_manifest": meta["target_manifest"],
        "target_manifest_sha256": meta["target_manifest_sha256"],
        "k6_binary_path": args.k6_bin,
        "k6_binary_sha256": k6_sha,
        "k6_version": "v0.54.0 (commit/baba871c8a, go1.23.1, linux/amd64)",
        "relayfold_biota_bench_proxy_sha256": proxy_sha,
        "relayfold_biota_bench_proxy_path": proxy_bin,
        "bench_upstream_sha256": upstream_sha,
        "bench_upstream_path": upstream_bin,
        "incumbent_image_digests": image_digests,
        "campaign_dir": str(campaign),
        "manifest_path": str(manifest_path),
        "campaign_metadata_path": str(meta_path),
        "result_path": str(out_json),
        "result_sha256": result_sha,
        "command": (
            f"PERSISTENT=1 BENCHMARK_GENERATOR=k6 BENCHMARK_OUTPUT={campaign} "
            f"SPBENCH=/home/ctown/.cache/cargo-target/release/sang-codspeed "
            f"benchmarks/run_local_matrix.sh --rates 2000 --duration 5 --warmup 2 "
            f"--repeats 5 --vus 256"
        ),
        "spbench_run_id": "7071305:1156577",
        "validity_rule": (
            "row.valid iff checks_passed == requests*2 AND dropped_iterations == 0 "
            "AND status_5xx == 0 AND request_errors == 0 AND bad_status == 0 "
            "AND k6_exit_code == 0"
        ),
    }
    out_provenance.write_text(json.dumps(provenance, indent=2))

    print(f"wrote {out_json} ({out_json.stat().st_size} bytes, sha256={result_sha})")
    print(f"wrote {out_provenance} ({out_provenance.stat().st_size} bytes)")
    print()
    print("Per-target summary (median over 5 repeats):")
    header = f"{'target':<20} {'ach_rps':>10} {'p95_ms':>10} {'p99_ms':>10} {'p999_ms':>10} {'valid':>6} {'vus_peak':>9}"
    print(header)
    print("-" * len(header))
    for s in summaries:
        print(
            f"{s['name']:<20} "
            f"{s['achieved_rps_median']:>10.2f} "
            f"{s['p95_ms_median']:>10.3f} "
            f"{s['p99_ms_median']:>10.3f} "
            f"{s['p999_ms_median']:>10.3f} "
            f"{s['valid_count']}/{s['total_count']:<3} "
            f"{s['vus_peak_max']:>9}"
        )


if __name__ == "__main__":
    main()
