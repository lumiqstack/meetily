(() => {
      const host = location.hostname.toLowerCase();
      if (!['teams.microsoft.com', 'teams.cloud.microsoft',
            'teams.microsoft.com.mcas.ms', 'teams.cloud.microsoft.mcas.ms'].includes(host)) {
        return { status: 'waiting' };
      }
      const text = el => (el?.innerText || el?.textContent || '').replace(/\s+/g, ' ').trim();
      const tabs = [...document.querySelectorAll('[role="tab"]')];
      const tab = tabs.find(el => /^ai summary$/i.test(text(el)));
      if (!tab) {
        const choices = [...document.querySelectorAll('a,button,[role="button"]')];
        const webChoice = choices.find(el =>
          /^(continue (on|in) (this|the) browser|use the web app( instead)?|open (in|on) (the )?web( app)?|continue in browser)$/i
            .test(text(el) || el.getAttribute('aria-label') || ''));
        if (webChoice) {
          webChoice.click();
          return { status: 'opening_web' };
        }
        const page = text(document.body).slice(0, 1200);
        if (/open (your|the) teams app|join (on|in) (the )?teams app|use the web app/i.test(page)) {
          return { status: 'launcher' };
        }
        // Sign-in can restore the last chat and lose the incoming deep link.
        // Never click that chat's Recap tab: it may be a different meeting.
        const names = tabs.map(text);
        const recapSelected = tabs.some(el => /^recap$/i.test(text(el)) &&
          el.getAttribute('aria-selected') === 'true');
        if (!recapSelected && names.some(s => /^chat$/i.test(s)) &&
            names.some(s => /^(shared|files|recap)$/i.test(s))) {
          return { status: 'signed_in_chat' };
        }
        return { status: 'waiting' };
      }
      if (tab.getAttribute('aria-selected') !== 'true') {
        tab.click();
        return { status: 'waiting' };
      }

      const state = window.__meetilyRecapContent ||= {
        expanded: false, notes: [], tasks: [], last: '', stable: 0, taskScrolled: false
      };
      const visible = el => el.getClientRects().length > 0;
      const headingNodes = [...document.querySelectorAll('h1,h2,h3,h4,h5,h6,[role="heading"]')].filter(visible);
      const findHeading = pattern => headingNodes.find(el => pattern.test(text(el))) ||
        [...document.querySelectorAll('span,div,button')].filter(visible)
          .find(el => pattern.test(text(el)) &&
            ![...el.children].some(child => pattern.test(text(child))));
      const notesHeading = findHeading(/^meeting notes$/i);
      const tasksHeading = findHeading(/^follow[\s\u2010-\u2015-]*up tasks$/i);
      if (!notesHeading && !tasksHeading) return { status: 'waiting' };
      const expand = [...document.querySelectorAll('button')]
        .find(el => visible(el) && /^expand all$/i.test(text(el)));
      if (expand && !state.expanded) {
        state.expanded = true;
        expand.click();
        return { status: 'waiting' };
      }
      const follows = (first, second) =>
        !!(first.compareDocumentPosition(second) & Node.DOCUMENT_POSITION_FOLLOWING);
      const controls = /^(copy|copy notes|copy link|edit|more options|expand|collapse|assign|unassigned|are these (?:tasks|notes) useful\??)$/i;
      const timestamp = /^\d{1,2}:\d{2}(?::\d{2})?$/;
      const inline = node => {
        if (node.nodeType === Node.TEXT_NODE)
          return node.textContent.replace(/([\\*_\[\]])/g, '\\$1');
        if (node.nodeType !== Node.ELEMENT_NODE) return '';
        if (node.matches('script,style,svg,input,[aria-hidden="true"]') ||
            (node.matches('button,a,[role="button"],sup,time') &&
              (controls.test(text(node)) || timestamp.test(text(node)) || /^\d+$/.test(text(node))))) return '';
        if (node.tagName === 'BR') return '\n';
        const value = [...node.childNodes].map(inline).join('');
        if (node.matches('strong,b')) return value.trim() ? '**' + value.trim() + '**' : '';
        if (node.matches('p,div')) return '\n' + value + '\n';
        return value;
      };
      const rowMarkdown = (row, tasks) => {
        const raw = inline(row).replace(/[ \t]+/g, ' ').trim();
        const child = /^\s*[.\u2022\u25e6]\s+/.test(raw) ||
          Number(row.getAttribute('aria-level') || 1) > 1;
        let value = raw.replace(/^\s*(?:[.\u2022\u25e6-]\s+)+/, '').trim();
        // Playback timestamps can also be plain sibling text after the note.
        // Keep times used inside actual sentences (for example "meet at 10:30").
        value = value.split('\n').filter(line => !timestamp.test(line.trim())).join('\n').trim();
        if (!value || controls.test(value) || /^[\d\s.\u2022\u25e6-]+$/.test(value)) return '';
        value = value.replace(/(\*\*[^*\n]+:\*\*)(?=\S)/g, '$1 ');
        // Some Teams rows render the topic in a styled span rather than <strong>.
        if (!value.startsWith('**')) {
          const subject = value.match(/^([^:\n]{1,160}):(?:\s+|(?=[^\d]))/);
          if (subject) value = '**' + subject[1].trim() + ':** ' + value.slice(subject[0].length);
        }
        const indent = !tasks && child ? '  ' : '';
        value = value.replace(/\n+/g, '\n' + indent + '  ');
        return indent + (tasks ? '- [ ] ' : '- ') + value;
      };
      const level = el => Number(el.getAttribute('aria-level') || el.tagName.match(/^H([1-6])$/)?.[1] || 2);
      const boundaryAfter = heading => headingNodes.find(el =>
        follows(heading, el) && level(el) <= level(heading));
      const sectionRows = (heading, before, tasks) => {
        if (!heading) return { rows: [], plain: '' };
        const region = heading.closest('section,[role="region"],[role="tabpanel"]');
        const inside = el => visible(el) && (!region || region.contains(el)) &&
          follows(heading, el) && (!before || follows(el, before));
        let candidates = [...document.querySelectorAll('[role="row"],[role="listitem"],li,p')]
          .filter(inside);
        // A row may contain paragraphs/list items: emit the outer row only.
        candidates = candidates.filter(el => !candidates.some(parent => parent !== el && parent.contains(el)));
        if (!candidates.length) {
          // Task cards need not expose ARIA rows. Read leaf blocks only inside
          // the explicit tasks section, never the surrounding Teams chat.
          candidates = [...document.querySelectorAll('div')].filter(inside)
            .filter(el => text(el) && !el.querySelector('div,p,li,[role="row"],[role="listitem"]'));
        }
        const range = document.createRange();
        range.setStartAfter(heading);
        if (before) range.setEndBefore(before);
        else {
          range.setEndAfter(region || document.body.lastElementChild);
        }
        return { rows: candidates.map(el => rowMarkdown(el, tasks)).filter(Boolean),
          plain: range.cloneContents().textContent || '' };
      };
      const notes = sectionRows(notesHeading, tasksHeading || (notesHeading && boundaryAfter(notesHeading)), false);
      const tasks = sectionRows(tasksHeading, tasksHeading && boundaryAfter(tasksHeading), true);
      const emptyTasks = /^(?:no (?:follow[\s\u2010-\u2015-]*up )?tasks(?: were)?(?: identified| found| generated)?|couldn't find any (?:follow-up )?tasks)[.!]?$/i.test(tasks.plain.trim());
      for (const row of notes.rows) if (!state.notes.includes(row)) state.notes.push(row);
      if (!emptyTasks) for (const row of tasks.rows) if (!state.tasks.includes(row)) state.tasks.push(row);

      // Lazy sections may be below the viewport. Sweep the containing pane,
      // keeping already-read rows if Teams virtualizes them while scrolling.
      let pane = (notesHeading || tasksHeading)?.parentElement;
      while (pane && pane !== document.body) {
        if (pane.scrollHeight > pane.clientHeight + 20 &&
            /auto|scroll/.test(getComputedStyle(pane).overflowY)) {
          const old = pane.scrollTop;
          pane.scrollTop += Math.max(200, pane.clientHeight * 0.75);
          if (pane.scrollTop > old) return { status: 'collecting_recap' };
          break;
        }
        pane = pane.parentElement;
      }
      if (tasksHeading && !state.taskScrolled) {
        state.taskScrolled = true;
        tasksHeading.scrollIntoView({ block: 'nearest' });
        return { status: 'collecting_recap' };
      }
      if (!tasksHeading || (!state.tasks.length && !emptyTasks)) {
        return { status: 'waiting_tasks' };
      }
      if (!state.notes.length && !state.tasks.length) return { status: 'waiting' };
      const snapshot = JSON.stringify([state.notes, state.tasks]);
      state.stable = snapshot === state.last ? state.stable + 1 : 0;
      state.last = snapshot;
      if (state.stable < 2) return { status: 'collecting_recap' };
      return { status: 'ready', notes: state.notes, tasks: state.tasks };
    })()
