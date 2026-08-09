/* pgmind — the manual.
   Six things, no dependencies: syntax colour, the active sidebar entry, a
   generated table of contents with scroll-spy, heading anchors, copy
   buttons, and the reference page's filter box. */

(() => {
  'use strict';

  const esc = (s) =>
    s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

  /* ------------------------------------------------------------ syntax */

  // Order is meaningful: strings and comments are matched first so that a
  // "--" inside a $md$ block, or a keyword inside a comment, is never
  // re-scanned as something else.
  const SQL_RULES = [
    ['string', /\$md\$[\s\S]*?\$md\$|\$fn\$[\s\S]*?\$fn\$|'(?:[^']|'')*'/],
    ['comment', /--[^\n]*/],
    ['fn', /\b(?:knowledge|pgmind)\.\w+/],
    ['keyword', /\b(?:select|from|where|order\s+by|group\s+by|having|and|or|not|null|as|array|interval|now|limit|offset|join|left\s+join|lateral|on|using|distinct|with|recursive|returning|is|in|values|insert|into|update|set|delete|create|extension|table|function|index|view|materialized|begin|commit|rollback|declare|loop|exception|when|then|else|end|if|raise|language|returns|grant|revoke|to|case|union|all|exists|coalesce|count|min|max|sum|avg|show|explain|analyze|do|for|policy|security|row|level|enable|default|cast|true|false|asc|desc|between|like|ilike)\b/],
    ['num', /\b\d+(?:\.\d+)?\b/],
    ['op', /=>|::|\$\d+|\|\||->>|->/],
  ];
  const SQL = new RegExp(SQL_RULES.map(([, re]) => `(${re.source})`).join('|'), 'gi');

  function paintSql(code) {
    let out = '';
    let last = 0;
    let m;
    SQL.lastIndex = 0;
    while ((m = SQL.exec(code)) !== null) {
      const rule = SQL_RULES.findIndex((_, i) => m[i + 1] !== undefined);
      out += esc(code.slice(last, m.index));
      out += `<span class="tok-${SQL_RULES[rule][0]}">${esc(m[0])}</span>`;
      last = m.index + m[0].length;
    }
    return out + esc(code.slice(last));
  }

  document.querySelectorAll('pre code.sql').forEach((el) => {
    el.innerHTML = paintSql(el.textContent);
  });

  document.querySelectorAll('pre code.shell').forEach((el) => {
    el.innerHTML = esc(el.textContent).replace(
      /#[^\n]*/g,
      (c) => `<span class="tok-comment">${c}</span>`
    );
  });

  // psql output: dim the rule/count furniture, redden real errors so a
  // deliberate failure reads as one at a glance.
  document.querySelectorAll('pre code.out').forEach((el) => {
    el.innerHTML = esc(el.textContent)
      .replace(/^(ERROR|DETAIL|CONTEXT|WARNING|NOTICE):/gm,
        (c) => `<span class="tok-err">${c}</span>`)
      .replace(/^\(\d+ rows?\)$/gm, (c) => `<span class="tok-comment">${c}</span>`);
  });

  /* ----------------------------------------------------------- sidebar */

  const here = location.pathname.replace(/^.*\//, '') || 'index.html';
  document.querySelectorAll('.docs-side a').forEach((a) => {
    const target = a.getAttribute('href').replace(/^.*\//, '').split('#')[0];
    if (target === here) a.setAttribute('aria-current', 'page');
  });

  const side = document.getElementById('side');
  const toggle = document.querySelector('.side-toggle');
  if (side && toggle) {
    let scrim = null;
    const close = () => {
      side.classList.remove('open');
      toggle.setAttribute('aria-expanded', 'false');
      if (scrim) { scrim.remove(); scrim = null; }
    };
    toggle.addEventListener('click', () => {
      const open = side.classList.toggle('open');
      toggle.setAttribute('aria-expanded', String(open));
      if (open) {
        scrim = document.createElement('button');
        scrim.className = 'side-scrim';
        scrim.setAttribute('aria-label', 'Close navigation');
        scrim.addEventListener('click', close);
        document.body.appendChild(scrim);
      } else {
        close();
      }
    });
    side.addEventListener('click', (e) => { if (e.target.closest('a')) close(); });
    addEventListener('keydown', (e) => { if (e.key === 'Escape') close(); });
  }

  /* --------------------------------------------------------- anchors */

  const main = document.getElementById('doc');
  const headings = main
    ? Array.from(main.querySelectorAll('h2[id], h3[id]'))
    : [];

  headings.forEach((h) => {
    const a = document.createElement('a');
    a.className = 'anchor';
    a.href = `#${h.id}`;
    a.textContent = '#';
    a.setAttribute('aria-label', `Link to ${h.textContent.trim()}`);
    h.appendChild(a);
  });

  /* ------------------------------------------------------------- toc */

  const toc = document.getElementById('toc');
  if (toc && headings.length) {
    // Clone and strip, rather than reading firstChild: a heading like
    // "The <code>markdown</code> type" has a text node first, so firstChild
    // would label the entry "The". Dropping .anchor keeps the trailing "#"
    // out, and a badge is furniture rather than part of the title.
    const label = (h) => {
      const c = h.cloneNode(true);
      c.querySelectorAll('.anchor, .badge').forEach((n) => n.remove());
      return c.textContent.replace(/\s+/g, ' ').trim() || h.id;
    };

    // Only h2s are listed outright. Their h3s hang underneath and stay hidden
    // until the reader is inside that section — the SQL reference has 45 of
    // them, and a flat list of 45 is a wall, not a map.
    const ul = document.createElement('ul');
    let sub = null;
    headings.forEach((h) => {
      const li = document.createElement('li');
      const a = document.createElement('a');
      a.href = `#${h.id}`;
      a.textContent = label(h);
      li.appendChild(a);
      if (h.tagName === 'H2') {
        ul.appendChild(li);
        sub = document.createElement('ul');
        sub.className = 'toc-sub';
        ul.appendChild(sub);
        li.dataset.section = h.id;
      } else if (sub) {
        a.className = 'l3';
        sub.appendChild(li);
      }
    });

    const cap = document.createElement('p');
    cap.textContent = 'On this page';
    toc.appendChild(cap);
    toc.appendChild(ul);

    const links = new Map(
      Array.from(toc.querySelectorAll('a')).map((a) => [a.getAttribute('href').slice(1), a])
    );
    const subs = Array.from(toc.querySelectorAll('.toc-sub'));
    let current = null;

    const spy = () => {
      // The heading nearest above the top of the viewport wins; falling back
      // to the first one keeps something lit before the reader scrolls.
      const line = window.scrollY + 140;
      let found = headings[0];
      for (const h of headings) {
        if (h.getBoundingClientRect().top + window.scrollY <= line) found = h;
        else break;
      }
      if (!found || found === current) return;
      if (current) links.get(current.id)?.classList.remove('on');
      links.get(found.id)?.classList.add('on');
      current = found;

      // Reveal the h3 group belonging to whichever h2 we are under.
      const section = found.tagName === 'H2'
        ? found
        : headings.slice(0, headings.indexOf(found)).reverse()
            .find((h) => h.tagName === 'H2');
      const owner = section ? links.get(section.id)?.parentElement : null;
      subs.forEach((s) => s.classList.toggle('open', owner && s.previousSibling === owner));
      if (found.tagName === 'H3') links.get(section?.id)?.classList.add('in');
      toc.querySelectorAll('a.in').forEach((a) => {
        if (a !== links.get(section?.id)) a.classList.remove('in');
      });
    };

    let ticking = false;
    addEventListener('scroll', () => {
      if (ticking) return;
      ticking = true;
      requestAnimationFrame(() => { spy(); ticking = false; });
    }, { passive: true });
    spy();
  }

  /* ------------------------------------------------------------ copy */

  document.querySelectorAll('.code').forEach((block) => {
    // Only the input is worth copying; a captured result set is not.
    const source = block.querySelector('pre:not(.out) code');
    if (!source) return;
    const btn = document.createElement('button');
    btn.className = 'copy';
    btn.type = 'button';
    btn.setAttribute('aria-label', 'Copy to clipboard');
    btn.innerHTML = '<svg width="14" height="14" aria-hidden="true"><use href="#copy"/></svg>';
    btn.addEventListener('click', async () => {
      try {
        await navigator.clipboard.writeText(source.textContent);
      } catch {
        return; // no clipboard permission: leave the button alone rather than lie
      }
      btn.classList.add('done');
      btn.innerHTML = '<svg width="14" height="14" aria-hidden="true"><use href="#check"/></svg>';
      setTimeout(() => {
        btn.classList.remove('done');
        btn.innerHTML = '<svg width="14" height="14" aria-hidden="true"><use href="#copy"/></svg>';
      }, 1400);
    });
    block.appendChild(btn);
  });

  /* ---------------------------------------------------------- filter */

  const filter = document.querySelector('.filter input');
  if (filter) {
    // Works for both long pages: function entries in the reference, recipe
    // entries in the cookbook. Same markup contract, one implementation.
    const entries = Array.from(document.querySelectorAll('.fn, .recipe'));
    const groups = Array.from(document.querySelectorAll('.fn-group, .recipe-group'));
    const count = document.querySelector('.filter .count');
    const total = entries.length;
    // Match against name + signature + purpose, lowercased once up front.
    const hay = new Map(entries.map((e) => [e, e.textContent.toLowerCase().slice(0, 600)]));

    const apply = () => {
      const q = filter.value.trim().toLowerCase();
      let shown = 0;
      entries.forEach((e) => {
        const hit = !q || hay.get(e).includes(q);
        e.hidden = !hit;
        if (hit) shown++;
      });
      groups.forEach((g) => {
        g.hidden = !g.querySelector('.fn:not([hidden]), .recipe:not([hidden])');
      });
      if (count) count.textContent = q ? `${shown} of ${total}` : `${total} entries`;
    };
    filter.addEventListener('input', apply);
    filter.addEventListener('keydown', (e) => {
      if (e.key === 'Escape') { filter.value = ''; apply(); }
    });
    apply();
  }
})();
