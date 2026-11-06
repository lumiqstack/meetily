// Browser fixture suite for the Teams recap list script. Not part of the
// default `bun test` unit run (no ".test." in the name); run it explicitly with
// `pnpm run test:browser` — see tests/README.md for its prerequisites.
// Missing prerequisites fail loudly: this suite must never silently pass.
//
// The fixture is a synthetic Recap app ("Meeting Recaps"): an app bar button,
// a Home list of grouped rows, and on each row the meeting object Teams hands
// its React component, faked as a fiber chain. No real Teams page or data.
const { test, before, after } = require('node:test');
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
let chromium;
try {
  ({ chromium } = require('playwright'));
} catch (error) {
  throw new Error(
    'The Teams recap list suite needs the Playwright module, which is not a project dependency. ' +
    'Point NODE_PATH at an existing Playwright install (see tests/README.md). ' + error.message
  );
}

const SINCE = '2026-10-29T01:16:21Z';
const template = readFileSync(join(__dirname, '../../src-tauri/src/audio/teams_recap_list.js'), 'utf8');
// The real waits are seconds long; the fixture renders in milliseconds.
function render(probe) {
  let script = template.replace(/%SINCE%/g, SINCE).replace(/%PROBE%/g, String(probe));
  for (const [from, to] of [['SETTLE_MS = 6000', 'SETTLE_MS = 150'], ['EMPTY_MS = 20000', 'EMPTY_MS = 1500'], ['IDLE_MS = 8000', 'IDLE_MS = 1500']]) {
    assert.ok(script.includes(from), `the list script no longer declares ${from}; update this suite`);
    script = script.replace(from, to);
  }
  return script;
}

const SP = 'https://contoso-my.sharepoint.com';
const OCCURRENCE_PREFIX = '040000008200E00074C5B7101A82E008';
// Newest calendar slot first, as the Recap app lists them. kind: "file" rows
// carry recommendationData.url (the recording's path), "item" rows only a
// transcript, "dom" rows no React props at all. `rec` adds a recording entry.
const MEETINGS = [
  { t: 'Practice onboarding', slot: '2026-11-05T22:30:00.000Z', kind: 'item', occ: '07EA0B04', tr: '2026-11-04T23:30:17.892Z', group: 'This week' },
  { t: 'Kick-Off Sesión', slot: '2026-11-05T19:30:00.000Z', kind: 'file', rec: true, tr: '2026-11-05T19:34:25.530Z', spanish: true, group: 'This week' },
  { t: 'Clients Monthly Review', slot: '2026-11-05T18:30:00.000Z', kind: 'file', occ: '07EA0B05', tr: '2026-11-05T18:36:49.000Z', group: 'This week' },
  // The same meeting's second row: its recording, naming the same file.
  { t: 'Clients Monthly Review', slot: '2026-11-05T18:30:00.000Z', kind: 'file', occ: '07EA0B05', rec: true, tr: '2026-11-05T18:36:49.519Z', twin: 2, group: 'This week' },
  { t: 'No props row', slot: '2026-11-04T16:00:00.000Z', kind: 'dom', group: 'This week' },
  { t: 'Weekly sync', slot: '2026-10-30T15:00:00.000Z', kind: 'item', occ: '07EA0A1E', tr: '2026-10-30T15:02:00.000Z', group: 'Last week' },
  // A recurring row showing the series' next slot beside an old recap.
  { t: 'Old transcript, new slot', slot: '2026-10-31T15:00:00.000Z', kind: 'item', occ: '07EA0A1F', tr: '2026-10-20T15:02:00.000Z', group: 'Last week' },
  { t: 'Planning', slot: '2026-10-29T17:00:00.000Z', kind: 'file', tr: '2026-10-29T17:05:00.000Z', group: 'Last week' },
  { t: 'Before the window', slot: '2026-10-27T17:00:00.000Z', kind: 'file', tr: '2026-10-27T17:05:00.000Z', group: 'Earlier' },
  // Shown only in the "oldgroups" mode, as a collapsed group below the window.
  { t: 'Long before', slot: '2026-09-20T17:00:00.000Z', kind: 'file', tr: '2026-09-20T17:05:00.000Z', group: 'September' },
].map((m, i) => {
  const id = m.twin ?? i;
  const stamp = m.tr ? m.tr.slice(0, 10).replace(/-/g, '') + '_' + m.tr.slice(11, 19).replace(/:/g, '') : '';
  return {
    ...m, i, id,
    ical: OCCURRENCE_PREFIX + (m.occ || '00000000') + 'F6E8BC310000000000000000100000000000000000' + String(id).padStart(8, '0'),
    thread: '19:meeting_T' + id + '@thread.v2',
    item: '01ITEM' + id,
    call: 'call-' + i,
    file: SP + '/personal/org' + id + '/Documents/Recordings/' + m.t.replace(/ /g, '%20') + '-' + stamp + '-' +
      (m.spanish ? 'Grabación%20de%20la%20reunión.mp4' : 'Meeting%20Recording.mp4'),
  };
});
const meeting = title => MEETINGS.find(m => m.t === title);
const seriesOf = ical => ical.slice(0, 32) + '00000000' + ical.slice(40);

function fixture() {
  return `<!doctype html><meta charset=utf-8><title>Chat | Microsoft Teams</title>
<style>#scroller{height:320px;overflow-y:auto} li{height:60px;list-style:none}</style>
<div data-tid="app-layout-area--nav"><div data-tid="app-bar-wrapper">
  <button aria-label="Chat" aria-pressed="true">Chat</button>
  <button id=recapBtn data-tid="981bbfd0-5eb8-4393-8158-bfff69d2d6b5" aria-label="Meeting Recaps" aria-pressed="false">Meeting Recaps</button>
</div></div>
<div data-tid="app-layout-area--header" id=header></div>
<div data-tid="app-layout-area--main" id=main></div>
<script>
const M = ${JSON.stringify(MEETINGS)};
const mode = new URLSearchParams(location.search).get('mode') || 'list';
window.clicks = { app: 0, home: 0, group: 0, back: 0, row: 0 };
const fiber = (el, props) => {
  const own = { memoizedProps: props, return: null };
  el['__reactFiber$fixture'] = { memoizedProps: { className: 'b' }, return: { memoizedProps: { className: 'a' }, return: own } };
};
const meetingOf = m => {
  const transcript = { url: '${SP}/personal/org' + m.id + '/_api/v2.1/drives/b!drive' + m.id + '/items/' + m.item + '/versions/current/media/transcripts/x' + m.i + '/streamContent',
    callId: m.call, iCalUid: m.ical.toLowerCase(), startDateTime: m.tr, driveId: 'b!drive' + m.id };
  const value = { subject: m.t, threadId: m.thread, iCalUid: m.ical, start: m.slot, end: m.slot, organizerName: 'PERSON Name' + m.id,
    organizerId: 'org-' + m.id, transcripts: [transcript], recordings: [], authToken: 'SECRETAUTH' };
  if (m.rec) value.recordings = [{ driveId: 'b!drive' + m.id, url: '${SP}/:v:/g/personal/org' + m.id + '/share' + m.id }];
  if (m.kind === 'file') {
    value.recommendationData = { driveId: 'b!drive' + m.id, driveItemId: m.item, url: m.file,
      meetingSummary: 'CONFIDENTIAL summary prose about the budget', actionItems: [{ displayCleanContent: 'CONFIDENTIAL action' }] };
    value.meetingFiles = [{ organizerTenantId: 'tenant-1' }];
  }
  return value;
};
const rowHtml = m => '<li role=listitem data-tid="podcast-meeting-item" tabindex=0 aria-label="' + m.t + ', PERSON Name' + m.id + '" data-i=' + m.i + '>' +
  '<span class="fui-Checkbox"><input type=checkbox aria-label="' + m.t + '"></span><time datetime="' + m.slot + '">5 THU</time>' +
  '<button class=row-button aria-label="' + m.t + '">' + m.t + '</button><button class=row-button aria-label="Go to chat"></button>' +
  '<button class=row-button data-tid="podcast-view-recap-button">Recap</button></li>';
const listHtml = name => '<ul role=list class="fui-List">' + M.filter(m => m.group === name).map(rowHtml).join('') + '</ul>';
const groupHtml = (name, open) => '<div class="fui-AccordionItem"><div class="fui-AccordionHeader"><button class="fui-AccordionHeader__button" aria-expanded="' + open + '">' + name + '</button></div>' +
  '<div class="fui-AccordionPanel" data-group="' + name + '">' + (open ? listHtml(name) : '') + '</div></div>';
const wire = () => document.querySelectorAll('[data-tid="podcast-meeting-item"]').forEach(li => {
  const m = M[+li.dataset.i];
  if (m.kind !== 'dom' && !li['__reactFiber$fixture']) fiber(li, { meeting: meetingOf(m) });
  li.querySelectorAll('.row-button').forEach(b => b.onclick = () => { window.clicks.row += 1; });
});
let audio = mode === 'audio';
const home = () => {
  header.innerHTML = '<span>Meeting Recaps</span><div role=tablist><button id=homeTab role=tab aria-selected=' + !audio + '>Home</button>' +
    '<button role=tab aria-selected=' + audio + '>Audio recaps</button></div><button data-tid="recaps-app-feedback-button" aria-label="Feedback"></button>';
  homeTab.onclick = () => { window.clicks.home += 1; audio = false; home(); };
  main.innerHTML = '<div class="fui-SkeletonItem"></div>';
  clearTimeout(window.render);
  window.render = setTimeout(() => {
    if (audio) { main.innerHTML = '<div>Your audio recaps</div>'; return; }
    if (mode === 'empty') { main.innerHTML = '<h1>Good afternoon</h1><div>No recaps yet</div>'; return; }
    main.innerHTML = '<div id=scroller><div class="fui-Accordion">' + groupHtml('This week', true) + groupHtml('Last week', false) + '</div></div>';
    wire();
    // One handler for every group header, including groups added later.
    main.onclick = event => {
      const btn = event.target.closest('.fui-AccordionHeader__button');
      if (!btn) return;
      window.clicks.group += 1;
      // A re-render swallows the first click: the group stays collapsed.
      if (mode === 'recollapse' && window.clicks.group === 1) return;
      btn.setAttribute('aria-expanded', 'true');
      btn.closest('.fui-AccordionItem').querySelector('.fui-AccordionPanel').innerHTML = listHtml(btn.textContent);
      wire();
    };
    // Older rows arrive in a later page once the list is scrolled to its end;
    // in the "stalled" mode that page never comes.
    let earlier = false;
    clearInterval(window.pager);
    window.pager = setInterval(() => {
      const sc = document.getElementById('scroller');
      if (sc && !earlier && mode !== 'stalled' && sc.scrollTop > 0 && sc.scrollTop + sc.clientHeight >= sc.scrollHeight - 60) {
        earlier = true;
        setTimeout(() => {
          main.querySelector('.fui-Accordion').insertAdjacentHTML('beforeend', groupHtml('Earlier', true) + (mode === 'oldgroups' ? groupHtml('September', false) : ''));
          wire();
        }, 100);
      }
    }, 30);
  }, 200);
};
recapBtn.onclick = () => {
  window.clicks.app += 1;
  recapBtn.setAttribute('aria-pressed', 'true');
  if (mode === 'open') {
    // A recap left open from an earlier visit: no list until "back".
    header.innerHTML = '<button id=back aria-label="Go to Meeting Recaps"></button><span>Some recap</span>';
    main.innerHTML = '<div data-tid="intelligent-recap-header"></div>';
    back.onclick = () => { window.clicks.back += 1; home(); };
    return;
  }
  home();
};
</script>`;
}

let browser;
before(async () => {
  try {
    browser = await chromium.launch({ channel: 'chrome', headless: true });
  } catch (error) {
    throw new Error(
      'The Teams recap list suite needs Google Chrome installed (Playwright channel "chrome"). ' + error.message
    );
  }
});
after(async () => { await browser?.close(); });

// Poll the script the way teams_recap_list.rs does, until it says "done".
async function list(mode = 'list', probe = false) {
  const page = await browser.newPage();
  try {
    await page.route('https://teams.cloud.microsoft/**', route => route.fulfill({ contentType: 'text/html', body: fixture() }));
    await page.goto('https://teams.cloud.microsoft/v2/?mode=' + mode);
    const script = render(probe);
    const statuses = [];
    let result;
    for (let i = 0; i < 300; i++) {
      result = await page.evaluate(script);
      if (statuses[statuses.length - 1] !== result.status) statuses.push(result.status);
      if (result.status === 'done') break;
      await page.waitForTimeout(40);
    }
    assert.equal(result.status, 'done', `the script never finished: ${statuses.join(' > ')}`);
    return { result, statuses, clicks: await page.evaluate(() => window.clicks) };
  } finally {
    await page.close();
  }
}
const query = item => new URL(item.recapUrl).searchParams;

test('lists the scan window from the rows\' meeting data and stops past it', async () => {
  const { result, clicks } = await list();
  assert.deepEqual(result.items.map(item => item.title), [
    'Practice onboarding', 'Kick-Off Sesión', 'Clients Monthly Review', 'Clients Monthly Review', 'No props row', 'Weekly sync', 'Planning',
  ]);
  // Opened the app once, the collapsed group once, and never a row's buttons.
  assert.deepEqual(clicks, { app: 1, home: 0, group: 1, back: 0, row: 0 });
  assert.equal(result.items.find(item => item.title === 'No props row').recapUrl, null);
});

test('builds a recording\'s link with the Share button\'s keys, keyed on its file', async () => {
  const { result } = await list();
  const m = meeting('Kick-Off Sesión');
  const item = result.items.find(entry => entry.title === m.t);
  assert.equal(item.linkKind, 'file');
  assert.ok(item.recapUrl.startsWith('https://teams.cloud.microsoft/l/meetingrecap?'), item.recapUrl);
  const q = query(item);
  assert.deepEqual([...q.keys()], ['driveId', 'driveItemId', 'sitePath', 'fileUrl', 'iCalUid', 'threadId', 'organizerId', 'tenantId', 'callId',
    'threadType', 'meetingType', 'subType', 'recapType']);
  assert.equal(q.get('sitePath'), m.file);
  assert.equal(q.get('fileUrl'), new URL(m.file).href + '?web=1');
  assert.match(q.get('fileUrl'), /Grabaci%C3%B3n%20de%20la%20reuni%C3%B3n\.mp4\?web=1$/);
  assert.equal(q.get('driveItemId'), m.item);
  assert.equal(q.get('callId'), m.call);
  assert.equal(q.get('meetingType'), 'Scheduled');
  assert.equal(q.get('recapType'), 'RecordingAndTranscript');
  assert.equal(item.startedAt, m.tr);
});

test('marks an occurrence with its series and gives a transcript-only row its drive item', async () => {
  const { result } = await list();
  const review = query(result.items.find(entry => entry.title === 'Clients Monthly Review'));
  const m = meeting('Clients Monthly Review');
  assert.equal(review.get('iCalUid'), m.ical);
  assert.equal(review.get('masterICalUid'), seriesOf(m.ical));
  assert.equal(review.get('meetingType'), 'Recurring');
  assert.equal(review.get('recapType'), 'ODSPTranscript');

  const practice = result.items.find(entry => entry.title === 'Practice onboarding');
  const p = meeting('Practice onboarding');
  assert.equal(practice.linkKind, 'item');
  const q = query(practice);
  assert.equal(q.get('fileUrl'), null);
  assert.equal(q.get('driveItemId'), p.item);
  assert.equal(q.get('masterICalUid'), seriesOf(p.ical));
  // Dated by its transcript, not by the next slot the row shows.
  assert.equal(practice.startedAt, p.tr);
});

test('a meeting\'s transcript row and recording row name the same file', async () => {
  const { result } = await list();
  const [first, second] = result.items.filter(entry => entry.title === 'Clients Monthly Review').map(query);
  assert.equal(first.get('fileUrl'), second.get('fileUrl'));
  assert.notEqual(first.get('callId'), second.get('callId'));
  assert.deepEqual([first.get('recapType'), second.get('recapType')], ['ODSPTranscript', 'RecordingAndTranscript']);
});

test('leaves out a row dated before the window even when its slot is inside it', async () => {
  const { result } = await list();
  assert.ok(!result.items.some(entry => entry.title === 'Old transcript, new slot'));
  assert.ok(!result.items.some(entry => entry.title === 'Before the window'));
});

test('switches from Audio recaps to Home before reading', async () => {
  const { result, clicks } = await list('audio');
  assert.equal(clicks.home, 1);
  assert.equal(result.items.length, 7);
});

test('opens a group again when a re-render collapses it', async () => {
  const { result, clicks } = await list('recollapse');
  assert.equal(clicks.group, 2);
  assert.equal(result.items.length, 7);
});

test('leaves collapsed groups below the window closed', async () => {
  const { result, clicks } = await list('oldgroups');
  assert.equal(clicks.group, 1);
  assert.equal(result.items.length, 7);
});

test('closes a recap left open with its back button', async () => {
  const { result, clicks } = await list('open');
  assert.equal(clicks.back, 1);
  assert.equal(result.items.length, 7);
});

test('says why it stopped and the oldest slot it reached, probe or not', async () => {
  const { result } = await list();
  assert.equal(result.stop, 'since');
  assert.equal(result.oldest, '2026-10-27T17:00:00.000Z');
  // Older rows that never load: the list ends inside the window and says so.
  const { result: stalled } = await list('stalled');
  assert.equal(stalled.stop, 'idle');
  assert.equal(stalled.oldest, '2026-10-29T17:00:00.000Z');
  const { result: empty } = await list('empty');
  assert.deepEqual([empty.stop, empty.oldest], ['empty', null]);
});

test('an empty Recap app finishes with no items', async () => {
  const { result } = await list('empty');
  assert.deepEqual(result.items, []);
});

test('the probe holds counts and timings, never titles, names, links or meeting text', async () => {
  const { result } = await list('list', true);
  const { probe } = result;
  assert.equal(probe.rows, 9);
  assert.deepEqual(probe.kinds, { item: 3, file: 5, none: 1 });
  assert.equal(probe.rowsWithoutProps, 1);
  assert.equal(probe.stop, 'since');
  assert.equal(probe.since, '2026-10-29T01:16:21.000Z');
  assert.ok(probe.firstRowsMs > 0 && probe.listMs >= probe.firstRowsMs, JSON.stringify(probe));
  const text = JSON.stringify(probe);
  for (const leak of ['Kick-Off', 'Planning', 'PERSON', 'SECRET', 'CONFIDENTIAL', 'sharepoint', 'meetingrecap']) {
    assert.ok(!text.includes(leak), `probe leaked ${leak}: ${text}`);
  }
  assert.ok(!JSON.stringify(result.items).match(/PERSON|SECRET|CONFIDENTIAL/), 'items carry only titles, times and links');
});
