// Exercise the served inbox page against controlled API failures and out-of-order responses.
// NODE_PATH=~/tools/playwright/node_modules SELFTEST_CHANNEL=chrome node tools/inbox-selftest.mjs
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
let playwright;
for (const name of ['playwright', 'playwright-core']) { try { playwright = require(name); break; } catch {} }
if (!playwright) throw new Error('Playwright must be on NODE_PATH');
const html = await readFile(new URL('../crates/cv/src/cmd/serve_tasks_page.html', import.meta.url));
const server = createServer((_request, response) => { response.setHeader('Content-Type', 'text/html'); response.end(html); });
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const browser = await playwright.chromium.launch(process.env.SELFTEST_CHANNEL ? { channel: process.env.SELFTEST_CHANNEL } : {});
try {
  const page = await browser.newPage(); const errors = [];
  page.on('pageerror', error => errors.push(String(error)));
  const inbox = { who: 'test', counts: { decisions: 0, discussing: 0, provisional: 0, assigned: 0, claimed: 0, unread: 0, closed: 0 }, items: [], closed: [] };
  const lane = { agent_id: 'lane-a', description: 'useful lane', status: 'running', tokens: { total: 0 }, calls: 0, tasks: [] };
  const lanes = { counts: { running: 1, done: 0, stranded: 0, total: 1 }, lanes: [lane], warnings: ['task log line quarantined'] };
  let mode = 'error'; let held; let inboxDown = false;
  await page.route('**/api/**', async route => {
    const url = new URL(route.request().url());
    if (url.pathname === '/api/inbox') return route.fulfill(inboxDown ? { status: 500, json: { error: 'inbox unavailable' } } : { json: inbox });
    if (url.pathname === '/api/lanes') {
      if (url.searchParams.get('session') === 'slow') { held = route; return; }
      if (mode === 'network') return route.abort('failed');
      if (mode === 'error') return route.fulfill({ status: 404, json: { error: 'session not found' } });
      return route.fulfill({ json: lanes });
    }
    return route.fulfill({ status: 500, json: { error: 'fixture unavailable' } });
  });
  await page.goto(`http://127.0.0.1:${server.address().port}/?pane=lanes&lanes=missing&who=test`);
  await page.getByRole('alert').filter({ hasText: 'session not found' }).waitFor();
  assert.equal(await page.locator('#n-lanes').textContent(), '', 'error has no stale lane count');
  mode = 'ok'; await page.getByRole('button', { name: 'Retry loading', exact: true }).click();
  await page.locator('table.lanes').waitFor();
  assert.match(await page.locator('#main').textContent(), /task log line quarantined/, 'warning visible beside useful lanes');
  assert.match(await page.locator('#main').textContent(), /useful lane/);
  mode = 'network'; await page.locator('#refresh').click();
  await page.getByRole('alert').waitFor();
  assert.equal(await page.locator('table.lanes').count(), 0, 'network failure replaces stale table');
  assert.equal(await page.locator('#n-lanes').textContent(), '', 'network failure clears stale count');
  mode = 'ok'; await page.getByRole('button', { name: 'Retry loading', exact: true }).click();
  await page.locator('table.lanes').waitFor();
  await page.locator('#lanes-session').fill('slow'); await page.locator('#lanes-session').dispatchEvent('change');
  await page.waitForFunction(() => document.querySelector('#lanes-session')?.value === 'slow');
  for (let n = 0; !held && n < 100; n++) await new Promise(resolve => setTimeout(resolve, 10));
  assert(held, 'old root response held');
  await page.locator('#lanes-session').fill('new'); await page.locator('#lanes-session').dispatchEvent('change');
  await page.locator('table.lanes').waitFor();
  await held.fulfill({ status: 500, json: { error: 'obsolete failure' } });
  await page.waitForTimeout(30);
  assert.equal(await page.getByRole('alert').count(), 0, 'obsolete failure cannot replace new success');
  assert.equal(await page.locator('#lanes-session').inputValue(), 'new');
  inboxDown = true; await page.locator('#refresh').click();
  await page.waitForFunction(() => document.querySelector('#counter')?.textContent === 'inbox unavailable');
  assert.equal(await page.locator('table.lanes').count(), 1, 'lane facts remain useful when task inbox fails');
  assert.deepEqual(errors, [], 'page has no uncaught errors');
  console.log('inbox selftest: lane errors, retries, warnings and stale responses passing');
} finally { await browser.close(); await new Promise(resolve => server.close(resolve)); }
