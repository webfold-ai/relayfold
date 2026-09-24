#!/usr/bin/env python3
"""Check Streamable HTTP MCP behavior before considering throughput numbers."""

import argparse
import json
import pathlib
import statistics
import time
import urllib.error
import urllib.request


def decode(body, content_type):
    if "text/event-stream" in content_type:
        events = [line[6:] for line in body.decode().splitlines() if line.startswith("data: ")]
        if not events:
            raise ValueError("SSE response had no data event")
        return json.loads(events[-1])
    return json.loads(body)


def request(url, payload, session=None, version=None, token=None):
    headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}
    if session:
        headers["Mcp-Session-Id"] = session
    if version:
        headers["MCP-Protocol-Version"] = version
    if token:
        headers["Authorization"] = f"Bearer {token}"
    req = urllib.request.Request(url, data=json.dumps(payload).encode(), headers=headers, method="POST")
    start = time.perf_counter()
    with urllib.request.urlopen(req, timeout=20) as response:
        body = response.read()
        elapsed = time.perf_counter() - start
        content_type = response.headers.get("Content-Type", "")
        return (decode(body, content_type) if body else None, response.headers.get("Mcp-Session-Id"),
                elapsed, len(body), response.status)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("url")
    parser.add_argument("--calls", type=int, default=20)
    parser.add_argument("--meta-server", help="Backend name for a compact gateway's gateway_invoke tool")
    parser.add_argument("--token")
    parser.add_argument("--output", type=pathlib.Path)
    args = parser.parse_args()
    if args.calls < 1:
        parser.error("calls must be positive")
    output = {"url": args.url, "calls_requested": args.calls, "status": "failed"}
    try:
        init, session, _, _, _ = request(args.url, {"jsonrpc": "2.0", "id": 1,
            "method": "initialize", "params": {"protocolVersion": "2025-06-18",
            "capabilities": {}, "clientInfo": {"name": "relayfold-bench", "version": "0.1"}}}, token=args.token)
        version = init["result"]["protocolVersion"]
        request(args.url, {"jsonrpc": "2.0", "method": "notifications/initialized"}, session, version, args.token)
        listed, _, list_time, list_bytes, _ = request(args.url, {"jsonrpc": "2.0", "id": 2,
            "method": "tools/list", "params": {}}, session, version, args.token)
        tools = listed["result"]["tools"]
        if not tools:
            raise ValueError("no tools returned")
        meta = args.meta_server is not None
        name = "gateway_invoke" if meta else next(
            (tool["name"] for tool in tools if tool["name"].endswith("echo_000")), tools[0]["name"])
        search_ms = None
        if meta:
            if not {"gateway_search_tools", "gateway_invoke"}.issubset({tool["name"] for tool in tools}):
                raise ValueError("gateway does not expose search and invoke meta-tools")
            found, _, search_elapsed, _, _ = request(args.url, {"jsonrpc": "2.0", "id": 3,
                "method": "tools/call", "params": {"name": "gateway_search_tools",
                "arguments": {"query": "echo_000", "limit": 5}}}, session, version, args.token)
            if "echo_000" not in json.dumps(found.get("result", {})):
                raise ValueError("search did not discover echo_000")
            search_ms = round(search_elapsed * 1000, 3)
        latencies = []
        for index in range(args.calls):
            value = f"relayfold-{index}"
            arguments = ({"server": args.meta_server, "tool": "echo_000", "arguments": {"value": value}}
                         if meta else {"value": value})
            called, _, elapsed, _, _ = request(args.url, {"jsonrpc": "2.0", "id": index + 4,
                "method": "tools/call", "params": {"name": name, "arguments": arguments}},
                session, version, args.token)
            if value not in json.dumps(called.get("result", {})):
                raise ValueError(f"tool call {index} did not echo its input: {called}")
            latencies.append(elapsed * 1000)
        ordered = sorted(latencies)
        output.update(status="passed", protocol_version=version, session=bool(session),
                      tool_count=len(tools), list_bytes=list_bytes, list_ms=round(list_time * 1000, 3),
                      mode="meta" if meta else "raw", search_ms=search_ms,
                      tool=name, call_p50_ms=round(statistics.median(latencies), 3),
                      call_p95_ms=round(ordered[max(0, int(0.95 * len(ordered) + 0.999) - 1)], 3),
                      calls_passed=len(latencies))
    except Exception as error:
        output["error"] = str(error)
    print(json.dumps(output, indent=2))
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(output, indent=2) + "\n")
    if output["status"] != "passed":
        raise SystemExit(1)


if __name__ == "__main__":
    main()
