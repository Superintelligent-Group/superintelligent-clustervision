# clustervision — desktop app (Tauri v2)

A window around clustervision's **static web UI** (session viewer, timeline, compare, stats,
splice composer, fleet dashboard): sessions read through the in-process read API, **native zip
ingest** from File → Open, a **native application menu**, **persisted window state**, and an
auto-launched local `cv serve`.

## Layout

```
app/
├── README.md            ← this file
└── src-tauri/           ← the Tauri v2 Rust app (its OWN cargo workspace)
    ├── Cargo.toml       ← declares `[workspace]` so it's EXCLUDED from the repo-root workspace
    ├── build.rs         ← tauri-build codegen
    ├── tauri.conf.json  ← v2 config; identifier `town.lunar.clustervision`
    ├── capabilities/
    │   └── default.json ← grants core + shell:allow-open + dialog:default to the main window
    ├── icons/           ← app icon set (crystal-ball theme)
    └── src/
        ├── main.rs      ← thin binary entry point
        └── lib.rs       ← Tauri runtime: the `api` command, menu, zip open, cv-serve lifecycle
```

### Why its own workspace
`src-tauri/Cargo.toml` has a top-level `[workspace]` table. That makes it a **standalone
cargo workspace**, so it is *excluded* from the main clustervision workspace at the repo
root. Building or CI-testing the main crates never compiles the app (which pulls in the
heavy WebKit/Tauri stack), and vice-versa. The app still depends on the real crates by
relative path: `cv-core` at `../../crates/cv-core`. Note that `cv-core`'s
**package** name is `clustervision-core` (renamed for crates.io in 0.10), so the manifest says
`cv-core = { path = "../../crates/cv-core", package = "clustervision-core" }` — the same form
`crates/cv/Cargo.toml` uses. Asking for a bare `cv-core` fails resolution instantly, and because
this workspace is excluded from the root one, that went unnoticed for a whole release. CI's
`desktop` job now compiles and tests it on every push.

### Frontend
No build step. `frontendDist` in `tauri.conf.json` points at the existing static web app
(`../../web`). The desktop window simply loads `web/index.html`. We do **not** modify `web/`.

## The native command: `api`

The desktop app answers session reads **natively** instead of fetching `http://localhost:7777`
(the webview's cross-origin fetch to localhost is unreliable). It has exactly one command:

| Command | Args | Returns |
|---------|------|---------|
| `api`   | `{ path: string, query?: string }` | the JSON body (a string to `JSON.parse`), or rejects with `"<status>: <error>"` |

`path` is any read-API path (`/api/sessions`, `/api/session/claude/<id>/messages`, …) and
`query` its raw query string. It calls `cv_core::api::route_json`, the same function `cv serve`
answers `/api/*` with, so a session cannot look different between the server and the app; the
routes and shapes are the ones in the manual's [server chapter](../manual/src/serving.md#routes).
The web UI calls it through `apiGet(path, params)` in `web/components/hydrate.js`, which uses
`fetch` against `cv serve` in a browser instead.

Generation (the splice composer's ⚡) is the web UI's OpenRouter path in the app too;
`cv splice --generate` is the CLI's, with `LMSTUDIO_API_BASE` for a free local model.

## Native application menu

The app installs a native [`Menu`](https://docs.rs/tauri/2/tauri/menu/) in `setup` (handled in
`on_menu_event`):

- **File → Open zip…** (`Cmd/Ctrl+O`) — native file dialog (`tauri-plugin-dialog`) → in-memory
  unzip (the `zip` crate, deflate only) + the same `cv_core::ingest::ingest_files` the browser's
  WASM path uses → **emits the `cv://open-sessions` Tauri event** with the `Session[]` JSON
  payload. The `web/` UI can `listen("cv://open-sessions", …)` to load the sessions without WASM.
  Ingest/read errors surface in a native error dialog. The picker + ingest run off the main
  thread so the event loop never blocks.
- **File → Quit** — predefined quit item.
- **View → Reload** (`Cmd/Ctrl+R`) reloads the webview, **Toggle DevTools** (`Cmd/Ctrl+Alt+I`;
  the `tauri` `devtools` feature is enabled so this works in release bundles too),
  **Toggle Fullscreen**, and **Toggle cv serve** (start/stop the sidecar at runtime).
- **Help → About clustervision** (predefined, with name/version/description) and
  **Open the web demo** (opens the public demo in the default browser via `shell:allow-open`).

The native dialog is driven from **Rust** (`app.dialog().file()…`), so no JS-side permission is
strictly required; `dialog:default` is still granted in `capabilities/default.json` for any
future JS use.

## Window state

Window size/position (and maximized/fullscreen flags) persist across launches via the official
[`tauri-plugin-window-state`](https://docs.rs/tauri-plugin-window-state) — registered as
`tauri_plugin_window_state::Builder::new().build()`. It writes a small `.window-state.json` in
the app config dir, restores on startup, and saves on close automatically. No manual save/restore
code needed.

## cv serve wiring

On startup the app **spawns `cv serve --port 7777`** so the bundled fleet dashboard (which
fetches `http://localhost:7777`) works unchanged. The child handle is stored in Tauri managed
state and **killed on app exit** (`RunEvent::Exit`).

**Approach chosen: `std::process::Command`** (not a bundled sidecar) — simplest thing that
compiles and runs with no extra bundling step. Binary resolution order:

1. the repo's build output: `../../target/release/cv`, then `../../target/debug/cv`
2. `cv` on `PATH`

So in dev you only need:

```sh
cargo build -p clustervision   # from the repo root → target/debug/cv
# or: cargo build -p clustervision --release
```

If `cv` can't be found, the app still runs; only the live fleet dashboard is unavailable
(a clear note is logged to stderr). You can also start/stop the sidecar at runtime from
**View → Toggle cv serve**. The child is always killed on app exit (`RunEvent::Exit`).

> **Optional: make it a real sidecar for a self-contained bundle.** Build `cv`
> (`cargo build -p clustervision --release`), copy it to `app/src-tauri/binaries/cv-<target-triple>`
> (e.g. `cv-aarch64-apple-darwin`), add `"externalBin": ["binaries/cv"]` under `bundle` in
> `tauri.conf.json`, add the `shell:allow-execute`/sidecar permission to `capabilities/default.json`,
> and spawn it via `tauri_plugin_shell`'s sidecar API instead of `std::process::Command`. The
> `std::process` path above is intentionally the default because it needs no bundle prep.

## Build & run

From `app/src-tauri/`:

```sh
# Compile-check the Rust (fast; no bundling, no system bundler deps):
cargo build

# Run the app in dev (opens the window, loads web/, launches cv serve):
cargo tauri dev

# Produce a distributable bundle (.app / .dmg on macOS, etc.):
cargo tauri build
```

Requirements:
- **Rust** (stable or nightly) + `tauri-cli` v2 (`cargo install tauri-cli --version '^2'`).
- A **WebKit-based webview**: macOS uses the system WebKit (already present); Linux needs
  `libwebkit2gtk-4.1-dev` + `libgtk-3-dev` etc.; Windows uses WebView2.
- For the live fleet dashboard: a built `cv` (see [cv serve wiring](#cv-serve-wiring)).

## Verified in this environment

- `cargo build` **and** `cargo build --release` in `app/src-tauri` **compile cleanly** — Tauri v2
  (with the `devtools` feature) + system WebKit on macOS; the dialog, window-state, and shell
  plugins; the `zip` crate; and cv-core via a relative path. No warnings from
  `clustervision-app` itself.
- The app is **excluded from the main repo workspace** (`cargo metadata` at the repo root does
  not list `clustervision-app`).

A full `cargo tauri dev` / `cargo tauri build` bundle (.app/.dmg) needs a display and (for
distribution) codesigning, so it was not produced here — run it on your machine with `tauri-cli`
installed (`cargo install tauri-cli --version '^2'`). Getting it to **compile** with the menu,
dialog, native ingest, and window-state is what's verified above.
