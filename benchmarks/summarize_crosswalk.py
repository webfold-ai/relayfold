#!/usr/bin/env python3
"""Render the capability/workload crosswalk without turning unknowns into passes."""
import argparse
import json
from pathlib import Path

def main():
    p = argparse.ArgumentParser()
    p.add_argument("crosswalk", type=Path)
    args = p.parse_args()
    data = json.loads(args.crosswalk.read_text())
    print("candidate\tclass\thttp_target\tsource")
    for c in data["candidates"]:
        print(f"{c['id']}\t{c['class']}\t{c['http_target'] or 'unprovisioned'}\t{c['source']}")
    print("\nscenario\tplane\tmetrics")
    for s in data["scenarios"]:
        print(f"{s['id']}\t{s['plane']}\t{','.join(s['metrics'])}")

if __name__ == "__main__":
    main()
