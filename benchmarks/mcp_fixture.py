#!/usr/bin/env python3
"""Deterministic stdio MCP backend for gateway correctness and catalog tests."""

import json
import os
import sys

count = int(os.environ.get("MCP_FIXTURE_TOOLS", "100"))
tools = [{"name": f"echo_{index:03d}", "description": "Return one string unchanged",
          "inputSchema": {"type": "object", "properties": {"value": {"type": "string"}},
                          "required": ["value"]}} for index in range(count)]

for line in sys.stdin:
    try:
        request = json.loads(line)
        method = request.get("method")
        if "id" not in request:
            continue
        if method == "initialize":
            result = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                      "serverInfo": {"name": "relayfold-bench-fixture", "version": "0.1"}}
        elif method == "tools/list":
            result = {"tools": tools}
        elif method == "tools/call":
            name = request.get("params", {}).get("name")
            value = request.get("params", {}).get("arguments", {}).get("value", "")
            if name not in {tool["name"] for tool in tools}:
                result = {"isError": True, "content": [{"type": "text", "text": "unknown tool"}]}
            else:
                result = {"content": [{"type": "text", "text": str(value)}]}
        else:
            result = {"error": {"code": -32601, "message": "method not found"}}
        response = {"jsonrpc": "2.0", "id": request["id"]}
        if "error" in result:
            response.update(result)
        else:
            response["result"] = result
        sys.stdout.write(json.dumps(response, separators=(",", ":")) + "\n")
        sys.stdout.flush()
    except Exception as error:
        print(f"fixture error: {error}", file=sys.stderr, flush=True)
