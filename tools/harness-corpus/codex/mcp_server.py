"""A tiny stdio MCP server (newline-delimited JSON-RPC) with one tool, for the corpus MCP scenario."""
import json
import sys

TOOLS = [{"name": "add", "description": "Add two integers and return the sum.",
          "inputSchema": {"type": "object", "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}},
                          "required": ["a", "b"]}}]


def reply(i, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": i, "result": result}) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    if not line.strip():
        continue
    m = json.loads(line)
    meth, i = m.get("method"), m.get("id")
    if i is None:
        continue
    if meth == "initialize":
        reply(i, {"protocolVersion": m.get("params", {}).get("protocolVersion", "2025-06-18"),
                  "capabilities": {"tools": {}}, "serverInfo": {"name": "corpus", "version": "1.0.0"}})
    elif meth == "tools/list":
        reply(i, {"tools": TOOLS})
    elif meth == "tools/call":
        args = m.get("params", {}).get("arguments") or {}
        s = int(args.get("a", 0)) + int(args.get("b", 0))
        reply(i, {"content": [{"type": "text", "text": str(s)}], "isError": False})
    else:
        reply(i, {})
