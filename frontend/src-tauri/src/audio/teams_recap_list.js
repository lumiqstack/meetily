(() => {
      // Rendered by teams_recap_list.rs: %SINCE% is an RFC 3339 instant and
      // %PROBE% a boolean. The script reads the meeting list of the Teams
      // Recap app ("Meeting Recaps" on the app bar) and never exposes Tauri
      // IPC or sends page text anywhere.
      const SINCE = new Date('%SINCE%');
      const PROBE = %PROBE%;
      const MAX_ROWS = 600;
      // Let the shell render, and a click take effect, before reading rows.
      const SETTLE_MS = 6000;
      // An open Recap app showing no rows and no loading placeholders for
      // this long has nothing to list.
      const EMPTY_MS = 20000;
      // The list is complete once it has neither grown nor scrolled for this
      // long: older rows load in pages as the list scrolls.
      const IDLE_MS = 8000;
      const host = location.hostname.toLowerCase();
      if (!['teams.microsoft.com', 'teams.cloud.microsoft',
            'teams.microsoft.com.mcas.ms', 'teams.cloud.microsoft.mcas.ms'].includes(host)) {
        return { status: 'waiting', href: location.href };
      }
      const ROW = '[data-tid="podcast-meeting-item"]';
      const text = el => (el?.innerText || el?.textContent || '').replace(/\s+/g, ' ').trim();
      const label = el => (el.getAttribute('aria-label') || text(el)).trim();
      const visible = el => el.getClientRects().length > 0;
      const session = {
        get: key => { try { return sessionStorage.getItem(key); } catch (e) { return null; } },
        set: (key, value) => {
          try { sessionStorage.setItem(key, value); return sessionStorage.getItem(key) === value; } catch (e) { return false; }
        }
      };
      const state = window.__meetilyRecapList ||= {
        clicks: {}, seen: {}, order: [], details: [], shapes: {}, errors: [], oldest: null,
        firstSeen: Date.now(), lastClick: 0, emptySince: 0, idleSince: 0, lastCount: -1, lastProbeAt: 0
      };
      const noteError = (where, e) => { if (state.errors.length < 20) state.errors.push(where + ': ' + e.message); };

      // ---- app-choice page, same handling as the recap import ----
      const webChoice = [...document.querySelectorAll('a,button,[role="button"]')].find(el =>
        /^(continue (on|in) (this|the) browser|use the web app( instead)?|open (in|on) (the )?web( app)?|continue in browser)$/i
          .test(text(el) || el.getAttribute('aria-label') || ''));
      if (webChoice) {
        webChoice.click();
        return { status: 'opening_web', href: location.href };
      }

      // ---- what a row holds: the meeting object its React component gets
      // as a prop, which carries every field of a recap sharing link ----
      const fiberOf = el => {
        const key = Object.keys(el).find(name => name.startsWith('__reactFiber$'));
        return key ? el[key] : null;
      };
      // The props of the nearest component at or above `el` that pass `wanted`.
      const propsAbove = (el, wanted) => {
        for (let fiber = fiberOf(el), level = 0; fiber && level < 12; level++, fiber = fiber.return) {
          const props = fiber.memoizedProps;
          if (props && typeof props === 'object' && wanted(props)) return props;
        }
        return null;
      };
      const meetingOf = row => propsAbove(row, props => {
        const meeting = props.meeting;
        return !!meeting && typeof meeting === 'object' && !!(meeting.threadId || meeting.iCalUid);
      })?.meeting || null;
      const str = value => typeof value === 'string' ? value.trim() : '';
      const listOf = value => Array.isArray(value) ? value.filter(entry => entry && typeof entry === 'object') : [];
      const instant = value => {
        const when = value ? new Date(value) : null;
        return when && !isNaN(when) ? when : null;
      };
      // Outlook's GlobalObjectId: bytes 16-19 hold an occurrence's original
      // date, and are zero for a series and for a single meeting.
      const GLOBAL_ID = /^040000008200E00074C5B7101A82E008[0-9A-F]{8,}$/i;
      const isOccurrence = ical => GLOBAL_ID.test(ical) && !/^0{8}$/.test(ical.slice(32, 40));
      const seriesOf = ical => ical.slice(0, 32) + '00000000' + ical.slice(40);
      const itemInApiUrl = url => (str(url).match(/\/items\/([^/?#]+)/) || [])[1] || '';
      // The recording file as a sharing link writes it: non-ASCII letters
      // percent-encoded and Office's "?web=1".
      const fileLink = url => {
        try {
          const file = new URL(url);
          file.search = 'web=1';
          file.hash = '';
          return file.href;
        } catch (e) {
          return '';
        }
      };
      const tenantOf = meeting => str(meeting.tenantId) || str(meeting.organizerTenantId) ||
        listOf(meeting.meetingFiles).map(file => str(file.organizerTenantId)).find(Boolean) || '';
      // A recap sharing link built from the row, with the same keys and order
      // the recap's Share button uses. A recording's row names its file, so
      // the link keys on that file like a pasted link does. A
      // transcript-only row has only the transcript's drive item.
      const recapOf = meeting => {
        const transcripts = listOf(meeting.transcripts);
        const recordings = listOf(meeting.recordings);
        const files = transcripts.concat(recordings);
        const recommended = meeting.recommendationData && typeof meeting.recommendationData === 'object'
          ? meeting.recommendationData : {};
        const file = str(recommended.url);
        const ical = str(meeting.iCalUid);
        const recurring = isOccurrence(ical);
        const first = read => files.map(read).find(Boolean) || '';
        const params = [
          ['driveId', str(recommended.driveId) || first(entry => str(entry.driveId))],
          ['driveItemId', str(recommended.driveItemId) || first(entry => str(entry.driveItemId) || itemInApiUrl(entry.url))],
          ['sitePath', file],
          ['fileUrl', file ? fileLink(file) : ''],
          ['iCalUid', ical],
          ['masterICalUid', recurring ? seriesOf(ical) : ''],
          ['threadId', str(meeting.threadId) || str(meeting.skypeTeamsProperties?.conversationId)],
          ['organizerId', str(meeting.organizerId)],
          ['tenantId', tenantOf(meeting)],
          ['callId', first(entry => str(entry.callId))],
          ['threadType', 'meeting'],
          ['meetingType', recurring ? 'Recurring' : 'Scheduled'],
          ['subType', 'RecapSharingLink_RecapCore'],
          ['recapType', transcripts.length ? (recordings.length ? 'RecordingAndTranscript' : 'ODSPTranscript') : '']
        ].filter(([, value]) => value);
        const has = name => params.some(([key]) => key === name);
        if (!has('threadId') && !has('iCalUid')) return null;
        // When it was recorded, not the calendar slot: a recurring row can
        // show the series' next slot beside an earlier occurrence's recap.
        const starts = files.map(entry => instant(entry.startDateTime)).filter(Boolean);
        return {
          recapUrl: 'https://teams.microsoft.com/l/meetingrecap?' +
            params.map(([key, value]) => key + '=' + encodeURIComponent(value)).join('&'),
          linkKind: file ? 'file' : has('driveItemId') ? 'item' : 'thread',
          recordedAt: starts.length ? new Date(Math.min(...starts)) : null,
          detail: { recurring, transcripts: transcripts.length, recordings: recordings.length, tenant: has('tenantId') }
        };
      };
      const readRow = row => {
        const meeting = meetingOf(row);
        const title = (str(meeting?.subject) ||
          row.querySelector('input[type="checkbox"][aria-label]')?.getAttribute('aria-label') || '').trim().slice(0, 200);
        const scheduled = instant(meeting?.start) || instant(row.querySelector('time[datetime]')?.getAttribute('datetime'));
        const recap = meeting ? recapOf(meeting) : null;
        const startedAt = recap?.recordedAt || scheduled;
        return {
          meeting,
          scheduled,
          item: {
            recapUrl: recap ? recap.recapUrl : null,
            linkKind: recap ? recap.linkKind : null,
            title,
            startedAt: startedAt ? startedAt.toISOString() : null
          },
          detail: { scheduled: scheduled ? scheduled.toISOString() : null, props: !!meeting, ...(recap?.detail || {}) }
        };
      };

      // ---- probe only: a record for tuning, written to a local file ----
      const VIEWS_KEY = 'meetilyRecapViews';
      const TESTS_KEY = 'meetilyRecapTests';
      const SECRET_KEY = /token|auth|secret|password|cookie|session|credential|signature|^sig$|^key$/i;
      // Tokens never leave the page, even into the local probe file.
      const redact = value => JSON.parse(JSON.stringify(value)
        .replace(/(tempauth|access_token|token|sig|code)(=|%3D)[^&%"'\s\\]+/gi, '$1$2REDACTED')
        .replace(/eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+/g, '<jwt>'));
      // Key paths of a meeting object. Values only for id, type, time and
      // file fields; never prose, names or summaries.
      const SHOW_VALUE = /^(id|.*Ids?|.*ICalUid|iCalUid|type|subType|.*Type|start|end|.*DateTime|.*Time|url|webUrl|location|fileUrl|sitePath|fileName|name|is[A-Z].*|has[A-Z].*|state|status)$/;
      const shapeOf = root => {
        const out = [];
        const seen = new WeakSet();
        const walk = (value, path, depth) => {
          if (out.length >= 160) return;
          const key = path.slice(path.lastIndexOf('.') + 1).replace(/\[\d+\]$/, '');
          if (Array.isArray(value)) {
            out.push(path + ' [' + value.length + ']');
            if (value.length && depth < 4) walk(value[0], path + '[0]', depth + 1);
          } else if (value && typeof value === 'object') {
            if (seen.has(value) || value.nodeType || value === window) return;
            seen.add(value);
            if (depth >= 4) {
              out.push(path + ' {' + Object.keys(value).length + ' keys}');
              return;
            }
            for (const name of Object.keys(value).slice(0, 60)) walk(value[name], path + '.' + name, depth + 1);
          } else if (typeof value === 'string') {
            const plain = !/\s/.test(value) || /^https?:/i.test(value) || /\.(mp4|vtt|docx?|pptx?|xlsx?|loop)$/i.test(value);
            const shown = SHOW_VALUE.test(key) && !SECRET_KEY.test(key) && plain && value.length <= 600;
            out.push(path + (shown ? ' = ' + value : ' <string ' + value.length + '>'));
          } else if (typeof value === 'number' || typeof value === 'boolean') {
            out.push(path + ' = ' + value);
          } else if (value !== undefined) {
            out.push(path + ' <' + (value === null ? 'null' : typeof value) + '>');
          }
        };
        walk(root, '$', 0);
        return out;
      };
      const parsed = raw => { try { return JSON.parse(raw || 'null'); } catch (e) { return null; } };
      const views = PROBE ? parsed(session.get(VIEWS_KEY)) || [] : [];
      // Whether the page paints while the window is hidden: older rows load
      // on scroll, which may need painting.
      if (PROBE && !state.counting) {
        state.counting = true;
        state.frames = 0;
        const tick = () => { state.frames += 1; requestAnimationFrame(tick); };
        requestAnimationFrame(tick);
      }
      // One line per change of view, kept across Teams' own reloads.
      const noteView = status => {
        if (!PROBE) return;
        const view = {
          status,
          path: location.pathname,
          title: document.title.replace(/^\(\d+\)\s*/, '').slice(0, 120),
          visibility: document.visibilityState,
          app: !!document.querySelector('[data-tid="recaps-app-feedback-button"]'),
          rows: document.querySelectorAll(ROW).length,
          skeletons: document.querySelectorAll('[data-tid="app-layout-area--main"] .fui-SkeletonItem').length,
          recap: !!document.querySelector('[data-tid="recap-share-button"]')
        };
        const last = views[views.length - 1];
        if (last && Object.keys(view).every(key => last[key] === view[key])) return;
        views.push({ at: new Date().toISOString(), loadMs: Math.round(performance.now()), frames: state.frames, ...view });
        if (views.length > 40) views.splice(4, 1);
        session.set(VIEWS_KEY, JSON.stringify(views));
      };
      const listReport = () => ({
        clicks: state.clicks,
        errors: state.errors,
        rows: state.order.length,
        rowsWithoutProps: state.details.filter(detail => !detail.props).length,
        firstRowsMs: state.firstRowsMs,
        listMs: state.listMs,
        frames: state.frames,
        scrolls: state.scrolls,
        pane: state.pane,
        details: state.details.slice(0, 80),
        shapes: state.shapes
      });
      const report = saved => redact({
        try: 3,
        href: location.href,
        since: SINCE.toISOString(),
        list: saved ? saved.list : listReport(),
        tests: saved ? saved.tests : undefined,
        views
      });
      // The app keeps the last report it got, so a report every few seconds
      // is enough.
      const probe = (saved, force) => {
        if (!PROBE || (!force && Date.now() - state.lastProbeAt < 5000)) return undefined;
        state.lastProbeAt = Date.now();
        return report(saved);
      };
      const reply = (status, extra, saved) => {
        try { noteView(status); } catch (e) { noteError('view', e); }
        return { status, href: location.href, ...(extra || {}), probe: probe(saved, status === 'done') };
      };

      // ---- probe only: open a built link the way the recap import will, and
      // compare what the recap's Share button holds with what the row gave ----
      const fileKey = url => {
        try {
          const file = new URL(url);
          return file.host.toLowerCase() + decodeURIComponent(file.pathname).toLowerCase();
        } catch (e) {
          return '';
        }
      };
      const SHARE_FIELDS = ['fileUrl', 'sitePath', 'driveId', 'driveItemId', 'iCalUid', 'masterICalUid', 'threadId',
        'organizerId', 'tenantId', 'organizerTenantId', 'callId', 'threadType', 'meetingType', 'subType', 'recapType',
        'meetingStartTime', 'meetingEndTime'];
      const shareProps = () => {
        const button = document.querySelector('[data-tid="recap-share-button"]');
        const props = button && propsAbove(button, value => 'fileUrl' in value || 'sitePath' in value || 'driveItemId' in value);
        if (!props) return null;
        const picked = { keys: Object.keys(props).filter(key => key !== 'children').slice(0, 80) };
        for (const name of SHARE_FIELDS) {
          const value = props[name];
          if (typeof value === 'string' || typeof value === 'number') picked[name] = String(value).slice(0, 600);
        }
        return picked;
      };
      const pickTests = items => ['item', 'file']
        .map(kind => items.find(item => item.linkKind === kind))
        .filter(Boolean)
        .map(item => {
          const query = new URL(item.recapUrl).searchParams;
          const expect = { fileKey: fileKey(query.get('fileUrl') || '') };
          for (const name of ['callId', 'driveItemId', 'iCalUid', 'threadId']) expect[name] = query.get(name) || '';
          return { title: item.title, kind: item.linkKind, query: item.recapUrl.split('?')[1], expect };
        });
      const saveTests = saved => session.set(TESTS_KEY, JSON.stringify(saved));
      const runTests = saved => {
        const test = saved.tests[saved.index];
        if (!test) {
          if (saved.phase !== 'finished') {
            saved.phase = 'finished';
            saveTests(saved);
          }
          return reply('done', { items: saved.items }, saved);
        }
        if (!test.navigatedAt) {
          test.navigatedAt = Date.now();
          saveTests(saved);
          location.href = location.origin + '/l/meetingrecap?' + test.query;
          return reply('navigating', {}, saved);
        }
        const elapsed = Date.now() - test.navigatedAt;
        const share = shareProps();
        if (share && !test.shareMs) {
          test.shareMs = elapsed;
          saveTests(saved);
        }
        // The button can render before its recap's ids arrive.
        const complete = share && (share.callId || share.driveItemId || elapsed - test.shareMs > 15000);
        if (complete || elapsed > 75000) {
          test.ms = elapsed;
          test.share = share;
          if (share) {
            test.matches = { fileUrl: !!test.expect.fileKey && fileKey(share.fileUrl || '') === test.expect.fileKey };
            for (const name of ['callId', 'driveItemId', 'iCalUid', 'threadId']) {
              test.matches[name] = (share[name] || '').toLowerCase() === test.expect[name].toLowerCase();
            }
          }
          test.landed = {
            title: document.title.replace(/^\(\d+\)\s*/, '').slice(0, 120),
            path: location.pathname,
            seen: ['intelligent-recap-header', 'recap-share-button', 'meeting-recap-main-panel', 'podcast-meeting-item',
              'recaps-app-feedback-button'].filter(tid => document.querySelector('[data-tid="' + tid + '"]'))
          };
          saved.index += 1;
          saveTests(saved);
        }
        return reply('navigating', {}, saved);
      };
      const pending = PROBE ? parsed(session.get(TESTS_KEY)) : null;
      if (pending) return runTests(pending);

      // ---- steer to the Recap app once. The shell may open on Chat or
      // Activity, and the app may be pinned or behind "more apps"; its id is
      // tried before its name. ----
      const RECAP_APP_ID = '981bbfd0-5eb8-4393-8158-bfff69d2d6b5';
      const RAIL = '[data-tid="app-bar-wrapper"],[data-tid="app-layout-area--nav"]';
      const inRail = el => !!el.closest(RAIL);
      const buttons = () => [...document.querySelectorAll('[role="tab"],[role="button"],[role="menuitem"],[role="option"],button,a')]
        .filter(visible);
      const named = (el, pattern) => pattern.test(label(el)) || pattern.test(text(el));
      const isSelected = el => el.getAttribute('aria-selected') === 'true' ||
        el.getAttribute('aria-current') === 'page' || el.getAttribute('aria-pressed') === 'true';
      const appName = /^(recaps?|meeting recaps?)(?:\s*[,(].*)?$/i;
      const moreApps = /^(view more apps|more added apps|more apps)(?:\s*[,(].*)?$/i;
      const clickNow = (key, el) => {
        state.clicks[key] = new Date().toISOString();
        state.lastClick = Date.now();
        el.click();
        return reply('navigating');
      };
      const settled = () => Date.now() - Math.max(state.firstSeen, state.lastClick) >= SETTLE_MS;
      if (!state.clicks.app) {
        const byId = document.querySelector('[data-tid="' + RECAP_APP_ID + '"]');
        const app = byId && visible(byId) ? byId
          : buttons().find(el => inRail(el) && named(el, appName)) ||
            (state.clicks.moreApps ? buttons().find(el => !inRail(el) && named(el, appName)) : null);
        if (app && isSelected(app)) {
          state.clicks.app = 'already open';
        } else if (app) {
          return clickNow('app', app);
        } else {
          if (!state.clicks.moreApps && Date.now() - state.firstSeen > 15000) {
            const more = buttons().find(el => inRail(el) && named(el, moreApps));
            if (more) return clickNow('moreApps', more);
          }
          return reply('navigating');
        }
      }
      if (!settled()) return reply('navigating');
      // The app opens on Home, its list; "Audio recaps" is the other tab.
      if (!state.clicks.home) {
        const home = buttons().find(el => el.getAttribute('role') === 'tab' && !inRail(el) && named(el, /^home$/i));
        if (home && !isSelected(home)) return clickNow('home', home);
        if (home) state.clicks.home = 'already open';
      }

      const finishList = items => {
        state.listMs ||= Math.round(performance.now());
        if (PROBE && !state.testsQueued) {
          state.testsQueued = true;
          const saved = { phase: 'running', index: 0, items, list: listReport(), tests: pickTests(items) };
          if (saved.tests.length && saveTests(saved)) return reply('page', { items, more: true }, saved);
        }
        return reply('done', { items });
      };
      const rows = [...document.querySelectorAll(ROW)].filter(visible);
      if (!rows.length) {
        // A recap left open from an earlier visit hides the list.
        const back = !state.clicks.back && buttons().find(el => !inRail(el) && named(el, /^(go|back) to (meeting )?recaps?$/i));
        if (back) return clickNow('back', back);
        const home = document.querySelector('[data-tid="recaps-app-feedback-button"]');
        const loading = document.querySelector('[data-tid="app-layout-area--main"] .fui-SkeletonItem');
        if (home && !loading) {
          state.emptySince ||= Date.now();
          if (Date.now() - state.emptySince >= EMPTY_MS) return finishList([]);
        } else {
          state.emptySince = 0;
        }
        if (/open (your|the) teams app|join (on|in) (the )?teams app|use the web app/i.test(text(document.body).slice(0, 1200))) {
          return reply('launcher');
        }
        return reply('waiting');
      }
      state.emptySince = 0;
      state.firstRowsMs ||= Math.round(performance.now());

      // ---- collect rows; the list runs newest first by calendar slot, so
      // the oldest slot seen decides when to stop ----
      for (const row of rows) {
        let read;
        try {
          read = readRow(row);
        } catch (e) {
          noteError('row', e);
          continue;
        }
        const { item, meeting, scheduled, detail } = read;
        const key = item.recapUrl || item.title + '|' + item.startedAt;
        if (!state.seen[key]) {
          state.seen[key] = true;
          state.order.push(item);
          if (PROBE) {
            state.details.push({ title: item.title, kind: item.linkKind, startedAt: item.startedAt, ...detail });
            if (meeting && item.linkKind && !state.shapes[item.linkKind]) {
              try { state.shapes[item.linkKind] = shapeOf(meeting); } catch (e) { noteError('shape', e); }
            }
          }
        }
        const slot = scheduled || instant(item.startedAt);
        if (slot && (!state.oldest || slot < state.oldest)) state.oldest = slot;
      }
      const items = state.order.filter(item => !item.startedAt || new Date(item.startedAt) >= SINCE);
      if ((state.oldest && state.oldest < SINCE) || state.order.length >= MAX_ROWS) return finishList(items);

      // Open groups the list starts collapsed ("Last week", ...).
      const accordion = rows[0].closest('.fui-Accordion');
      const group = accordion && [...accordion.querySelectorAll('.fui-AccordionHeader__button[aria-expanded="false"]')]
        .filter(visible).find(el => !state.clicks['group ' + text(el).slice(0, 40)]);
      if (group) {
        state.clicks['group ' + text(group).slice(0, 40)] = new Date().toISOString();
        group.click();
        state.idleSince = 0;
        return reply('page', { items, more: true });
      }

      // Scroll the list's own pane (not the page) to load older rows.
      const lastRow = rows[rows.length - 1];
      let pane = lastRow.parentElement;
      while (pane && pane !== document.body) {
        if (pane.scrollHeight > pane.clientHeight + 20 && /auto|scroll/.test(getComputedStyle(pane).overflowY)) break;
        pane = pane.parentElement;
      }
      if (!pane || pane === document.body) pane = document.scrollingElement;
      const before = pane.scrollTop;
      pane.scrollTop += Math.max(300, pane.clientHeight * 0.8);
      const moved = pane.scrollTop > before;
      if (PROBE) {
        state.scrolls = (state.scrolls || 0) + (moved ? 1 : 0);
        state.pane = [pane.tagName.toLowerCase(), pane.getAttribute('data-tid') || '',
          (typeof pane.className === 'string' ? pane.className.split(/\s+/).filter(c => c.startsWith('fui-')).join('.') : ''),
          pane.scrollTop + '/' + pane.scrollHeight + '/' + pane.clientHeight].join(' ');
      }
      const grew = state.order.length !== state.lastCount;
      state.lastCount = state.order.length;
      if (moved || grew) {
        state.idleSince = 0;
      } else {
        state.idleSince ||= Date.now();
        const loadMore = buttons().find(el => !el.closest(ROW) && !inRail(el) &&
          /^(load|show|see) more( meetings| recaps)?$/i.test(label(el)));
        if (loadMore && !state.clicks['more ' + state.order.length]) {
          state.clicks['more ' + state.order.length] = new Date().toISOString();
          loadMore.click();
          state.idleSince = 0;
        }
      }
      if (state.idleSince && Date.now() - state.idleSince >= IDLE_MS) return finishList(items);
      return reply('page', { items, more: true });
    })()
