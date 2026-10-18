# Compatibility promise

Starting with 0.18, scripts can rely on these surfaces across minor releases:

- Session-row field names and JSON types from `ls --json`, including the common row
  keys listed by `cv schema --json`.
- Message and block field names, types, discriminant values, and the existing minimal
  message defaults. Optional fields may be absent when the source records no value.
- Visible CLI command/argument names, argument types, accepted enumerated values,
  requiredness and existing defaults described by `cv schema --commands --json`.
- MCP tool names and existing input properties, types, requiredness and defaults.
- HTTP API method/path patterns accepted by the read, search, export and task routers.
- Exit 0 for success, 1 for operational failure, and 2 for caller mistakes.

Additive fields, optional arguments, tools, routes, and enum values are compatible.
Consumers should ignore fields they do not use and handle unfamiliar enum values.
Help text, human output layout, diagnostic wording, opaque harness extras, tool input
objects, and native transcript formats are outside this promise. HTTP response bodies
beyond the shared session/message JSON are not covered by this first gate; neither
are every query parameter nor every command's specialized output format.

When a command/flag or route is replaced, it keeps a pointer for at least one minor
release: CLI exit 2 or HTTP 410, naming the replacement. MCP tool deprecations need an
explicit documented migration because MCP has no HTTP 410 equivalent. Removing a
covered name requires an exact, reviewed gate exception and a test of the pointer;
changing the baseline to hide a removal is not an exception. Already retired names
remain governed by their documented removal versions.

## What the gate checks

`tools/compatibility-gate.sh` selects the highest reachable stable release tag, records
its commit, and builds it in a separate checkout and target directory. It adds the
same small private serde probe to that checkout: this exercises the released public
`Message`/`Block` implementations with rich and minimal fixtures; it is not a claim
that the baseline checkout is pristine. No baseline artifact is downloaded at runtime.
The candidate and baseline use the pinned toolchain and separate synthetic homes.

The comparison permits additions and prose changes, and rejects removals and changed
existing shapes/defaults. It compares the existing command and data schemas, actual
`ls`/`show` JSON, serialized messages containing every existing block type, real MCP
`tools/list`, HTTP probes derived from the routers' method/path patterns, and success,
operational-failure and usage-error exits. The schema field lists are also checked
against the actual row and serializer output. Extend the probe when adding contract
fields or block variants so the next release baseline has complete fixture coverage.
Every documented role, message kind and origin must also serialize to its documented
value: accepting the old name as an input alias does not preserve emitted JSON.
The checks cover these fixtures and
inventories; they do not prove arbitrary semantic equivalence for every transcript.

The one initial exception is the approved 0.18 waiting-command change: MCP `workflow`
removes unbounded `follow`; callers use bounded `for`. It applies only against
v0.17.0 during 0.18 development/release (including the development tree before its
Cargo version is bumped). CLI pipe/terminal behavior is covered separately by
`crates/cv/tests/announce.rs`.

Run the Python mutation tests with `python3 -I tools/test-compatibility.py`. Run the
full gate on a build host, with release tags available and an owned candidate target.
The build wrapper retains its own temporary checkout/target for inspection. Before
accepting a contract change, inspect the actual failure and update only its narrow
exception and pointer test when a deprecation was intended.

CI also exercises upgrade from the checksum-pinned, released 0.16.1 Linux binary over every
session-bearing corpus version. `tools/check-upgrade.py --baseline <old-cv> --candidate <new-cv>`
copies stores into an isolated home, compares upgraded and clean candidate rows/events and
streamed `show` hashes, and requires two unchanged index passes to report no changed sessions.
Stores that 0.16.1 did not support are reported as new discovery coverage. Scratch catalogs
remain available for diagnosis; transcript output is never dumped. Run this on the build host.
