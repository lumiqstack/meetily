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
        clicks: {}, clicked: {}, seen: {}, order: [], idle: 0, lastCount: -1, firstSeen: Date.now(), lastClick: 0,
        errors: [], net: [], stages: null, snapSig: null, pendingSig: null, pendingSince: 0, lastProbeAt: 0
      };
      const session = {
        get: key => { try { return sessionStorage.getItem(key); } catch (e) { return null; } },
        set: (key, value) => { try { sessionStorage.setItem(key, value); } catch (e) { /* quota or blocked */ } }
      };
      const STAGES_KEY = 'meetilyRecapStages';
      state.stages ||= JSON.parse(session.get(STAGES_KEY) || '[]');

      // ---- probe only: note the page's recap and meeting calls, so the report
      // shows where the list's data comes from. Key names and short values
      // only; nothing leaves the page except through the local probe file. ----
      const SECRET_KEY = /token|auth|secret|password|cookie|session|credential|signature|^sig$|^key$/i;
      const shapeOf = root => {
        const out = [];
        const walk = (value, path, depth) => {
          if (out.length >= 60) return;
          if (Array.isArray(value)) {
            out.push(path + ' [' + value.length + ']');
            value.slice(0, 2).forEach((item, i) => walk(item, path + '[' + i + ']', depth + 1));
          } else if (value && typeof value === 'object') {
            if (depth < 7) for (const name of Object.keys(value).slice(0, 40)) walk(value[name], path + '.' + name, depth + 1);
          } else if (SECRET_KEY.test(path.slice(path.lastIndexOf('.') + 1))) {
            out.push(path + ' <hidden>');
          } else if (typeof value === 'string') {
            const prose = /\s/.test(value) && value.length > 70;
            out.push(path + ' = ' + (prose ? '<text ' + value.length + ' chars>' : value.slice(0, 300)));
          } else {
            out.push(path + ' = ' + value);
          }
        };
        walk(root, '$', 0);
        return out;
      };
      const urlKey = url => { try { const u = new URL(url, location.href); return u.origin + u.pathname; } catch (e) { return null; } };
      const netWanted = url => {
        const key = urlKey(url);
        return !!key && /recap|meeting|calendar|substrate|stream|intelligent|transcript|graphql|sharepoint/i.test(key) &&
          !/telemetry|browser\.events|beacon|collect|trouter|presence|\.(js|css|png|svg|woff2?|map)$/i.test(key);
      };
      const noteCall = (url, status, body) => {
        const key = urlKey(url);
        if (!key || state.net.length >= 30 || state.net.filter(call => call.u === key).length >= 2) return;
        let json = null;
        try { json = JSON.parse(body); } catch (e) { /* not JSON */ }
        state.net.push({ u: key.slice(0, 200), s: status, n: (body || '').length, shape: json ? shapeOf(json) : undefined });
      };
      if (PROBE && !state.hooked) {
        state.hooked = true;
        try { performance.setResourceTimingBufferSize(3000); } catch (e) { /* older engine */ }
        const realFetch = window.fetch;
        window.fetch = function (...args) {
          const pending = realFetch.apply(this, args);
          try {
            const url = typeof args[0] === 'string' ? args[0] : args[0]?.url || String(args[0]);
            if (netWanted(url)) {
              pending.then(res => res.clone().text().then(body => noteCall(url, res.status, body))).catch(() => {});
            }
          } catch (e) { /* never disturb the page */ }
          return pending;
        };
        const realOpen = XMLHttpRequest.prototype.open;
        const realSend = XMLHttpRequest.prototype.send;
        XMLHttpRequest.prototype.open = function (method, url, ...rest) {
          this.__meetilyUrl = url;
          return realOpen.call(this, method, url, ...rest);
        };
        XMLHttpRequest.prototype.send = function (...args) {
          try {
            if (netWanted(this.__meetilyUrl)) {
              this.addEventListener('load', () => {
                try {
                  const body = this.responseType === '' || this.responseType === 'text' ? this.responseText
                    : this.responseType === 'json' ? JSON.stringify(this.response) : '';
                  noteCall(this.__meetilyUrl, this.status, body);
                } catch (e) { /* never disturb the page */ }
              });
            }
          } catch (e) { /* never disturb the page */ }
          return realSend.apply(this, args);
        };
      }

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
      const findRows = () => {
        const found = all('[role="row"],[role="listitem"],[role="treeitem"],[role="option"],li,article,a[href*="meetingrecap"]')
          .filter(looksLikeRow);
        // A row may nest list items: keep the outermost element only.
        return found.filter(el => !found.some(parent => parent !== el && parent.contains(el)));
      };

      // ---- probe for learning the layout; only returned when asked ----
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
        const wantedValue = /19:[a-z0-9_-]+|meetingrecap|sharepoint\.com|040000008200e00074c5b7101a82e008/i;
        const wantedKey = /thread|callid|icaluid|organizer|fileurl|driveid|driveitem|sitepath|starttime|endtime|startdate|subject|meetingid|recapurl|deeplink|weburl/i;
        const skip = new Set(['children', '_owner', '_store', 'ref', 'style', 'className', 'return', 'stateNode', 'queue', 'baseQueue', 'deps', 'data-tid']);
        const found = [];
        const seen = new WeakSet();
        let budget = 8000;
        const walk = (value, path, depth) => {
          if (budget-- <= 0 || found.length >= 40) return;
          const last = path.slice(path.lastIndexOf('.') + 1);
          if (typeof value === 'string') {
            if (!SECRET_KEY.test(last) && (wantedValue.test(value) || wantedKey.test(last))) found.push(path + '=' + value.slice(0, 400));
            return;
          }
          if (typeof value === 'number') {
            if (wantedKey.test(last)) found.push(path + '=' + value);
            return;
          }
          if (value instanceof Date) {
            if (!isNaN(value)) found.push(path + '=' + value.toISOString());
            return;
          }
          if (!value || typeof value !== 'object' || value === window || value.nodeType || depth > 6 || seen.has(value)) return;
          seen.add(value);
          for (const name of Object.keys(value).slice(0, 60)) {
            if (!skip.has(name) && !name.startsWith('__react')) walk(value[name], path + '.' + name, depth + 1);
          }
        };
        let fiber = root[key];
        for (let level = 0; fiber && level < 10; level++, fiber = fiber.return) {
          walk(fiber.memoizedProps, 'p' + level, 0);
          walk(fiber.memoizedState, 's' + level, 0);
        }
        return found;
      };
      const frames = () => [...document.querySelectorAll('iframe')].map(frame => {
        const rect = frame.getBoundingClientRect();
        let sameOrigin = false;
        try { sameOrigin = !!frame.contentDocument; } catch (e) { /* cross-origin */ }
        const raw = frame.getAttribute('src') || '';
        let src;
        try { if (raw) { const url = new URL(raw, location.href); src = url.origin + url.pathname; } } catch (e) { /* no src */ }
        return { src, srcdoc: frame.hasAttribute('srcdoc') || undefined, name: frame.name || undefined,
                 title: frame.title || undefined, sandbox: frame.getAttribute('sandbox') ?? undefined,
                 w: Math.round(rect.width), h: Math.round(rect.height), sameOrigin };
      });
      // Tokens never leave the page, even into the local probe file.
      const redact = value => JSON.parse(JSON.stringify(value)
        .replace(/(tempauth|access_token|token|sig|code)(=|%3D)[^&%"'\s\\]+/gi, '$1$2REDACTED')
        .replace(/eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+/g, '<jwt>'));

      // ---- probe: what each view of the page looks like. The open recap's
      // own content (notes, transcript, speakers) is never described. ----
      const OPAQUE = '[data-tid="meeting-recap-main-panel"]';
      const RAIL = '[data-tid="app-bar-wrapper"],[data-tid="app-layout-area--nav"]';
      const inRail = el => !!el.closest(RAIL);
      const fuiOf = el => (typeof el.className === 'string' ? el.className.split(/\s+/).filter(c => c.startsWith('fui-')) : []).join('.');
      const ownText = el => [...el.childNodes].filter(n => n.nodeType === 3).map(n => n.nodeValue).join(' ').replace(/\s+/g, ' ').trim();
      const STATE_ATTRS = ['aria-selected', 'aria-current', 'aria-pressed', 'aria-expanded', 'aria-checked', 'aria-level', 'aria-posinset', 'aria-setsize', 'tabindex'];
      const shortName = el => {
        const parts = [el.tagName.toLowerCase()];
        const role = el.getAttribute('role');
        if (role) parts.push('[' + role + ']');
        const tid = el.getAttribute('data-tid');
        if (tid) parts.push('{' + tid.slice(0, 60) + '}');
        const fui = fuiOf(el);
        if (fui) parts.push('.' + fui);
        return parts.join(' ');
      };
      const describe = el => {
        const parts = [shortName(el)];
        for (const name of STATE_ATTRS) {
          const value = el.getAttribute(name);
          if (value !== null) parts.push(name.replace('aria-', '') + '=' + value);
        }
        const aria = el.getAttribute('aria-label');
        if (aria) parts.push('label="' + aria.slice(0, 90) + '"');
        const own = ownText(el);
        if (own) parts.push('"' + own.slice(0, 70) + '"');
        if (el.tagName === 'A' && el.getAttribute('href')) {
          const key = urlKey(el.getAttribute('href'));
          if (key) parts.push('href=' + key.slice(0, 80));
        }
        return parts.join(' ');
      };
      const pathOf = el => {
        const chain = [];
        for (let node = el; node && node !== document.body && chain.length < 8; node = node.parentElement) chain.push(shortName(node));
        return chain;
      };
      const worthALine = el => el.getAttribute('role') || el.getAttribute('data-tid') || el.getAttribute('aria-label') ||
        el.hasAttribute('tabindex') || ownText(el) || fuiOf(el) || /^(A|BUTTON|INPUT|LI|UL|OL|H[1-6]|TIME|IFRAME|TABLE|TR)$/.test(el.tagName);
      const outline = (root, limit) => {
        const out = [];
        let dropped = 0;
        const walk = (el, depth) => {
          if (/^(script|style|svg|path|link|meta)$/i.test(el.tagName) || getComputedStyle(el).display === 'none') return;
          const pad = ' '.repeat(Math.min(depth, 24));
          if (el.matches(OPAQUE)) {
            out.push(pad + shortName(el) + ' (not described: ' + el.getElementsByTagName('*').length + ' elements)');
            return;
          }
          const shown = worthALine(el);
          if (shown) {
            if (out.length < limit) out.push(pad + describe(el));
            else dropped++;
          }
          for (const child of el.children) walk(child, shown ? depth + 1 : depth);
        };
        walk(root, 0);
        return dropped ? out.concat('... ' + dropped + ' more') : out;
      };
      const dateish = value => value.length <= 300 && (dateLine.test(value) || relativeLine.test(value) ||
        /^(tomorrow|this week|last week|last month|earlier|older|(mon|tues?|wed(nes)?|thu(rs)?|fri|sat(ur)?|sun)(day)?)\b/i.test(value) ||
        new RegExp('\\b' + clock).test(value));
      const dateHits = () => {
        const found = [];
        for (const el of document.body.querySelectorAll('*')) {
          if (found.length >= 200) break;
          if (/^(SCRIPT|STYLE)$/.test(el.tagName) || el.closest(OPAQUE) || inRail(el)) continue;
          const own = ownText(el);
          if (el.tagName === 'TIME' || (own && dateish(own)) || dateish(el.getAttribute('aria-label') || '')) found.push(el);
        }
        return found;
      };
      // Rows without assuming their markup: climb from each date until a
      // parent holds three or more children that each hold a date.
      const rowGuesses = hits => {
        const lists = new Map();
        const checked = new Set();
        for (const hit of hits.slice(0, 120)) {
          let node = hit;
          for (let up = 0; up < 14 && node.parentElement; up++, node = node.parentElement) {
            const parent = node.parentElement;
            if (checked.has(parent)) break;
            checked.add(parent);
            const holding = [...parent.children].filter(child => hits.some(other => child.contains(other)));
            if (holding.length >= 3) {
              lists.set(parent, holding);
              break;
            }
          }
        }
        return [...lists.entries()].sort((a, b) => b[1].length - a[1].length).slice(0, 3).map(([list, kids]) => ({
          list: describe(list),
          path: pathOf(list),
          count: kids.length,
          listProps: fiberIds(list).slice(0, 12),
          rows: kids.slice(0, 8).map(row => ({
            el: describe(row),
            lines: lines(row).slice(0, 8),
            attrs: idAttrs(row),
            props: fiberIds(row)
          }))
        }));
      };
      const counts = values => {
        const tally = {};
        for (const value of values) if (value) tally[value] = (tally[value] || 0) + 1;
        return tally;
      };
      const snapshot = () => {
        const section = (name, read) => {
          try { return read(); } catch (e) { state.errors.push(name + ': ' + e.message); return undefined; }
        };
        const pick = (selector, limit) => all(selector).filter(visible).filter(el => !inRail(el) && !el.closest(OPAQUE)).slice(0, limit).map(describe);
        const hits = section('dates', dateHits) || [];
        const areaTids = ['app-layout-area--mid-nav', 'app-layout-area--sub-nav', 'app-layout-area--header', 'app-layout-area--start', 'app-layout-area--main'];
        return {
          at: new Date().toISOString(),
          sinceLoadMs: Math.round(performance.now()),
          visibility: document.visibilityState,
          title: document.title,
          href: location.href,
          rail: section('rail', () => {
            const rail = [...document.querySelectorAll('[data-tid="app-bar-wrapper"] button,[data-tid="app-bar-wrapper"] [role="button"]')];
            const raw = el => [...el.attributes].filter(a => a.name !== 'class' && a.name !== 'style').map(a => a.name + '=' + a.value.slice(0, 60));
            return {
              marked: rail.filter(el => STATE_ATTRS.slice(0, 4).some(name => el.getAttribute(name) !== null)).slice(0, 20).map(describe),
              recaps: rail.filter(el => /recap/i.test(label(el))).map(raw),
              chat: rail.filter(el => /^chat/i.test(label(el))).slice(0, 1).map(raw)
            };
          }),
          frames: section('frames', frames),
          areas: section('areas', () => [...document.querySelectorAll('[data-tid^="app-layout-area--"]')].map(el => {
            const rect = el.getBoundingClientRect();
            return el.getAttribute('data-tid').replace('app-layout-area--', '') + ' ' + Math.round(rect.width) + 'x' + Math.round(rect.height) +
              ' els=' + el.getElementsByTagName('*').length;
          })),
          outline: section('outline', () => Object.fromEntries(areaTids.map(tid => {
            const root = document.querySelector('[data-tid="' + tid + '"]');
            return [tid.replace('app-layout-area--', ''), root ? outline(root, 140) : null];
          }))),
          roles: section('roles', () => counts([...document.querySelectorAll('[role]')].map(el => el.getAttribute('role')))),
          components: section('components', () => counts([...document.querySelectorAll('[class*="fui-"]')].map(fuiOf))),
          lists: section('lists', () => pick('[role="grid"],[role="list"],[role="listbox"],[role="tree"],[role="table"],ul,ol', 20)),
          headings: section('headings', () => pick('h1,h2,h3,h4,[role="heading"]', 30)),
          tabs: section('tabs', () => pick('[role="tab"]', 30)),
          selected: section('selected', () => pick('[aria-selected="true"],[aria-current]', 12).concat(
            all('[aria-selected="true"],[aria-current]').filter(el => !inRail(el) && !el.closest(OPAQUE)).slice(0, 3).map(el => pathOf(el).join(' < ')))),
          controls: section('controls', () => pick('button,[role="button"],[role="link"],a,[role="checkbox"],[role="combobox"],[role="search"],input', 60)),
          dates: hits.slice(0, 30).map(el => ({ text: (ownText(el) || el.getAttribute('aria-label') || '').slice(0, 120), path: pathOf(el) })),
          rowGuesses: section('rowGuesses', () => rowGuesses(hits)),
          listRowsFound: section('listRows', () => findRows().length),
          panel: section('panel', () => Object.fromEntries(
            ['entity-header', 'intelligent-recap-header', 'Meeting-Recap-recording-container', 'recap-share-button', 'recap-open-in-stream-button', 'meeting-recap-main-panel']
              .map(tid => { const el = document.querySelector('[data-tid="' + tid + '"]'); return [tid, el ? fiberIds(el).slice(0, 25) : null]; }))),
          ids: section('ids', () => {
            const html = documents().map(doc => doc.body.innerHTML).join('\n');
            return {
              threads: [...new Set(html.match(new RegExp(threadId.source, 'gi')) || [])].slice(0, 10),
              recapLinks: [...new Set(html.replace(/&amp;/g, '&').match(new RegExp(recapHref.source, 'gi')) || [])].slice(0, 10)
            };
          }),
          tids: section('tids', () => [...new Set([...document.querySelectorAll('[data-tid]')].map(el => el.getAttribute('data-tid')))]
            .filter(tid => !/-\d+(-empty)?$/.test(tid)).slice(0, 150))
        };
      };
      // A new view is recorded once it has held still for a moment, whoever
      // caused it, so a list the user passes through by hand is not lost.
      const STAGE_STABLE_MS = 3000;
      const MAX_STAGES = 10;
      const signature = () => {
        const size = tid => document.querySelector('[data-tid="' + tid + '"]')?.getElementsByTagName('*').length || 0;
        const recapOpen = !!document.querySelector(OPAQUE);
        return [document.title.replace(/^\(\d+\)\s*/, ''), recapOpen,
          Math.round(size('app-layout-area--mid-nav') / 25), Math.round(size('app-layout-area--sub-nav') / 25),
          Math.round(size('app-layout-area--start') / 25), recapOpen ? 0 : Math.round(size('app-layout-area--main') / 100),
          document.querySelectorAll('iframe').length].join('|');
      };
      const watch = () => {
        const sig = signature();
        if (sig === state.snapSig) { state.pendingSig = null; return; }
        if (sig !== state.pendingSig) { state.pendingSig = sig; state.pendingSince = Date.now(); return; }
        if (Date.now() - state.pendingSince < STAGE_STABLE_MS) return;
        state.snapSig = sig;
        state.pendingSig = null;
        // Keep the first views and the latest ones.
        if (state.stages.length >= MAX_STAGES) state.stages.splice(2, 1);
        state.stages.push(snapshot());
        state.lastProbeAt = 0;
        session.set(STAGES_KEY, JSON.stringify(state.stages));
      };
      const resources = () => {
        const seen = new Map();
        for (const entry of performance.getEntriesByType('resource')) {
          const key = urlKey(entry.name);
          if (key && netWanted(entry.name) && !/\.json$/i.test(key)) seen.set(key, (seen.get(key) || 0) + 1);
        }
        return [...seen].slice(-60).map(([url, times]) => times + ' ' + url.slice(0, 200));
      };
      // Reporting every poll would slow an already busy page, and the app
      // keeps the last report it got.
      const probe = () => {
        if (!PROBE || Date.now() - state.lastProbeAt < 10000) return undefined;
        state.lastProbeAt = Date.now();
        return redact({
          try: 2,
          href: location.href,
          title: document.title,
          since: SINCE.toISOString(),
          clicks: state.clicks,
          clicked: state.clicked,
          errors: state.errors.slice(0, 20),
          net: state.net,
          resources: resources(),
          stages: state.stages
        });
      };

      const navigating = () => ({ status: 'navigating', href: location.href, probe: probe() });
      const clickNow = (key, el) => {
        state.clicks[key] = new Date().toISOString();
        if (PROBE) state.clicked[key] = describe(el);
        state.lastClick = Date.now();
        el.click();
        return navigating();
      };
      const settled = () => Date.now() - Math.max(state.firstSeen, state.lastClick) >= SETTLE_MS;

      if (PROBE) {
        try { watch(); } catch (e) { state.errors.push('watch: ' + e.message); }
      }

      // ---- steer to the recap app, once per step, before any row counts: the
      // shell may open on Chat or Activity, whose rows carry dates too. The
      // app is the first-party Recap app, named "Recap" or "Meeting Recaps"
      // depending on the tenant, so its id is tried before its name. In a
      // probe the opening view is recorded first. ----
      const RECAP_APP_ID = '981bbfd0-5eb8-4393-8158-bfff69d2d6b5';
      const buttons = () => all('[role="tab"],[role="button"],[role="menuitem"],[role="menuitemradio"],[role="option"],button,a').filter(visible);
      const named = (el, pattern) => pattern.test(label(el)) || pattern.test(text(el));
      const isSelected = el => el.getAttribute('aria-selected') === 'true' ||
        el.getAttribute('aria-current') === 'page' || el.getAttribute('aria-pressed') === 'true';
      const railName = /^(meet|recaps?|meeting recaps?)(?:\s*[,(].*)?$/i;
      const tabName = /^(recaps?|meeting recaps?|past|past meetings|recent)(?:\s*[,(].*)?$/i;
      const moreApps = /^(view more apps|more added apps|more apps)(?:\s*[,(].*)?$/i;
      const openingRecorded = !PROBE || state.stages.length > 0 || Date.now() - state.firstSeen > 20000;
      if (!state.clicks.railApp && openingRecorded) {
        const byId = document.querySelector('[data-tid="' + RECAP_APP_ID + '"]');
        const rail = byId && visible(byId) ? byId : buttons().find(el => inRail(el) && named(el, railName)) || buttons().find(el => named(el, railName));
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
        // Never the rail button again: it carries the same name as the tab.
        const tab = buttons().find(el => !inRail(el) && named(el, tabName) && !isSelected(el));
        if (tab) return clickNow('recapTab', tab);
      }
      if (!settled()) return navigating();

      // A probe only watches: it leaves the window to the user, who closes it
      // when done, and reads no rows.
      if (PROBE) return { status: 'waiting', href: location.href, probe: probe() };

      const rows = findRows();
      if (!rows.length) {
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
