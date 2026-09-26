import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';

const rust = readFileSync(new URL('../../src-tauri/src/audio/teams_recap.rs', import.meta.url), 'utf8');
const script = rust.match(/const RECAP_SCRIPT: &str = r#"([\s\S]*?)"#;/)[1];
function tab(name, selected = false) {
  return { innerText: name, clicked: false,
    getAttribute: () => String(selected),
    click() { this.clicked = true; } };
}
function probe(tabs, host = 'teams.cloud.microsoft') {
  return runInNewContext(script, {
    location: { hostname: host },
    document: { body: { innerText: '' },
      querySelectorAll: selector => selector === '[role="tab"]' ? tabs : [] },
  });
}

test('recognizes the reported restored chat without opening its unrelated recap', () => {
  const tabs = ['Chat', 'Shared', 'Notes', 'Recap'].map(name => tab(name));
  assert.equal(probe(tabs).status, 'signed_in_chat');
  assert.ok(tabs.every(t => !t.clicked));
});
test('does not interrupt sign-in', () => {
  assert.equal(probe([], 'login.microsoftonline.com').status, 'waiting');
});
test('allows a selected recap time to load its AI summary', () => {
  assert.equal(probe([tab('Chat'), tab('Shared'), tab('Recap', true)]).status, 'waiting');
});
test('opens AI summary when the recap has loaded', () => {
  const summary = tab('AI summary');
  assert.equal(probe([summary]).status, 'waiting');
  assert.equal(summary.clicked, true);
});
test('handles the corporate proxy Teams host', () => {
  assert.equal(probe([tab('Chat'), tab('Files')], 'teams.microsoft.com.mcas.ms').status, 'signed_in_chat');
});
