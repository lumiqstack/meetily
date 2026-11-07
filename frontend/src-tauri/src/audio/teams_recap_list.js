(() => {
      // Rendered by teams_recap_list.rs: %SINCE% is an RFC 3339 instant and
      // %PROBE% a boolean. The script reads the meeting list of the Teams
      // Recap app ("Meeting Recaps" on the app bar) and never exposes Tauri
      // IPC or sends page text anywhere.
      const SINCE = new Date('%SINCE%');
      const PROBE = %PROBE%;
      // The same cap as MAX_ROWS in teams_recap_list.rs.
      const MAX_ROWS = 500;
      // Let the shell render, and a click take effect, before reading rows.
      const SETTLE_MS = 6000;
      // An open Recap app showing no rows and no loading placeholders for
      // this long has nothing to list.
      const EMPTY_MS = 20000;
      // The list is complete once it has neither grown nor scrolled deeper for
      // this long: older rows load in pages as the list scrolls.
      const IDLE_MS = 12000;
      // While it waits, the list is scrolled back up a screen this often, so
      // the next scroll brings its end into view again and a request for
      // older rows that went nowhere is made again.
      const NUDGE_MS = 3000;
      const host = location.hostname.toLowerCase();
      if (!['teams.microsoft.com', 'teams.cloud.microsoft',
            'teams.microsoft.com.mcas.ms', 'teams.cloud.microsoft.mcas.ms'].includes(host)) {
        return { status: 'waiting', href: location.href };
      }
      const ROW = '[data-tid="podcast-meeting-item"]';
      const text = el => (el?.innerText || el?.textContent || '').replace(/\s+/g, ' ').trim();
      const label = el => (el.getAttribute('aria-label') || text(el)).trim();
      const visible = el => el.getClientRects().length > 0;
      const state = window.__meetilyRecapList ||= {
        clicks: {}, opened: {}, seen: {}, order: [], kinds: {}, withoutProps: 0, errors: [], oldest: null, stop: '',
        firstSeen: Date.now(), lastClick: 0, emptySince: 0, idleSince: 0, lastCount: -1,
        paneEl: null, deepest: 0, scrolls: 0, nudges: 0, nudgedAt: 0, grows: [], reloading: false
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
      // the recap's Share button uses. Every row seen so far names its
      // recording file (`recommendationData.url`, even for a meeting whose
      // row lists no recording), so the link keys on that file like a pasted
      // link does. The link uses this page's own host: the recap import
      // opens it in a fresh window, and a teams.microsoft.com link first
      // spends about 40 seconds on the redirect to teams.cloud.microsoft.
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
          recapUrl: location.origin + '/l/meetingrecap?' +
            params.map(([key, value]) => key + '=' + encodeURIComponent(value)).join('&'),
          linkKind: file ? 'file' : has('driveItemId') ? 'item' : 'thread',
          recordedAt: starts.length ? new Date(Math.min(...starts)) : null
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
          }
        };
      };

      // ---- one reload when the list stops before the since date. On the
      // first listing after Meetily started, the Recap app showed its first
      // page of rows and never loaded older ones, while a page loaded later
      // in the same session listed the whole window. The mark lives in this
      // window's sessionStorage, and every listing opens a new window. It
      // holds the first pass's counts, for the probe. ----
      const RELOAD_KEY = 'meetilyRecapListReload';
      const firstPass = () => {
        try {
          return JSON.parse(sessionStorage.getItem(RELOAD_KEY) || 'null');
        } catch (e) {
          return null;
        }
      };
      const reloadOnce = () => {
        try {
          if (sessionStorage.getItem(RELOAD_KEY) !== null) return false;
          sessionStorage.setItem(RELOAD_KEY, JSON.stringify({
            rows: state.order.length, scrolls: state.scrolls, nudges: state.nudges, grows: state.grows,
            pane: state.pane || null, firstRowsMs: state.firstRowsMs, reloadMs: Math.round(performance.now())
          }));
        } catch (e) {
          noteError('reload', e);
          return false;
        }
        state.reloading = true;
        setTimeout(() => location.reload(), 50);
        return true;
      };

      // ---- probe only: counts and timings of this listing, written to a
      // local file by the debug command. No titles, links or row data. ----
      // Whether the page paints while the window is still hidden: older rows
      // load on scroll, which may need painting.
      if (PROBE && !state.counting) {
        state.counting = true;
        state.frames = 0;
        const tick = () => { state.frames += 1; requestAnimationFrame(tick); };
        requestAnimationFrame(tick);
      }
      const report = () => ({
        href: location.origin + location.pathname,
        since: SINCE.toISOString(),
        clicks: state.clicks,
        errors: state.errors,
        rows: state.order.length,
        kinds: state.kinds,
        rowsWithoutProps: state.withoutProps,
        stop: state.stop || null,
        firstRowsMs: state.firstRowsMs,
        listMs: state.listMs,
        frames: state.frames,
        scrolls: state.scrolls,
        nudges: state.nudges,
        // [ms since the page started, rows] each time the list grew.
        grows: state.grows,
        pane: state.pane,
        firstPass: firstPass()
      });
      const reply = (status, extra) =>
        ({ status, href: location.href, ...(extra || {}), probe: PROBE ? report() : undefined });
      if (state.reloading) return reply('reloading');

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

      const finishList = (items, why) => {
        state.stop ||= why;
        state.listMs ||= Math.round(performance.now());
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
          if (Date.now() - state.emptySince >= EMPTY_MS) return finishList([], 'empty');
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
      // the oldest slot seen decides when to stop. A meeting can have two
      // rows, one for its transcript and one for its recording: both name
      // the same file, and teams_recap_list.rs keeps the first. ----
      let pastWindow = null;
      for (const row of rows) {
        let read;
        try {
          read = readRow(row);
        } catch (e) {
          noteError('row', e);
          continue;
        }
        const { item, meeting, scheduled } = read;
        const key = item.recapUrl || item.title + '|' + item.startedAt;
        if (!state.seen[key]) {
          state.seen[key] = true;
          state.order.push(item);
          const kind = item.linkKind || 'none';
          state.kinds[kind] = (state.kinds[kind] || 0) + 1;
          if (!meeting) state.withoutProps += 1;
        }
        const slot = scheduled || instant(item.startedAt);
        if (slot && (!state.oldest || slot < state.oldest)) state.oldest = slot;
        if (slot && slot < SINCE) pastWindow ||= row;
      }
      const items = state.order.filter(item => !item.startedAt || new Date(item.startedAt) >= SINCE);
      const pastEnd = !!state.oldest && state.oldest < SINCE;
      const grew = state.order.length !== state.lastCount;
      if (grew && state.lastCount >= 0 && state.grows.length < 40) {
        state.grows.push([Math.round(performance.now()), state.order.length]);
      }
      state.lastCount = state.order.length;

      // Open groups the list shows collapsed ("Last week", ...). A re-render
      // can collapse a group again, so each may be opened a few times. Once
      // the list has reached rows from before the window, only a group above
      // the first of them can still hold rows inside it.
      const accordion = rows[0].closest('.fui-Accordion');
      const collapsed = accordion ? [...accordion.querySelectorAll('.fui-AccordionHeader__button[aria-expanded="false"]')]
        .filter(el => visible(el) && (state.opened[text(el).slice(0, 40)] || 0) < 3) : [];
      const group = pastEnd
        ? collapsed.find(el => pastWindow && (el.compareDocumentPosition(pastWindow) & Node.DOCUMENT_POSITION_FOLLOWING))
        : collapsed[0];
      if (group) {
        const name = text(group).slice(0, 40);
        state.opened[name] = (state.opened[name] || 0) + 1;
        state.clicks['group ' + name] = new Date().toISOString();
        group.click();
        state.idleSince = 0;
        return reply('page', { items, more: true });
      }
      if (pastEnd) return finishList(items, 'since');
      if (state.order.length >= MAX_ROWS) return finishList(items, 'cap');

      // Scroll the list's own pane (not the page) to load older rows.
      const lastRow = rows[rows.length - 1];
      let pane = lastRow.parentElement;
      while (pane && pane !== document.body) {
        if (pane.scrollHeight > pane.clientHeight + 20 && /auto|scroll/.test(getComputedStyle(pane).overflowY)) break;
        pane = pane.parentElement;
      }
      if (!pane || pane === document.body) pane = document.scrollingElement;
      if (state.paneEl !== pane) {
        state.paneEl = pane;
        state.deepest = 0;
      }
      const before = pane.scrollTop;
      pane.scrollTop += Math.max(300, pane.clientHeight * 0.8);
      const moved = pane.scrollTop > before;
      // Progress is a new row or a scroll deeper than any before. Scrolling
      // down again after a nudge is neither, so it never resets the wait.
      const deeper = pane.scrollTop > state.deepest;
      state.deepest = Math.max(state.deepest, pane.scrollTop);
      if (deeper) state.scrolls += 1;
      state.pane = pane.scrollTop + '/' + pane.scrollHeight + '/' + pane.clientHeight;
      if (grew || deeper) {
        state.idleSince = 0;
        state.nudgedAt = 0;
      } else {
        const now = Date.now();
        state.idleSince ||= now;
        const loadMore = buttons().find(el => !el.closest(ROW) && !inRail(el) &&
          /^(load|show|see) more( meetings| recaps)?$/i.test(label(el)));
        if (loadMore && !state.clicks['more ' + state.order.length]) {
          state.clicks['more ' + state.order.length] = new Date().toISOString();
          loadMore.click();
          state.idleSince = 0;
        } else if (!moved && now - (state.nudgedAt || state.idleSince) >= NUDGE_MS) {
          pane.scrollTop = Math.max(0, pane.scrollTop - pane.clientHeight);
          state.nudgedAt = now;
          state.nudges += 1;
        }
      }
      if (state.idleSince && Date.now() - state.idleSince >= IDLE_MS) {
        if (reloadOnce()) return reply('reloading');
        return finishList(items, 'idle');
      }
      return reply('page', { items, more: true });
    })()
