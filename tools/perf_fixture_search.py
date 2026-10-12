"""Production sweep/semantic oracle input; payload generation uses bounded scratch."""


def generate_sweep(root, size, escaped):
    path = root / "sweep.jsonl"
    prefix = (b'{"type":"user","sessionId":"perf","uuid":"user","cwd":"/perf",'
              b'"message":{"role":"user","content":"Inspect the file"}}\n'
              b'{"type":"assistant","sessionId":"perf","uuid":"assistant",'
              b'"message":{"role":"assistant","model":"test","content":[{"type":"tool_use",'
              b'"id":"call","name":"Edit","input":{"file_path":"/perf/a.rs","content":"')
    suffix = b'","metadata":{"n":3,"flag":true}}}]}}\n'
    unit = b'\\u0078' if escaped else b'x'
    with path.open("xb") as out:
        out.write(prefix)
        for start in range(0, size, 65536):
            out.write(unit * min(65536, size - start))
        out.write(suffix)
    return {"harness": "claude", "id": "perf", "path": str(path), "root": str(root),
            "shape": "sweep-arguments", "records": 1, "expected_messages": 2,
            "decoded_payload_bytes": size, "physical_input_bytes": path.stat().st_size,
            "escaped": escaped, "synthetic": True,
            "provenance": "native Claude envelope and independent production sweep body oracle"}
