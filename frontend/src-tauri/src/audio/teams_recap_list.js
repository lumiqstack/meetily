(() => {
      // Rendered by teams_recap_list.rs: %SINCE% is an RFC 3339 instant and
      // %PROBE% a boolean. The script reads the Meet app's recap list and
      // never exposes Tauri IPC or sends page text anywhere.
      const SINCE = new Date('%SINCE%');
      const PROBE = %PROBE%;
      const MAX_ROWS = 600;
      const host = location.hostname.toLowerCase();
      if (!['teams.microsoft.com', 'teams.cloud.microsoft',
            'teams.microsoft.com.mcas.ms', 'teams.cloud.microsoft.mcas.ms'].includes(host)) {
        return { status: 'waiting', href: location.href };
      }
      const text = el => (el?.innerText || el?.textContent || '').replace(/\s+/g, ' ').trim();
      const label = el => (el.getAttribute('aria-label') || text(el)).trim();
      const visible = el => el.getClientRects().length > 0;
      const state = window.__meetilyRecapList ||= {
        clicks: {}, seen: {}, order: [], idle: 0, lastCount: -1
      };
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
      const choices = [...document.querySelectorAll('a,button,[role="button"]')];
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
      let rows = [...document.querySelectorAll('[role="row"],[role="listitem"],li,article,a[href*="meetingrecap"]')]
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
      const probe = () => {
        if (!PROBE) return undefined;
        const pick = (selector, limit) => [...document.querySelectorAll(selector)].filter(visible).slice(0, limit).map(brief);
        const bodyHtml = document.body.innerHTML;
        return {
          href: location.href,
          title: document.title,
          since: SINCE.toISOString(),
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
            attrs: idAttrs(row)
          })),
          tids: [...new Set([...document.querySelectorAll('[data-tid]')].map(el => el.getAttribute('data-tid')))].slice(0, 250),
          hrefs: [...new Set([...document.querySelectorAll('a[href]')].map(a => a.href)
            .filter(h => /meetingrecap|thread|meet|recap/i.test(h)))].slice(0, 40),
          threads: [...new Set(bodyHtml.match(new RegExp(threadId.source, 'gi')) || [])].slice(0, 20),
          recapLinksInHtml: [...new Set(bodyHtml.replace(/&amp;/g, '&').match(new RegExp(recapHref.source, 'gi')) || [])].slice(0, 20),
          bodyText: text(document.body).slice(0, 2000),
          clicks: state.clicks
        };
      };

      // ---- no rows yet: steer toward the Meet app's recap list, once per step ----
      if (!rows.length) {
        const clickOnce = (key, find) => {
          if (state.clicks[key]) return false;
          const el = find();
          if (!el) return false;
          state.clicks[key] = new Date().toISOString();
          el.click();
          return true;
        };
        const buttons = () => [...document.querySelectorAll('[role="tab"],[role="button"],button,a')].filter(visible);
        const unselected = el => el.getAttribute('aria-selected') !== 'true' && el.getAttribute('aria-current') !== 'page';
        if (clickOnce('recapTab', () => buttons().find(el =>
              /^(recaps?|meeting recaps?|past|past meetings|recent)$/i.test(label(el)) && unselected(el)))) {
          return { status: 'navigating', href: location.href, probe: probe() };
        }
        if (clickOnce('railApp', () => buttons().find(el =>
              /^(meet|recap|recaps|meeting recaps?)(?:\s*,?\s*\d+ unread.*)?$/i.test(label(el)) && unselected(el)))) {
          return { status: 'navigating', href: location.href, probe: probe() };
        }
        const page = text(document.body).slice(0, 1200);
        if (/open (your|the) teams app|join (on|in) (the )?teams app|use the web app/i.test(page)) {
          return { status: 'launcher', href: location.href, probe: probe() };
        }
        return { status: 'waiting', href: location.href, probe: probe() };
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
      let pane = rows[rows.length - 1].parentElement;
      while (pane && pane !== document.body) {
        if (pane.scrollHeight > pane.clientHeight + 20 &&
            /auto|scroll/.test(getComputedStyle(pane).overflowY)) break;
        pane = pane.parentElement;
      }
      if (!pane || pane === document.body) pane = document.scrollingElement;
      const before = pane.scrollTop;
      pane.scrollTop += Math.max(300, pane.clientHeight * 0.8);
      const moved = pane.scrollTop > before;
      const grew = state.order.length !== state.lastCount;
      state.lastCount = state.order.length;
      state.idle = moved || grew ? 0 : state.idle + 1;
      const loadMore = [...document.querySelectorAll('button,[role="button"]')]
        .filter(visible).find(el => /^(load more|show more|see more|more)$/i.test(label(el)));
      if (!moved && !grew && loadMore && !state.clicks['loadMore' + state.order.length]) {
        state.clicks['loadMore' + state.order.length] = new Date().toISOString();
        loadMore.click();
        state.idle = 0;
      }
      if (state.idle >= 3) return finish();
      return { status: 'page', items, more: true, href: location.href, probe: probe() };
    })()
