#!/usr/bin/env python3
"""Compare locally built release/candidate contracts. No network or real harness stores."""
import argparse
import http.client
import json
import os
from pathlib import Path
import re
import selectors
import subprocess
import tempfile


class Incompatible(Exception):
    pass


def require(ok, message):
    if not ok:
        raise Incompatible(message)


def subset(old, new, path="contract"):
    """Additions are compatible; changing existing values, defaults or requiredness is not."""
    if isinstance(old, dict):
        require(isinstance(new, dict), f"{path}: object changed")
        for key, value in old.items():
            if key in {"description", "help", "about"} or (key == "required" and "properties" in old):
                continue
            require(key in new, f"{path}.{key}: removed")
            subset(value, new[key], f"{path}.{key}")
        # JSON Schema required lists describe input obligations, not output vocabulary.
        if "properties" in old:
            require(set(new.get("required", [])) <= set(old.get("required", [])), f"{path}: new required input")
    elif isinstance(old, list):
        require(isinstance(new, list), f"{path}: array changed")
        for value in old:
            def matches(candidate):
                try:
                    subset(value, candidate, path)
                    return True
                except Incompatible:
                    return False
            require(any(matches(candidate) for candidate in new), f"{path}: removed/changed {value!r}")
    else:
        require(old == new, f"{path}: {old!r} -> {new!r}")


def named(rows):
    require(len({r["name"] for r in rows}) == len(rows), "duplicate contract name")
    return {r["name"]: r for r in rows}


def shape(value, key=""):
    if key in {"extra", "input", "details"}:
        return "opaque JSON"
    if isinstance(value, dict):
        return {k: shape(v, k) for k, v in value.items()}
    if isinstance(value, list):
        unique = {json.dumps(shape(v), sort_keys=True) for v in value}
        return [json.loads(v) for v in sorted(unique)]
    if key in {"type", "role", "kind", "origin"} and isinstance(value, str):
        return "=" + value
    return "null" if value is None else type(value).__name__


def routes(source):
    """Read actual Rust routing slice patterns, including alternatives and task verb dispatch.

    This deliberately fails closed when a router moves: update this tiny adapter alongside it.
    It is an inventory check plus live probes, not a Rust parser or response-schema promise.
    """
    source = Path(source)
    core = (source / "crates/cv-core/src/api.rs").read_text().split("pub fn route_json(", 1)[1].split("pub fn route_staged(", 1)[0]
    serve = (source / "crates/cv/src/cmd/serve.rs").read_text()
    reads = serve.split("fn route(", 1)[1].split("// ---", 1)[0]
    export = serve.split("fn download_export(", 1)[1].split("Some((||", 1)[0]
    tasks = (source / "crates/cv/src/cmd/serve_tasks.rs").read_text()
    task_routes = tasks.split("pub(crate) fn route(", 1)[1].split("fn api_", 1)[0]
    patterns = lambda text: re.findall(r'\[\s*"api"\s*,[^\]]+\]', text)
    def normalize(pattern):
        return "/" + "/".join(token.strip()[1:-1] if token.strip().startswith('"') else
                               "*" if token.strip() == ".." else "{}"
                               for token in pattern[1:-1].split(',') if token.strip())
    result = {"GET " + normalize(p) for p in patterns(core + reads + export)}
    for post, pattern in re.findall(r'\((true|false),\s*(\[\s*"api"\s*,[^\]]+\])\)', task_routes):
        route = ("POST " if post == "true" else "GET ") + normalize(pattern)
        if route == "POST /api/task/{}/{}":
            verbs = re.findall(r'^\s*"([a-z_]+)"\s*=>', tasks.split("match verb {", 1)[1], re.M)
            require(bool(verbs), "task action inventory empty")
            result.update("POST /api/task/{}/" + verb for verb in verbs)
        else:
            result.add(route)
    require("GET /api/sessions" in result and "POST /api/task/{}/resolve" in result, "routing inventory incomplete")
    return sorted(result)


def data_contract(schema):
    # Protect machine vocabularies/field inventories, not incidental prose in the reference.
    keys = {"session_row": ("keys", "enrich_keys", "search_extra_keys"),
            "session": ("fields", "lineage"), "message": ("fields", "role", "kind", "origin", "usage"),
            "block": ("tag", "types")}
    return {section: {key: schema[section][key] for key in fields} for section, fields in keys.items()}


def world(root):
    root = Path(root)
    home = root / "home"
    projects = home / ".claude/projects/fixture"
    projects.mkdir(parents=True)
    records = [
        {"type": "user", "uuid": "u", "cwd": "/compat", "timestamp": "2026-01-01T00:00:00Z",
         "message": {"role": "user", "content": "hello"}},
        {"type": "assistant", "uuid": "a", "timestamp": "2026-01-01T00:00:01Z",
         "message": {"role": "assistant", "model": "fixture", "content": [{"type": "text", "text": "hello"}]}},
    ]
    (projects / "compat-session.jsonl").write_text("".join(json.dumps(r) + "\n" for r in records))
    return {"PATH": os.environ["PATH"], "HOME": str(home), "CLUSTERVISION_HOME": str(root / "catalog"),
            "CLAUDE_CONFIG_DIR": str(home / ".claude"), "CODEX_HOME": str(home / ".codex"),
            "XDG_CACHE_HOME": str(root / "cache"), "XDG_CONFIG_HOME": str(home / ".config"),
            "XDG_DATA_HOME": str(home / ".local/share"), "NO_COLOR": "1"}


def invoke(binary, env, args, code=0, stdin=None):
    p = subprocess.run([str(binary), *args], env=env, input=stdin, capture_output=True, text=True, timeout=30)
    require(p.returncode == code, f"{args}: exit {p.returncode}, expected {code}: {p.stderr[-500:]}")
    return p.stdout


def http_routes(binary, env, inventory):
    process = subprocess.Popen([str(binary), "serve", "--port", "0"], env=env,
                               stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    try:
        selector = selectors.DefaultSelector()
        selector.register(process.stderr, selectors.EVENT_READ)
        require(bool(selector.select(10)), "serve did not announce its bound address")
        banner = process.stderr.readline()
        match = re.search(r'http://([^: ]+):(\d+)', banner)
        require(match is not None, f"serve banner: {banner!r}")
        host, port = match.group(1, 2)
        responses = {}
        for route in inventory:
            method, template = route.split(" ", 1)
            path = template
            if path.startswith("/api/session/{}/{}"):
                path = path.replace("/api/session/{}/{}", "/api/session/claude/compat-session", 1)
            path = path.replace("{}", "missing").replace("*", "missing")
            connection = http.client.HTTPConnection(host, int(port), timeout=10)
            try:
                connection.request(method, path, body=json.dumps({"who": "compat", "text": "fixture"}) if method == "POST" else None,
                                   headers={"Content-Type": "application/json"})
                response = connection.getresponse()
                body = response.read().decode()
                if response.status == 410:
                    require(bool(re.search(r"/api/|cv ", body)), f"{route}: 410 must point to its replacement")
                responses[route] = {"status": response.status, "pointer": body if response.status == 410 else ""}
                require(response.status < 500, f"{route}: server failure {response.status}: {body[:200]}")
            finally:
                connection.close()
        return responses
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
        process.stderr.close()


def collect(binary, probe, source, root):
    env = world(root)
    commands = json.loads(invoke(binary, env, ["schema", "--commands", "--json"]))
    schema = json.loads(invoke(binary, env, ["schema", "--json"]))
    rows = json.loads(invoke(binary, env, ["ls", "--json"]))
    require(isinstance(rows, list) and len(rows) == 1, "fixture must discover exactly one session")
    session = json.loads(invoke(binary, env, ["show", "compat-session", "--json"]))
    messages = json.loads(invoke(probe, env, [], stdin=json.dumps(schema["message"])))
    requests = "\n".join(json.dumps({"jsonrpc": "2.0", "id": i, "method": method, "params": params})
                         for i, method, params in [(1, "initialize", {"protocolVersion": "2025-06-18"}), (2, "tools/list", {})]) + "\n"
    responses = [json.loads(line) for line in invoke(binary, env, ["mcp"], stdin=requests).splitlines()]
    tools = next(r["result"]["tools"] for r in responses if r.get("id") == 2)
    # The existing schema's field inventories must agree with actual serialized fields.
    require(set(schema["session_row"]["keys"]) <= rows[0].keys(), "session-row schema drifted from ls JSON")
    rich = messages[0]
    require(set(schema["message"]["fields"]) == rich.keys(), "message schema drifted from real serde")
    require(set(schema["block"]["types"]) == {b["type"] for b in rich["content"]}, "probe must cover every block tag")
    for block in rich["content"]:
        require(set(schema["block"]["types"][block["type"]]) <= block.keys(), f"block field missing: {block['type']}")
    invoke(binary, env, ["show", "absent-session"], code=2)
    invoke(binary, env, ["--not-a-cv-flag"], code=2)
    # Failure to create an archive under an ordinary file is an operational error, not usage.
    denied = Path(root) / "not-a-directory"
    denied.write_text("fixture")
    failure_env = dict(env, CLUSTERVISION_HOME=str(denied / "catalog"))
    invoke(binary, failure_env, ["task", "open", "compat operational failure"], code=1)
    return {"commands": commands, "schema": data_contract(schema), "rows": shape(rows), "session": shape(session),
            "messages": shape(messages), "tools": tools, "routes": http_routes(binary, env, routes(source)),
            "exits": [0, 1, 2]}


def compare(old, new, baseline_tag, candidate_version):
    # G1 was explicitly approved for 0.18: bounded MCP workflow calls replace unbounded follow.
    # Cargo still says 0.17.0 during 0.18 development; that exact version is included.
    # Exact exceptions are populated from the reviewed v0.17.0 -> 0.18 change, not inferred.
    old = json.loads(json.dumps(old))
    new = json.loads(json.dumps(new))
    if baseline_tag == "v0.17.0" and (candidate_version == "0.17.0" or candidate_version.startswith("0.18.")):
        for tool in old["tools"]:
            if tool["name"] == "workflow":
                replacement = named(new["tools"]).get("workflow", {}).get("inputSchema", {}).get("properties", {})
                if "follow" in tool["inputSchema"].get("properties", {}) and "follow" not in replacement:
                    require(replacement.get("for", {}).get("type") == "string", "G1 workflow follow removal needs bounded for")
                    tool["inputSchema"]["properties"].pop("follow")
    for key in ("commands", "tools"):
        before, after = named(old[key]), named(new[key])
        for name, value in before.items():
            require(name in after, f"{key}.{name}: removed")
            if key == "commands":
                value["args"] = named(value["args"])
                after[name]["args"] = named(after[name]["args"])
                for arg, spec in after[name]["args"].items():
                    require(arg in value["args"] or not spec["required"], f"{name}.{arg}: new required argument")
            subset(value, after[name], f"{key}.{name}")
    for key in ("schema", "rows", "session", "messages", "exits"):
        subset(old[key], new[key], key)
    for route, response in old["routes"].items():
        require(route in new["routes"], f"HTTP route removed: {route}")
        require(response["status"] == new["routes"][route]["status"], f"HTTP status changed: {route}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("baseline", "candidate", "baseline-probe", "candidate-probe", "baseline-source", "candidate-source", "baseline-tag", "candidate-version"):
        parser.add_argument("--" + name, required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="cv-compat-") as root:
        before = collect(Path(args.baseline).resolve(), Path(args.baseline_probe).resolve(), args.baseline_source, Path(root) / "baseline")
        after = collect(Path(args.candidate).resolve(), Path(args.candidate_probe).resolve(), args.candidate_source, Path(root) / "candidate")
        compare(before, after, args.baseline_tag, args.candidate_version)
    print(f"compatible with {args.baseline_tag}: CLI/schema, real JSON/serde, MCP, HTTP routes, exits")


if __name__ == "__main__":
    try:
        main()
    except (Incompatible, subprocess.TimeoutExpired) as error:
        raise SystemExit(str(error))
