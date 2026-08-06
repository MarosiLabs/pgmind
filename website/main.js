/* pgmind — landing page behaviour.
   Four small things: syntax colour, the example tabs, a hairline on the
   header once you scroll, and a one-shot reveal. No dependencies. */

(() => {
  'use strict';

  /* ------------------------------------------------------------ syntax */

  const esc = (s) =>
    s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

  // Order is meaningful: strings and comments come first so that a "--"
  // inside a $md$ block, or a keyword inside a comment, is never re-scanned.
  const RULES = [
    ['string', /\$md\$[\s\S]*?\$md\$|'(?:[^']|'')*'/],
    ['comment', /--[^\n]*/],
    ['fn', /\b(?:knowledge|pgmind)\.\w+/],
    ['keyword', /\b(?:select|from|where|order\s+by|group\s+by|and|or|not|null|as|array|interval|now|limit|join|on|distinct|with|returning|is|in|values|insert|into|update|set|create\s+extension)\b/],
    ['num', /\b\d+\b/],
    ['op', /=>|::|\$\d+/],
  ];

  const SQL = new RegExp(RULES.map(([, re]) => `(${re.source})`).join('|'), 'gi');

  function paint(code) {
    let out = '';
    let last = 0;
    let m;
    SQL.lastIndex = 0;
    while ((m = SQL.exec(code)) !== null) {
      const rule = RULES.findIndex((_, i) => m[i + 1] !== undefined);
      out += esc(code.slice(last, m.index));
      out += `<span class="tok-${RULES[rule][0]}">${esc(m[0])}</span>`;
      last = m.index + m[0].length;
    }
    return out + esc(code.slice(last));
  }

  document.querySelectorAll('pre code.sql').forEach((el) => {
    el.innerHTML = paint(el.textContent);
  });

  document.querySelectorAll('pre.shell code').forEach((el) => {
    el.innerHTML = esc(el.textContent).replace(
      /#[^\n]*/g,
      (c) => `<span class="tok-comment">${c}</span>`
    );
  });

  /* -------------------------------------------------------------- tabs */

  const tabs = Array.from(document.querySelectorAll('[role="tab"]'));

  function select(tab, focus) {
    tabs.forEach((t) => {
      const on = t === tab;
      const panel = document.getElementById(t.getAttribute('aria-controls'));
      t.setAttribute('aria-selected', String(on));
      t.tabIndex = on ? 0 : -1;
      panel.hidden = !on;
      // Toggling [hidden] does not restart a CSS animation; drop the class,
      // force a reflow, put it back.
      panel.classList.remove('enter');
      if (on) {
        void panel.offsetWidth;
        panel.classList.add('enter');
      }
    });
    if (focus) tab.focus();
  }

  tabs.forEach((tab, i) => {
    tab.addEventListener('click', () => select(tab, false));
    tab.addEventListener('keydown', (e) => {
      const step = { ArrowRight: 1, ArrowLeft: -1 }[e.key];
      let next = null;
      if (step) next = tabs[(i + step + tabs.length) % tabs.length];
      else if (e.key === 'Home') next = tabs[0];
      else if (e.key === 'End') next = tabs[tabs.length - 1];
      if (!next) return;
      e.preventDefault();
      select(next, true);
    });
  });

  /* ------------------------------------------------------------ header */

  const head = document.querySelector('.site-head');
  const onScroll = () => head.classList.toggle('stuck', window.scrollY > 8);
  addEventListener('scroll', onScroll, { passive: true });
  onScroll();

  /* ------------------------------------------------------------ reveal */

  const targets = document.querySelectorAll('.reveal');
  const still = matchMedia('(prefers-reduced-motion: reduce)').matches;

  if (still || !('IntersectionObserver' in window)) {
    targets.forEach((el) => el.classList.add('in'));
  } else {
    const io = new IntersectionObserver(
      (entries) => {
        entries.forEach((entry) => {
          if (!entry.isIntersecting) return;
          entry.target.classList.add('in');
          io.unobserve(entry.target);
        });
      },
      { rootMargin: '0px 0px -12% 0px' }
    );
    targets.forEach((el) => io.observe(el));
  }
})();
