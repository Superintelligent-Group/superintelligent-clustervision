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
        # structuredContent as an object: Gemini CLI's MCP client otherwise parses the text "5" as JSON
        # and rejects the number ("expected record").
        reply(i, {"content": [{"type": "text", "text": str(s)}], "structuredContent": {"sum": s}, "isError": False})
    elif meth == "ping":
        reply(i, {})
    else:
        # Method not found, per JSON-RPC. MCP 2026-07-28 clients (Goose 1.54) open with
        # `server/discover` and fall back to `initialize` only on this error; a `{}` result fails them.
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": i, "error": {"code": -32601, "message": f"no method {meth}"}}) + "\n")
        sys.stdout.flush()
