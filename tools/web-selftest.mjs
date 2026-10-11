// Run web/selftest.html headless and fail on any failed check or page error.
//
//   node tools/web-selftest.mjs [web-dir]          (default: web/ beside this script's repo)
//
// The page needs only a static server (browsers refuse ES modules over file://), so this serves
// the directory itself on an ephemeral loopback port; no `cv` binary is involved. Playwright is
// resolved through NODE_PATH, so it lives wherever the machine keeps it:
//
//   NODE_PATH=~/tools/playwright/node_modules node tools/web-selftest.mjs        # a local install
//   SELFTEST_CHANNEL=chrome node tools/web-selftest.mjs                          # the system Chrome
//
// With SELFTEST_CHANNEL set, `playwright-core` (no bundled browser) is enough; CI uses the Chrome
// that ubuntu-latest ships.
import { createServer } from "node:http";
import { createRequire } from "node:module";
import { readFile } from "node:fs/promises";
import { extname, join, normalize, resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
function playwright() {
  for (const name of ["playwright", "playwright-core"]) {
    try {
      return require(name);
    } catch {}
  }
  console.error("web-selftest: no playwright on NODE_PATH (npm i playwright-core, then NODE_PATH=…/node_modules)");
  process.exit(2);
}

const root = resolve(process.argv[2] ?? join(dirname(fileURLToPath(import.meta.url)), "..", "web"));
const TYPES = {
  ".html": "text/html", ".js": "text/javascript", ".mjs": "text/javascript", ".css": "text/css",
  ".json": "application/json", ".wasm": "application/wasm", ".png": "image/png", ".svg": "image/svg+xml",
};
const server = createServer(async (req, res) => {
  const path = normalize(decodeURIComponent(new URL(req.url, "http://x").pathname));
  const file = join(root, path);
  if (!file.startsWith(root)) return res.writeHead(403).end();
  try {
    const body = await readFile(file);
    res.writeHead(200, { "content-type": TYPES[extname(file)] ?? "application/octet-stream" }).end(body);
  } catch {
    res.writeHead(404).end();
  }
});
await new Promise((ok) => server.listen(0, "127.0.0.1", ok));
const { port } = server.address();

const { chromium } = playwright();
const channel = process.env.SELFTEST_CHANNEL || undefined;
const browser = await chromium.launch(channel ? { channel } : {});
try {
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  await page.goto(`http://127.0.0.1:${port}/selftest.html`);
  await page.waitForFunction(() => globalThis.__CV_SELFTEST__, null, { timeout: 30000 });
  const r = await page.evaluate(() => globalThis.__CV_SELFTEST__);
  console.log(`web selftest: ${r.total - r.failed}/${r.total} passing`);
  for (const f of r.failures) console.log(`  FAIL ${f}`);
  for (const e of errors) console.log(`  PAGE ERROR ${e}`);
  process.exitCode = r.failed || errors.length ? 1 : 0;
} finally {
  await browser.close();
  server.close();
}
