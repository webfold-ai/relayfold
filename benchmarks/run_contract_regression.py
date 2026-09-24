#!/usr/bin/env python3
"""Run identical gateway contract cases against every provisioned target."""
import argparse
import datetime as dt
import json
import pathlib
import urllib.error
import urllib.request

def main():
    p = argparse.ArgumentParser()
    p.add_argument("targets", type=pathlib.Path)
    p.add_argument("--cases", type=pathlib.Path, default=pathlib.Path("benchmarks/regression-cases.json"))
    p.add_argument("--output", type=pathlib.Path, default=pathlib.Path("benchmarks/results"))
    args = p.parse_args()
    targets = json.loads(args.targets.read_text())["targets"]
    cases = json.loads(args.cases.read_text())["cases"]
    rows = []
    for target in targets:
        for case in cases:
            url = target["url"].split("/1k")[0] + case["path"]
            try:
                with urllib.request.urlopen(urllib.request.Request(url, method=case["method"]), timeout=10) as response:
                    body = response.read()
                    status = response.status
            except urllib.error.HTTPError as error:
                status, body = error.code, error.read()
            except Exception as error:
                rows.append({"target": target["name"], "case": case["id"], "status": "error", "error": str(error)})
                continue
            passed = status == case["expect_status"] and ("expect_bytes" not in case or len(body) == case["expect_bytes"])
            rows.append({"target": target["name"], "case": case["id"], "status": status, "bytes": len(body), "passed": passed})
    out = args.output / ("contract-" + dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ") + ".json")
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps({"cases": cases, "results": rows}, indent=2) + "\n")
    print(out)
    print(f"passed={sum(r.get('passed', False) for r in rows)} total={len(rows)}")
    raise SystemExit(0 if all(r.get("passed", False) for r in rows) else 1)

if __name__ == "__main__":
    main()
