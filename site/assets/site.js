/* pando · the page: the chapter marker, navigation, copy buttons, reveals. */
(function () {
  'use strict';
  const P = (window.PANDO = window.PANDO || {});
  document.documentElement.classList.add('js');

  /* ---------- the chapter marker ---------- */
  const hud = document.querySelector('.chap');
  const chapters = Array.from(document.querySelectorAll('[data-chapter]'));
  const bar = hud && hud.querySelector('.chap-bar');
  const nEl = hud && hud.querySelector('.chap-n'), nameEl = hud && hud.querySelector('.chap-name'), beatEl = hud && hud.querySelector('.chap-beat');
  const N = 10;
  if (bar) for (let k = 0; k < N; k++) bar.appendChild(document.createElement('i'));
  const segs = bar ? Array.from(bar.children) : [];
  // the marker is written on every scroll frame: only what changed is written
  const fill = (s, f) => { f = String(f); if (s._f !== f) { s._f = f; s.style.setProperty('--f', f); } };
  const mark = (a, v) => { if (a._cur !== v) { a._cur = v; if (v == null) a.removeAttribute('aria-current'); else a.setAttribute('aria-current', v); } };

  const GL = 'abcdefghijklmnopqrstuvwxyz0123456789/-_#%$&*+=<>{}';
  function scramble(el, text) {
    if (!el || el.dataset.v === text) return;
    el.dataset.v = text;
    if (P.reduced) { el.textContent = text; return; }
    const t0 = performance.now(), dur = 320;
    const step = () => {
      if (el.dataset.v !== text) return;
      const k = Math.min(1, (performance.now() - t0) / dur), n = Math.floor(k * text.length);
      let s = '';
      for (let j = 0; j < text.length; j++) s += j < n || text[j] === ' ' || text[j] === '/' ? text[j] : GL[(Math.random() * GL.length) | 0];
      el.textContent = s;
      if (k < 1) requestAnimationFrame(step);
    };
    step();
  }
  let current = { n: '01', name: 'Pando', beat: 'the grove' };
  P.hud = {
    set(n, name, beat) {
      if (!hud || !n) return;
      if (n === current.n && name === current.name && beat === current.beat) return;
      if (n !== current.n) scramble(nEl, n);
      if (name !== current.name) scramble(nameEl, name);
      if (beat !== current.beat) scramble(beatEl, '// ' + beat);
      current = { n, name, beat };
      const idx = parseInt(n, 10) - 1;
      segs.forEach((s, k) => fill(s, k < idx ? 1 : 0));
    },
    progress(n, f) {
      const idx = parseInt(n, 10) - 1;
      if (segs[idx]) fill(segs[idx], Math.max(0.04, Math.min(1, f)));
    },
  };

  // the section under the marker decides the chapter; within a section, the
  // last beat heading above the marker decides the beat
  const beatsIn = new Map(chapters.map(c => [c, Array.from(c.querySelectorAll('[data-beat]'))]));
  const story = document.querySelector('.story');
  function onScroll() {
    const line = window.innerHeight * 0.3;
    if (story) {
      const r = story.getBoundingClientRect();
      if (r.bottom > line) {
        // the story drives the marker itself; fill 01–03 from its progress
        const f = Math.max(0, Math.min(1, -r.top / Math.max(1, r.height - window.innerHeight)));
        segs.forEach((s, k) => fill(s, k < 3 ? Math.max(0, Math.min(1, f * 3 - k)) : 0));
        navLinks.forEach(a => mark(a, null));
        return;
      }
    }
    let active = null;
    for (const c of chapters) { if (c.getBoundingClientRect().top <= line) active = c; }
    if (!active) return;
    let beat = active.dataset.beat0 || '';
    for (const b of beatsIn.get(active)) if (b.getBoundingClientRect().top <= line) beat = b.dataset.beat;
    // read before writing: a read after the marker's writes would lay the page out again
    const r = active.getBoundingClientRect();
    P.hud.set(active.dataset.chapter, active.dataset.name, beat);
    P.hud.progress(active.dataset.chapter, (line - r.top) / Math.max(1, r.height));
    navLinks.forEach(a => mark(a, a.hash === '#' + active.id ? 'true' : 'false'));
  }
  const navLinks = Array.from(document.querySelectorAll('.nav a[href^="#"]'));
  let ticking = false;
  window.addEventListener('scroll', () => { if (!ticking) { ticking = true; requestAnimationFrame(() => { ticking = false; onScroll(); }); } }, { passive: true });
  window.addEventListener('resize', onScroll);

  /* ---------- navigation on small screens ---------- */
  const nav = document.querySelector('.nav'), toggle = document.querySelector('.nav-toggle');
  if (toggle) {
    const setOpen = (o, focusBack) => {
      nav.classList.toggle('open', o);
      toggle.setAttribute('aria-expanded', o);
      toggle.textContent = o ? 'close' : 'menu';
      if (o) { const a = nav.querySelector('a'); if (a) a.focus(); } else if (focusBack) toggle.focus();
    };
    toggle.addEventListener('click', () => setOpen(!nav.classList.contains('open'), true));
    nav.addEventListener('click', e => { if (e.target.closest('a')) setOpen(false, false); });
    document.addEventListener('keydown', e => { if (e.key === 'Escape' && nav.classList.contains('open')) setOpen(false, true); });
    document.addEventListener('click', e => { if (nav.classList.contains('open') && !nav.contains(e.target)) setOpen(false, false); });
    window.addEventListener('scroll', () => { if (nav.classList.contains('open')) setOpen(false, false); }, { passive: true });
  }

  /* ---------- a jump to an anchor lands where the anchor really is ---------- */
  // Chapters far down are left unlaid-out until they come near (site.css), so
  // their heights are guesses; before the first jump, the page is laid out whole.
  document.addEventListener('click', e => {
    const a = e.target.closest && e.target.closest('a[href^="#"]');
    if (a) document.documentElement.classList.add('laid-out');
  }, true);

  /* ---------- copy buttons ---------- */
  document.addEventListener('click', e => {
    const b = e.target.closest('[data-copy]');
    if (!b) return;
    const sel = b.dataset.copy;
    const text = sel.startsWith('#') || sel.startsWith('.') ? (document.querySelector(sel) || {}).innerText : sel;
    if (!text) return;
    const done = () => { if (b.dataset.was == null) b.dataset.was = b.textContent; b.classList.add('done'); b.textContent = 'copied'; setTimeout(() => { b.classList.remove('done'); b.textContent = b.dataset.was; }, 1400); };
    if (navigator.clipboard && window.isSecureContext) navigator.clipboard.writeText(text.trim()).then(done, () => fallback(text, done, b));
    else fallback(text, done, b);
  });
  function fallback(text, done, b) {
    const ta = document.createElement('textarea'); ta.value = text.trim(); ta.style.position = 'fixed'; ta.style.opacity = '0';
    document.body.appendChild(ta); ta.select();
    try { if (document.execCommand('copy')) done(); } catch (_) { /* nothing copied: say nothing */ }
    ta.remove();
    if (b) b.focus();
  }

  /* ---------- code that scrolls sideways can be reached by keyboard ---------- */
  // A block is measured when it comes within a screen of view, and again on a
  // resize or a font change while it is there: measuring them all at once would
  // lay out the chapters the page leaves unrendered until they come near.
  const near = new Set();
  const sideways = el => { if (el.scrollWidth > el.clientWidth + 1) el.tabIndex = 0; else el.removeAttribute('tabindex'); };
  const sio = new IntersectionObserver(es => es.forEach(e => {
    if (e.isIntersecting) { near.add(e.target); sideways(e.target); } else near.delete(e.target);
  }), { rootMargin: '100% 0px' });
  document.querySelectorAll('pre, .tbl-scroll').forEach(el => { if (!el.closest('.tui, .setup')) sio.observe(el); });
  const remeasure = () => near.forEach(sideways);
  window.addEventListener('resize', remeasure);
  window.addEventListener('pando:fonts', remeasure);
  // a block inside a closed <details> has no size until it opens
  document.addEventListener('toggle', e => {
    if (e.target.open) e.target.querySelectorAll('pre, .tbl-scroll').forEach(el => { if (!el.closest('.tui, .setup')) sideways(el); });
  }, true);

  /* ---------- reveal on scroll ---------- */
  const io = new IntersectionObserver(es => es.forEach(e => { if (e.isIntersecting) { e.target.classList.add('in'); io.unobserve(e.target); } }), { rootMargin: '0px 0px -8% 0px' });
  document.querySelectorAll('.rv').forEach(el => io.observe(el));

  /* ---------- headlines type themselves in when they arrive ---------- */
  const heads = document.querySelectorAll('.chapter .headline[data-type]');
  const hio = new IntersectionObserver(es => es.forEach(e => {
    if (!e.isIntersecting) return;
    hio.unobserve(e.target);
    const el = e.target.querySelector('.txt');
    if (!el || P.reduced) return;
    const html = el.innerHTML, text = el.textContent;
    const t0 = performance.now(), cps = 52;
    const step = () => {
      const n = Math.floor(((performance.now() - t0) / 1000) * cps);
      if (n >= text.length) { el.innerHTML = html; return; }
      let s = text.slice(0, n);
      for (let j = n; j < Math.min(text.length, n + 3); j++) s += text[j] === ' ' ? ' ' : GL[(Math.random() * GL.length) | 0];
      el.textContent = s;
      requestAnimationFrame(step);
    };
    e.target.style.minHeight = e.target.offsetHeight + 'px';
    step();
  }), { rootMargin: '0px 0px -15% 0px' });
  heads.forEach(h => hio.observe(h));

  P.reduced = P.reduced || (window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches);
  requestAnimationFrame(onScroll);
})();
