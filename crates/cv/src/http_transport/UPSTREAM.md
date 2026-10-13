# Private HTTP transport provenance

Imported from tiny_http 0.12.0, the released source at https://crates.io/crates/tiny_http/0.12.0 and https://github.com/tiny-http/tiny-http/releases/tag/0.12.0 (release commit 212b1c4).
Crate archive SHA256: `389915df6413a2e74fb181895f933386023c71110878cd0825588928e64cdc82`. Original MIT and Apache-2.0 license files are retained here.

The module ships inside the CLI crate so packaging cannot restore an unpatched registry dependency. It creates no public package or protocol. Published dependencies remain ascii 1, chunked_transfer 1, httpdate 1 and log 0.4, already in the prior dependency graph.

Mechanical adaptations: lib.rs becomes mod.rs; crate-root paths point to crate::http_transport; root imports use self; only the existing non-TLS branches are retained, with the three unused TLS implementation files/dependencies omitted. Released HTTP parsing, responses and keepalive behavior remain intact. Upstream style/API lint allowances are local to this private module, as dependency builds previously capped those lints.

Functional delta: TaskPool reserves idle capacity for queued connection jobs. Shared dispatch_locked is the same production enqueue path; a deterministic test holds its mutex across a burst to model accepted connections arriving before notified workers reacquire it. A live socket test keeps preconnections and persistent HTTP owners open while a new connection responds and remains reusable. No timeout increase or keepalive disable.

The exact Rust-source adaptation is recorded in UPSTREAM.patch against released files; UPSTREAM-FILES.json pins their original SHA256s. The scheduling predicate changes from `waiting_tasks == 0` to `queue.len() >= waiting_tasks`, so each queued task already reserves one idle worker. Queue and waiting-state transitions remain under the original mutex. Worker growth/reclamation and HTTP keepalive settings otherwise remain unchanged.
