#!/usr/bin/env python3
"""Sample Linux process CPU and RSS while another benchmark is running."""
import argparse
import json
import os
import pathlib
import time

def read_stat(pid):
    fields = pathlib.Path(f"/proc/{pid}/stat").read_text().split()
    rss_pages = int(fields[23])
    ticks = int(fields[13]) + int(fields[14])
    return ticks, rss_pages * os.sysconf("SC_PAGE_SIZE")

def main():
    p = argparse.ArgumentParser()
    p.add_argument("pid", type=int)
    p.add_argument("--interval", type=float, default=0.25)
    p.add_argument("--output", type=pathlib.Path, required=True)
    args = p.parse_args()
    hz = os.sysconf("SC_CLK_TCK")
    samples = []
    previous = None
    while True:
        try:
            ticks, rss = read_stat(args.pid)
        except (FileNotFoundError, IndexError):
            break
        now = time.monotonic()
        cpu = None if previous is None else (ticks - previous[0]) / hz / max(now - previous[1], 1e-9) * 100
        samples.append({"monotonic": now, "cpu_percent_one_core": cpu, "rss_bytes": rss})
        previous = (ticks, now)
        time.sleep(args.interval)
    args.output.write_text(json.dumps({"pid": args.pid, "interval_s": args.interval, "samples": samples}, indent=2) + "\n")
    print(args.output)

if __name__ == "__main__":
    main()
