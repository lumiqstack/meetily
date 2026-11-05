(() => {
      // Rendered by teams_recap_list.rs: %SINCE% is an RFC 3339 instant and
      // %PROBE% a boolean. The script reads the Meet app's recap list and
      // never exposes Tauri IPC or sends page text anywhere.
      const SINCE = new Date('%SINCE%');
      const PROBE = %PROBE%;
      const MAX_ROWS = 600;
      // Let the shell render, and a click take effect, before reading rows.
      const SETTLE_MS = 6000;
      const host = location.hostname.toLowerCase();
      if (!['teams.microsoft.com', 'teams.cloud.microsoft',
            'teams.microsoft.com.mcas.ms', 'teams.cloud.microsoft.mcas.ms'].includes(host)) {
        return { status: 'waiting', href: location.href };
      }
      const text = el => (el?.innerText || el?.textContent || '').replace(/\s+/g, ' ').trim();
      const label = el => (el.getAttribute('aria-label') || text(el)).trim();
      const visible = el => el.getClientRects().length > 0;
      const state = window.__meetilyRecapList ||= {
        clicks: {}, seen: {}, order: [], idle: 0, lastCount: -1, firstSeen: Date.now(), lastClick: 0
      };
      // The page and any same-origin frame an app renders in. A cross-origin
      // frame is out of reach from here; the probe reports it.
      const documents = () => {
        const found = [document];
        for (const frame of document.querySelectorAll('iframe')) {
          try { if (frame.contentDocument?.body) found.push(frame.contentDocument); } catch (e) { /* cross-origin */ }
        }
        return found;
      };
      const all = selector => documents().flatMap(doc => [...doc.querySelectorAll(selector)]);
      const recapHref = /https?:\/\/[^"'\s<>]*\/l\/meetingrecap\?[^"'\s<>]*/i;
      const threadId = /19:meeting_[A-Za-z0-9+/=_-]+@thread\.v2/i;
      const months = '(?:jan|feb|mar|apr|may|jun|jul|aug|sep|sept|oct|nov|dec)[a-z]*\\.?';
      const clock = '\\d{1,2}:\\d{2}(?:\\s?[ap]\\.?m\\.?)?';
      const dateLine = new RegExp(
        '(?:(?:mon|tue|wed|thu|fri|sat|sun)[a-z]*,?\\s+)?(' +
        months + '\\s+\\d{1,2}(?:,?\\s+\\d{4})?|\\d{1,2}\\s+' + months + '(?:\\s+\\d{4})?|' +
        '\\d{1,2}/\\d{1,2}/\\d{2,4})(?:[,\\s]+(?:at\\s+)?(' + clock + '))?', 'i');
      const relativeLine = new RegExp('^(today|yesterday)\\b(?:[,\\s]+(?:at\\s+)?(' + clock + '))?', 'i');
      const lines = el => (el.innerText || '').split('\n').map(s => s.trim()).filter(Boolean);

      // ---- app-choice page, same handling as the recap import ----
      const choices = all('a,button,[role="button"]');
      const webChoice = choices.find(el =>
        /^(continue (on|in) (this|the) browser|use the web app( instead)?|open (in|on) (the )?web( app)?|continue in browser)$/i
          .test(text(el) || el.getAttribute('aria-label') || ''));
      if (webChoice) {
        webChoice.click();
        return { status: 'opening_web', href: location.href };
      }

      // ---- what a meeting row looks like ----
      const parseWhen = row => {
        const stamp = row.querySelector('time[datetime]');
        if (stamp) {
          const when = new Date(stamp.getAttribute('datetime'));
          if (!isNaN(when)) return when;
        }
        for (const line of lines(row)) {
          const rel = line.match(relativeLine);
          if (rel) {
            const day = new Date();
            if (/yesterday/i.test(rel[1])) day.setDate(day.getDate() - 1);
            if (rel[2]) {
              const at = new Date(day.toDateString() + ' ' + rel[2].replace(/\./g, ''));
              if (!isNaN(at)) return at;
            }
            day.setHours(0, 0, 0, 0);
            return day;
          }
          const m = line.match(dateLine);
          if (m) {
            let value = m[1];
            if (!/\d{4}/.test(value)) value += ' ' + new Date().getFullYear();
            if (m[2]) value += ' ' + m[2].replace(/\./g, '');
            const when = new Date(value);
            if (!isNaN(when)) {
              // A list without years that reaches into "next month" is last year's.
              if (when.getTime() > Date.now() + 86400000) when.setFullYear(when.getFullYear() - 1);
              return when;
            }
          }
        }
        return null;
      };
      const linkOf = row => {
        const anchor = [...row.querySelectorAll('a[href]')].map(a => a.href).find(h => recapHref.test(h));
        if (anchor) return { url: anchor, kind: 'href' };
        const html = row.outerHTML.replace(/&amp;/g, '&');
        const embedded = html.match(recapHref);
        if (embedded) return { url: embedded[0], kind: 'embedded' };
        const thread = html.match(threadId);
        if (thread) {
          return {
            url: 'https://teams.microsoft.com/l/meetingrecap?threadId=' + encodeURIComponent(thread[0]),
            kind: 'thread'
          };
        }
        return null;
      };
      const titleOf = row => {
        const heading = row.querySelector('h1,h2,h3,h4,h5,h6,[role="heading"],strong,b,[data-tid*="title" i],[class*="title" i]');
        const value = heading ? text(heading) : '';
        if (value && !dateLine.test(value) && !relativeLine.test(value)) return value.slice(0, 200);
        const first = lines(row).find(line => !dateLine.test(line) && !relativeLine.test(line) && line.length > 2);
        return (first || '').slice(0, 200);
      };
      const outsideLists = el => !!el.closest('nav,[role="navigation"],[role="tablist"],[role="menu"],[role="menubar"],[role="toolbar"]');
      const looksLikeRow = el => {
        if (!visible(el) || outsideLists(el)) return false;
        const value = text(el);
        if (value.length < 3 || value.length > 700) return false;
        return parseWhen(el) !== null || linkOf(el) !== null;
      };
      let rows = all('[role="row"],[role="listitem"],li,article,a[href*="meetingrecap"]')
        .filter(looksLikeRow);
      // A row may nest list items: keep the outermost element only.
      rows = rows.filter(el => !rows.some(parent => parent !== el && parent.contains(el)));

      // ---- probe for learning the layout; only returned when asked ----
      const brief = el => {
        const className = typeof el.className === 'string' ? el.className : '';
        return {
          tag: el.tagName.toLowerCase(),
          role: el.getAttribute('role') || undefined,
          tid: el.getAttribute('data-tid') || undefined,
          id: el.id || undefined,
          cls: className ? className.slice(0, 100) : undefined,
          label: (el.getAttribute('aria-label') || '').slice(0, 100) || undefined,
          text: text(el).slice(0, 140) || undefined
        };
      };
      const idAttrs = root => {
        const found = [];
        for (const el of [root, ...root.querySelectorAll('*')]) {
          for (const attr of el.attributes) {
            if (/19:|meetingrecap|recap|thread|callid|organizer|fileurl|icaluid/i.test(attr.value) ||
                /^data-/.test(attr.name) && attr.name !== 'data-tid') {
              found.push(el.tagName.toLowerCase() + '@' + attr.name + '=' + attr.value.slice(0, 160));
              if (found.length >= 25) return found;
            }
          }
        }
        return found;
      };
      // Data a row keeps in its React props rather than its markup: id-looking
      // strings only, with their key path. Bounded, because props reach the
      // whole component tree.
      const fiberIds = root => {
        const key = Object.keys(root).find(k => k.startsWith('__reactFiber$'));
        if (!key) return [];
        const wanted = /19:|meetingrecap|fileurl|sharepoint|thread|callid|icaluid|organizer|recap/i;
        const skip = new Set(['children', '_owner', '_store', 'ref', 'style', 'className', 'return', 'stateNode']);
        const found = [];
        const seen = new WeakSet();
        let budget = 3000;
        const walk = (value, path, depth) => {
          if (budget-- <= 0 || found.length >= 25) return;
          if (typeof value === 'string') {
            if (wanted.test(value) || wanted.test(path)) found.push(path + '=' + value.slice(0, 300));
            return;
          }
          if (!value || typeof value !== 'object' || depth > 5 || seen.has(value)) return;
          seen.add(value);
          for (const name of Object.keys(value).slice(0, 60)) {
            if (!skip.has(name)) walk(value[name], path + '.' + name, depth + 1);
          }
        };
        let fiber = root[key];
        for (let level = 0; fiber && level < 8; level++, fiber = fiber.return) {
          walk(fiber.memoizedProps, 'p' + level, 0);
        }
        return found;
      };
      const frames = () => [...document.querySelectorAll('iframe')].map(frame => {
        const rect = frame.getBoundingClientRect();
        let sameOrigin = false;
        try { sameOrigin = !!frame.contentDocument; } catch (e) { /* cross-origin */ }
        let src = '';
        try { const url = new URL(frame.src, location.href); src = url.origin + url.pathname; } catch (e) { /* no src */ }
        return { src, name: frame.name || undefined, title: frame.title || undefined,
                 w: Math.round(rect.width), h: Math.round(rect.height), sameOrigin };
      });
      // Tokens never leave the page, even into the local probe file.
      const redact = value => JSON.parse(JSON.stringify(value)
        .replace(/(tempauth|access_token|token|sig|code)(=|%3D)[^&%"'\s\\]+/gi, '$1$2REDACTED'));
      const OPENED_KEY = 'meetilyRecapRowOpened';
      const CAPTURE_KEY = 'meetilyRecapRowCapture';
      const session = {
        get: key => { try { return sessionStorage.getItem(key); } catch (e) { return state.clicks[key] ?? null; } },
        set: (key, value) => { try { sessionStorage.setItem(key, value); } catch (e) { state.clicks[key] = value; } }
      };
      const probe = () => {
        if (!PROBE) return undefined;
        const pick = (selector, limit) => all(selector).filter(visible).slice(0, limit).map(brief);
        const bodyHtml = documents().map(doc => doc.body.innerHTML).join('\n');
        return redact({
          href: location.href,
          title: document.title,
          since: SINCE.toISOString(),
          frames: frames(),
          rails: pick('[role="tab"],[role="button"][aria-label],nav a,nav button,[data-tid*="app-bar" i] button', 80),
          headings: pick('h1,h2,h3,[role="heading"]', 40),
          lists: pick('[role="grid"],[role="list"],[role="table"],ul,ol', 20),
          rowsFound: rows.length,
          rowSamples: rows.slice(0, 12).map(row => ({
            ...brief(row),
            lines: lines(row).slice(0, 8),
            when: parseWhen(row)?.toISOString() || null,
            link: linkOf(row),
            title: titleOf(row),
            attrs: idAttrs(row),
            props: fiberIds(row)
          })),
          tids: [...new Set(all('[data-tid]').map(el => el.getAttribute('data-tid')))].slice(0, 250),
          hrefs: [...new Set(all('a[href]').map(a => a.href)
            .filter(h => /meetingrecap|thread|meet|recap/i.test(h)))].slice(0, 40),
          threads: [...new Set(bodyHtml.match(new RegExp(threadId.source, 'gi')) || [])].slice(0, 20),
          recapLinksInHtml: [...new Set(bodyHtml.replace(/&amp;/g, '&').match(new RegExp(recapHref.source, 'gi')) || [])].slice(0, 20),
          bodyText: text(document.body).slice(0, 2000),
          clicks: state.clicks,
          afterRowOpened: JSON.parse(session.get(CAPTURE_KEY) || 'null')
        });
      };

      const navigating = () => ({ status: 'navigating', href: location.href, probe: probe() });
      const clickNow = (key, el) => {
        state.clicks[key] = new Date().toISOString();
        state.lastClick = Date.now();
        el.click();
        return navigating();
      };
      const settled = () => Date.now() - Math.max(state.firstSeen, state.lastClick) >= SETTLE_MS;

      // ---- probe only: a row was opened on an earlier poll. Record what that
      // showed, then go back to the list. Kept in sessionStorage because
      // opening a row may be a full page load, which resets `state`. ----
      if (PROBE && session.get(OPENED_KEY) && !session.get(CAPTURE_KEY)) {
        if (!settled()) return navigating();
        const bodyHtml = documents().map(doc => doc.body.innerHTML).join('\n');
        session.set(CAPTURE_KEY, JSON.stringify(redact({
          href: location.href,
          title: document.title,
          frames: frames(),
          headings: all('h1,h2,h3,[role="heading"]').filter(visible).slice(0, 20).map(brief),
          tabs: all('[role="tab"]').filter(visible).slice(0, 20).map(brief),
          threads: [...new Set(bodyHtml.match(new RegExp(threadId.source, 'gi')) || [])].slice(0, 5),
          recapLinksInHtml: [...new Set(bodyHtml.replace(/&amp;/g, '&').match(new RegExp(recapHref.source, 'gi')) || [])].slice(0, 5)
        })));
        state.lastClick = Date.now();
        history.back();
        return navigating();
      }

      // ---- steer to the Meet app's recap list, once per step, before any
      // row counts: the shell may open on Chat or Activity, whose rows carry
      // dates too ----
      const buttons = () => all('[role="tab"],[role="button"],[role="menuitem"],[role="menuitemradio"],[role="option"],button,a').filter(visible);
      const named = (el, pattern) => pattern.test(label(el)) || pattern.test(text(el));
      const isSelected = el => el.getAttribute('aria-selected') === 'true' ||
        el.getAttribute('aria-current') === 'page' || el.getAttribute('aria-pressed') === 'true';
      const railName = /^(meet|recaps?|meeting recaps?)(?:\s*[,(].*)?$/i;
      const tabName = /^(recaps?|meeting recaps?|past|past meetings|recent)(?:\s*[,(].*)?$/i;
      const moreApps = /^(view more apps|more added apps|more apps)(?:\s*[,(].*)?$/i;
      if (!state.clicks.railApp) {
        const rail = buttons().find(el => named(el, railName));
        if (rail && isSelected(rail)) {
          state.clicks.railApp = 'already selected';
        } else if (rail) {
          return clickNow('railApp', rail);
        } else if (!state.clicks.moreApps) {
          const more = buttons().find(el => named(el, moreApps));
          if (more) return clickNow('moreApps', more);
        }
      }
      if (state.clicks.railApp && !state.clicks.recapTab && Date.now() - state.lastClick >= SETTLE_MS) {
        const tab = buttons().find(el => named(el, tabName) && !isSelected(el));
        if (tab) return clickNow('recapTab', tab);
      }
      if (!settled()) return navigating();

      if (!rows.length) {
        const page = text(document.body).slice(0, 1200);
        if (/open (your|the) teams app|join (on|in) (the )?teams app|use the web app/i.test(page)) {
          return { status: 'launcher', href: location.href, probe: probe() };
        }
        return { status: 'waiting', href: location.href, probe: probe() };
      }

      // ---- probe only: rows without a link, so learn what opening one does.
      // The row itself, never a button inside it, and only a past meeting:
      // an upcoming row may hold Join. ----
      if (PROBE && !rows.some(row => linkOf(row)) && !session.get(OPENED_KEY)) {
        const past = rows.find(row => { const when = parseWhen(row); return when && when < new Date(); });
        if (past) {
          session.set(OPENED_KEY, '1');
          return clickNow('rowOpened', past);
        }
      }

      // ---- collect rows, oldest date seen decides when to stop ----
      let oldest = null;
      for (const row of rows) {
        const link = linkOf(row);
        const when = parseWhen(row);
        const title = titleOf(row);
        const key = link ? link.url : title + '|' + (when ? when.toISOString() : '');
        if (!state.seen[key]) {
          state.seen[key] = true;
          state.order.push({
            recapUrl: link ? link.url : null,
            linkKind: link ? link.kind : null,
            title,
            startedAt: when ? when.toISOString() : null
          });
        }
        if (when && (!oldest || when < oldest)) oldest = when;
      }
      const items = state.order.filter(item => !item.startedAt || new Date(item.startedAt) >= SINCE);
      const reachedSince = oldest !== null && oldest < SINCE;
      const finish = () => ({ status: 'done', items, href: location.href, probe: probe() });
      if (reachedSince || state.order.length >= MAX_ROWS) return finish();

      // Scroll the list's own pane (not the page) to reveal older rows.
      const lastRow = rows[rows.length - 1];
      let pane = lastRow.parentElement;
      while (pane && pane !== lastRow.ownerDocument.body) {
        if (pane.scrollHeight > pane.clientHeight + 20 &&
            /auto|scroll/.test(getComputedStyle(pane).overflowY)) break;
        pane = pane.parentElement;
      }
      if (!pane || pane === lastRow.ownerDocument.body) pane = lastRow.ownerDocument.scrollingElement;
      const before = pane.scrollTop;
      pane.scrollTop += Math.max(300, pane.clientHeight * 0.8);
      const moved = pane.scrollTop > before;
      const grew = state.order.length !== state.lastCount;
      state.lastCount = state.order.length;
      state.idle = moved || grew ? 0 : state.idle + 1;
      const loadMore = all('button,[role="button"]')
        .filter(visible).find(el => /^(load more|show more|see more|more)$/i.test(label(el)));
      if (!moved && !grew && loadMore && !state.clicks['loadMore' + state.order.length]) {
        state.clicks['loadMore' + state.order.length] = new Date().toISOString();
        loadMore.click();
        state.idle = 0;
      }
      if (state.idle >= 3) return finish();
      return { status: 'page', items, more: true, href: location.href, probe: probe() };
    })()
