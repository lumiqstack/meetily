// Run with node --test; playwright can be supplied through NODE_PATH.
const { test, before, after } = require('node:test');
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const { chromium } = require('playwright');
const script = readFileSync(join(__dirname, '../../src-tauri/src/audio/teams_recap.js'), 'utf8');
let browser;
before(async () => { browser = await chromium.launch({ channel: 'chrome', headless: true }); });
after(async () => { await browser?.close(); });

async function pageFor(content) {
  const page = await browser.newPage();
  // Synthetic local fixture. No authenticated Teams page or personal data.
  await page.route('https://teams.cloud.microsoft/**', route => route.fulfill({
    contentType: 'text/html', body: `<button role="tab" aria-selected="true">AI summary</button><div role="tabpanel">${content}</div>`,
  }));
  await page.goto('https://teams.cloud.microsoft/fixture');
  return page;
}
async function ready(page) {
  let result;
  for (let i = 0; i < 6; i++) {
    result = await page.evaluate(script);
    if (result.status === 'ready') return result;
  }
  assert.fail(`Expected complete recap, got ${result.status}`);
}
const notes = '<h3>Meeting notes</h3><div role="row"><strong>Planning:</strong> Keep the release small.</div><div role="row" aria-level="2">. Scope: Confirm the estimate.</div>';

test('preserves bold topics, child indentation and follow-up list items', async () => {
  const page = await pageFor(notes + '<h3>Follow-up tasks</h3><ul><li><strong>Send estimate:</strong> Alex to send it tomorrow.</li></ul>');
  try {
    const result = await ready(page);
    assert.deepEqual(result.notes, ['- **Planning:** Keep the release small.', '  - **Scope:** Confirm the estimate.']);
    assert.deepEqual(result.tasks, ['- [ ] **Send estimate:** Alex to send it tomorrow.']);
  } finally { await page.close(); }
});
test('does not return partial notes before delayed tasks mount', async () => {
  const page = await pageFor(notes);
  try {
    assert.equal((await page.evaluate(script)).status, 'waiting_tasks');
    await page.locator('[role="tabpanel"]').evaluate(el => el.insertAdjacentHTML('beforeend', '<h2>Follow-up tasks</h2><div role="row">Review scope: Alex</div>'));
    assert.deepEqual((await ready(page)).tasks, ['- [ ] **Review scope:** Alex']);
  } finally { await page.close(); }
});
test('accepts an explicit empty task section without fabricating tasks', async () => {
  const page = await pageFor(notes + '<h3>Follow-up tasks</h3><p>No follow-up tasks identified.</p>');
  try { assert.deepEqual((await ready(page)).tasks, []); }
  finally { await page.close(); }
});
test('keeps paragraphs together without duplicate nested row content', async () => {
  const page = await pageFor('<h2>Meeting notes</h2><div role="row"><p><b>Topic:</b> First paragraph.</p><p>Second paragraph.</p></div><h2>Follow-up tasks</h2><p>Send notes: Alex</p>');
  try {
    const result = await ready(page);
    assert.deepEqual(result.notes, ['- **Topic:** First paragraph.\n  Second paragraph.']);
    assert.equal(result.tasks.length, 1);
  } finally { await page.close(); }
});
test('reads div task cards and stops before the next section', async () => {
  const page = await pageFor(notes + '<h3>Follow-up tasks</h3><div>Send update: Alex</div><h3>Transcript</h3><p>Do not import this conversation.</p>');
  try { assert.deepEqual((await ready(page)).tasks, ['- [ ] **Send update:** Alex']); }
  finally { await page.close(); }
});
test('does not include unrelated content outside the recap panel', async () => {
  const page = await pageFor(notes + '<h3>Follow-up tasks</h3><li>Follow up: Alex</li>');
  try {
    await page.evaluate(() => document.body.insertAdjacentHTML('beforeend', '<p>Unrelated chat message</p>'));
    assert.deepEqual((await ready(page)).tasks, ['- [ ] **Follow up:** Alex']);
  } finally { await page.close(); }
});
