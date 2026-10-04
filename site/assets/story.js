/* pando · 01–03, the story: a grove the page scrolls over.
 *
 * The stage is sticky; the beats scroll past it. Scroll position becomes a
 * story time t (beat i sits at t = i), and every frame draws the grove as a
 * function of t, plus the clock for what is alive anyway: leaves, particles
 * on the root, the dust. Scrolling back plays the story backwards.
 */
(function () {
  'use strict';
  const P = window.PANDO;
  const { clamp, lerp, ss, E, prog } = P.m;
  const C = P.C;
  // 9903 → "9,903"; toLocaleString would load locale data first, a good part
  // of the page's first script on a phone
  const commas = n => String(n).replace(/\B(?=(\d{3})+(?!\d))/g, ',');

  const story = document.querySelector('.story');
  if (!story) return;
  const canvas = story.querySelector('.stage canvas');
  const beats = Array.from(story.querySelectorAll('.beat'));
  const copyEl = story.querySelector('.beat-copy');
  const hero = story.querySelector('.hero');
  const big = story.querySelector('.beat-big');
  const hint = story.querySelector('.scroll-hint');
  const seedHud = story.querySelector('.seed-hud');
  const co = {
    err: story.querySelector('.co.err'), port: story.querySelector('.co.port'),
    stash: story.querySelector('.co.stash'), pop: story.querySelector('.co.pop'), chat: story.querySelector('.co.ask'),
  };

  const grove = new P.Grove({ ghosts: 20 });
  const view = new P.View(canvas, { maxDpr: 2 });
  view.trackPointer(story.querySelector('.stage'));
  const dust = new P.Dust(160, 33);
  const X = grove.X, G = grove.G;
  const BR = ['main', 'feat/checkout', 'fix/login-loop', 'pr-128', 'feat/search', 'chore/deps'];
  if (seedHud) seedHud.textContent = `seed 0x5eed · ${commas(grove.segments)} segments`;

  /* ---------- story time from scroll ---------- */
  // Everything a frame needs from layout is measured here, not in the frame:
  // a layout read after the frame's own style writes would lay the page out
  // again, every frame.
  let tops = [], storyBottom = 0, sizes = new Map(), stillKey = '';
  function measure() {
    const y0 = window.scrollY;
    tops = beats.map(b => b.getBoundingClientRect().top + y0);
    tops.push(tops[tops.length - 1] + beats[beats.length - 1].offsetHeight);
    storyBottom = story.getBoundingClientRect().bottom + y0;
    sizes = new Map();
    stillKey = '';
  }
  // a style is written only when it changes
  const css = (el, k, v) => { const s = el._css || (el._css = {}); v = String(v); if (s[k] !== v) { s[k] = v; el.style[k] = v; } };
  function storyT() {
    const s = window.scrollY;
    if (s <= tops[0]) return 0;
    for (let i = 0; i < beats.length; i++) if (s < tops[i + 1]) return i + (s - tops[i]) / (tops[i + 1] - tops[i]);
    return beats.length;
  }

  /* ---------- the timeline, in beats ---------- */
  const T_SCAN0 = 1.3, T_SCAN1 = 1.95;
  const T_COUNT = 2.35;
  const T_NAME = 4.55;        // labels decode onto the trunks
  const T_WAVE = 5.4;         // the gold wave along the root: your repository
  const T_UNGROW = 6.3;       // the grove collapses to one trunk
  const FLIPS = [6.75, 7.2, 7.65], FV = [1, 2, 0];
  const T_BURST = 8.5;        // so I built pando
  const T_GREEN = 9.25;       // every branch, alive

  let loadT0 = null; // when the grove started growing on load
  const growDelay = [0.25, 0.05, 0.4, 0.15, 0.5, 0.3];

  function loadGrow(now, i) {
    if (P.reduced) return 1;
    if (loadT0 == null) return 0;
    return prog(now - loadT0, growDelay[i], 2.2, E.ioSine);
  }
  function ghostLoad(now, g) {
    if (P.reduced) return 1;
    if (loadT0 == null) return 0;
    return prog(now - loadT0, g.gs * 0.9, 2.2, E.ioSine);
  }

  function lone(t) {
    let k = -1; for (let j = 0; j < FLIPS.length; j++) if (t >= FLIPS[j]) k = j;
    const v = k < 0 ? 0 : FV[k];
    let F = 1;
    if (k >= 0) F = 0.45 + 0.55 * prog(t, FLIPS[k], 0.14, E.out);
    const nf = FLIPS[k + 1];
    if (nf !== undefined && t > nf - 0.06) F *= 1 - 0.55 * prog(t, nf - 0.06, 0.06, E.in);
    F *= 1 - prog(t, T_BURST - 0.1, 0.08, E.in);
    return { v, k, F, x: lerp(X[0], 960, prog(t, T_UNGROW + 0.1, 0.5, E.io)) };
  }
  const greenAt = (x, t) => {
    const u = (x - viewSpan[0]) / (viewSpan[1] - viewSpan[0]);
    return ss(T_GREEN + u * 0.55 + 0.05, T_GREEN + u * 0.55 + 0.18, t);
  };
  let viewSpan = [0, 1920];

  function trunkState(i, t, now) {
    let F = loadGrow(now, i);
    if (t > T_UNGROW) F *= 1 - prog(t, T_UNGROW + [0, 0.08, 0.04, 0.12, 0.02, 0.1][i], 0.35, E.io);
    if (t >= T_BURST) F = prog(t, T_BURST + Math.abs(X[i] - 960) / 5000, 0.35, E.out);
    return { x: X[i], y: G, s: 1, F, alpha: 1, green: greenAt(X[i], t) };
  }
  function ghostF(g, k, t, now) {
    let F = ghostLoad(now, g);
    if (t > T_UNGROW) F *= 1 - prog(t, T_UNGROW + (k % 5) * 0.03, 0.35, E.io);
    if (t >= T_BURST) F = prog(t, T_BURST + Math.abs(g.x - 960) / 5000, 0.35, E.out);
    return F;
  }
  function ghostAlpha(t) {
    let a = lerp(0.55, 0.3, ss(4.2, 4.6, t));
    a = lerp(a, 0.3, ss(T_BURST, T_BURST + 0.3, t));
    return a;
  }

  /* ---------- the camera ---------- */
  function camera(t) {
    const w = view.w, h = view.h, portrait = h > w * 1.05;
    const base = Math.max(Math.min(w / 1920, h / 1080), portrait ? w / 1250 : 0);
    // the hero: small grove at the bottom, the name in the sky
    const zH = base * (portrait ? 0.95 : 0.62), gyH = portrait ? 0.9 : 0.975;
    // the story: the grove in the lower half, room for the headline above
    const zS = base * (portrait ? 1.05 : 0.9), gyS = portrait ? 0.72 : h < 820 ? 0.79 : 0.74;
    const k = prog(t, 0.15, 0.8, E.io);
    let z = lerp(zH, zS, k), gy = lerp(gyH, gyS, k);
    // the problem: push in on the one trunk
    const push = ss(T_UNGROW + 0.2, T_UNGROW + 0.8, t) * (1 - ss(T_BURST - 0.05, T_BURST + 0.3, t));
    z *= 1 + 0.12 * push;
    // every branch alive: pull back a little
    z *= 1 - 0.1 * ss(T_GREEN, T_GREEN + 0.6, t);
    const drift = P.reduced ? 0 : 1;
    return { x: 960, y: G - (gy - 0.5) * h / z, z, drift };
  }

  /* ---------- the headline: typed in when its beat arrives ---------- */
  const headEl = copyEl.querySelector('.headline .txt');
  const subEl = copyEl.querySelector('.sub');
  const bodyEl = copyEl.querySelector('.beat-body');
  let shown = -1, typing = null;
  const beatCache = [];
  function beatData(i) {
    return beatCache[i] || (beatCache[i] = readBeat(i));
  }
  function readBeat(i) {
    const b = beats[i];
    const ps = Array.from(b.querySelectorAll('.beat-text p')).filter(p => !p.classList.contains('nojs'));
    return { head: b.dataset.head || '', hi: b.dataset.hi || '', sub: b.dataset.sub || '', body: ps.map(p => p.outerHTML).join(''), chap: b.dataset.chap, name: b.dataset.name, tag: b.dataset.tag, big: b.dataset.big };
  }
  function renderHead(s, hi) {
    if (s.indexOf('{count}') >= 0) return escapeHtml(s).replace('{count}', '<span class="count">000,000</span>');
    if (!hi) return escapeHtml(s);
    const j = s.indexOf(hi);
    if (j < 0) return escapeHtml(s);
    return escapeHtml(s.slice(0, j)) + '<span class="hi">' + escapeHtml(s.slice(j, j + hi.length)) + '</span>' + escapeHtml(s.slice(j + hi.length));
  }
  function escapeHtml(s) { return s.replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c])); }
  function showBeat(i) {
    // the marker follows the story every frame, so it is right after scrolling back up into it
    const bd = beats[i].dataset;
    if (P.hud && storyBottom - window.scrollY > innerHeight * 0.3) P.hud.set(bd.chap, bd.name, bd.tag);
    if (i === shown) return;
    shown = i;
    const d = beatData(i);
    copyEl.classList.toggle('empty', !d.head);
    css(copyEl, 'opacity', d.head ? 1 : 0);
    subEl.textContent = d.sub;
    css(subEl, 'display', d.sub ? '' : 'none');
    bodyEl.innerHTML = d.body;
    if (typing) cancelAnimationFrame(typing);
    const final = d.head, full = final.replace('{count}', '000,000');
    if (P.reduced || !full) { headEl.innerHTML = renderHead(final, d.hi); return; }
    const t0 = performance.now(), cps = 48, GL = 'abcdefghijklmnopqrstuvwxyz0123456789/-_#%$&*+=<>{}';
    const step = () => {
      const e = (performance.now() - t0) / 1000, n = Math.floor(e * cps);
      if (n >= full.length) { headEl.innerHTML = renderHead(final, d.hi); typing = null; return; }
      let s = full.slice(0, n);
      for (let j = n; j < Math.min(full.length, n + 3); j++) s += full[j] === ' ' ? ' ' : GL[(Math.random() * GL.length) | 0];
      headEl.innerHTML = renderHead(s, n > full.indexOf(d.hi) + d.hi.length ? d.hi : '');
      typing = requestAnimationFrame(step);
    };
    step();
  }

  /* ---------- the counter in the facts beat ---------- */
  function counterText(t) {
    const n = Math.round(47000 * E.out(clamp((t - T_COUNT) / 0.45)));
    return commas(n).padStart(6, '0');
  }

  /* ---------- one frame ---------- */
  function frame(now) {
    if (loadT0 == null) return;
    const t = storyT();
    const bi = clamp(Math.floor(t + 0.5), 0, beats.length - 1);
    showBeat(bi);
    const cnt = copyEl.querySelector('.count');
    if (cnt) { const s = counterText(t); if (cnt.textContent !== s) cnt.textContent = s; }

    if (view.resize()) measure();
    const cam = camera(t);
    view.cam = { x: cam.x + Math.sin(now * 0.31) * 6 * cam.drift, y: cam.y + Math.sin(now * 0.23 + 1) * 3 * cam.drift, z: cam.z };
    const left = view.unproj(0, 0)[0], right = view.unproj(view.w, 0)[0];
    viewSpan = [left, right];
    view._treePointer = view.pointerScreen ? view.unproj(view.pointerScreen[0], view.pointerScreen[1]) : null;

    // hero copy and the scroll hint leave as the story starts
    const heroA = 1 - ss(0.12, 0.55, t);
    css(hero, 'opacity', heroA);
    css(hero, 'transform', `translateY(${-40 * ss(0.12, 0.7, t)}px)`);
    css(hero, 'visibility', heroA <= 0.01 ? 'hidden' : 'visible');
    if (hint) css(hint, 'opacity', 1 - ss(0.02, 0.25, t));
    if (seedHud) css(seedHud, 'opacity', 1 - ss(beats.length - 1.4, beats.length - 1.1, t));
    css(copyEl, 'opacity', bi === 0 || beatData(bi).big ? 0 : ss(0.55, 0.85, t));
    big.classList.toggle('on', !!beatData(bi).big);

    view.clear();
    dust.draw(view, now, 0.8);
    view.world();

    // the ground, and the soil the scan dissolves
    grove.drawGround(view, 1);
    const scanY = G + 440 * ss(T_SCAN0, T_SCAN1, t);
    grove.drawSoil(view, scanY, 1 - ss(T_SCAN1, T_SCAN1 + 0.2, t) * 0.0);

    // trunk states
    const L = t >= T_UNGROW && t < T_BURST ? lone(t) : null;
    const st = X.map((x, i) => trunkState(i, t, now));
    if (L) { st[0].x = L.x; st[0].F = Math.max(L.F * loadGrow(now, 0), 0); }

    // the one root, and every trunk's connector to it
    const conns = [];
    st.forEach((s, i) => conns.push({ x: s.x, y: G, cdx: grove.trees[i].cdx, pres: clamp(s.F * 3), green: s.green, flare: 1.4 * ss(T_COUNT, T_COUNT + 0.12, t) * (1 - ss(T_COUNT + 0.5, T_COUNT + 0.9, t)) }));
    grove.ghosts.forEach((g, k) => conns.push({ x: g.x, y: g.y, cdx: g.tr.cdx, pres: clamp(ghostF(g, k, t, now) * 3) * 0.5 }));
    const glow = 1 + 0.9 * ss(T_COUNT, T_COUNT + 0.1, t) * (1 - ss(T_COUNT + 0.5, T_COUNT + 0.9, t))
      + 0.9 * ss(T_WAVE, T_WAVE + 0.3, t) * (1 - ss(T_UNGROW - 0.2, T_UNGROW + 0.1, t))
      + 0.9 * P.m.kick(t, T_BURST, 6) + 1.1 * ss(T_GREEN + 0.2, T_GREEN + 0.8, t);
    const rootA = t < T_SCAN0 ? 0 : 1 - 0.45 * ss(T_UNGROW, T_UNGROW + 0.4, t) * (1 - ss(T_BURST - 0.05, T_BURST + 0.1, t));
    grove.drawRoots(view, { reveal: scanY, alpha: rootA, glow, trunks: conns, particles: ss(T_SCAN1 - 0.1, T_SCAN1 + 0.3, t) }, now);
    grove.drawWave(view, (t - (T_SCAN1 - 0.1)) / 0.7, '246,210,122', left - 200, right + 300);
    grove.drawWave(view, (t - T_WAVE) / 0.7, '252,228,165', left - 200, right + 300);
    grove.drawWave(view, (t - T_GREEN) / 0.6, '124,255,178', left - 200, right + 300);

    // the grove
    const perGhost = (t > T_UNGROW - 0.01 && t < T_BURST + 0.5) || (now - loadT0 < 3.2 && !P.reduced)
      ? (g, k) => ghostF(g, k, t, now) : null;
    grove.drawGhosts(view, 1, ghostAlpha(t) * (1 - 0.55 * ss(3.4, 3.7, t) * (1 - ss(4.2, 4.5, t))), now, perGhost);
    const dimAll = 1 - 0.72 * ss(3.4, 3.7, t) * (1 - ss(4.2, 4.5, t));
    st.forEach((s, i) => {
      if (L && i === 0) {
        P.drawTree(view, grove.trees[0], { x: s.x, y: G, s: 1, F: s.F, alpha: dimAll, green: 0 }, now);
        return;
      }
      P.drawTree(view, grove.trees[i], { x: s.x, y: G, s: 1, F: s.F, alpha: dimAll, green: s.green }, now);
      // named: a band of gold climbs the trunk
      const front = (t - (T_NAME + i * 0.05)) * 3.2;
      P.drawLit(view, grove.trees[i], { x: s.x, y: G, s: 1, F: s.F }, front, '242,184,75');
    });

    // the burst, when everything comes back
    grove.drawBurst(view, 960, G, (t - T_BURST) * 2.2);

    // screen space: labels, the bracket, the callouts
    view.screen();
    drawLabels(t, now, st, L);
    drawBracket(t, now);
    placeCallouts(t, L);
  }

  function labelSize() { return Math.round(clamp(view.cam.z * 26, 12, 28)); }
  function drawLabels(t, now, st, L) {
    const size = labelSize();
    for (let i = 0; i < 6; i++) {
      let text = null, k = 1, alpha = 1, green = st[i].green, gold = false, strike = 0, row = i % 2;
      if (L && i === 0) {
        text = L.k < 0 ? BR[0] : BR[L.v];
        k = L.k < 0 ? 1 : clamp((t - FLIPS[L.k]) / 0.1);
        const nf = FLIPS[L.k + 1];
        if (nf !== undefined) strike = prog(t, nf - 0.12, 0.08, E.out);
        alpha = 1 - ss(T_BURST - 0.15, T_BURST - 0.05, t);
        row = 0;
        const [sx, sy] = view.proj(L.x, G);
        const r = P.drawLabel(view, sx, sy, P.decode(text, k, now, 3), { size: Math.round(size * 1.2), row, alpha, strike });
        if (r) drawHistory(t, L, r, size, alpha);
        continue;
      }
      if (t >= T_NAME && t < T_UNGROW + 0.15) {
        text = BR[i]; k = clamp((t - (T_NAME + i * 0.05)) / 0.18);
        alpha = (t < T_NAME + i * 0.05 ? 0 : 1) * (1 - ss(T_UNGROW - 0.05, T_UNGROW + 0.15, t));
        gold = t - (T_NAME + i * 0.05) < 0.35;
      } else if (t >= T_BURST + 0.15) {
        text = i === 3 ? 'pr-128' : BR[i];
        k = clamp((t - (T_BURST + 0.12 + i * 0.03)) / 0.14);
        alpha = t < T_BURST + 0.12 + i * 0.03 ? 0 : 1;
      }
      if (!text || alpha <= 0) continue;
      const [sx, sy] = view.proj(st[i].x, G);
      if (sx < -200 || sx > view.w + 200) continue;
      P.drawLabel(view, sx, sy, P.decode(text, k, now, i), { size, row, alpha, green, gold: gold && k < 1.2 && green < 0.5 });
    }
  }
  // the names this trunk had before, struck through, piling up beneath it
  function drawHistory(t, L, r, size, alpha) {
    const ctx = view.ctx, seq = ['main', ...FV.slice(0, L.k + 1).map(v => BR[v])], hist = seq.slice(0, -1).reverse();
    const hs = Math.round(size * 0.95), gap = hs * 1.55;
    ctx.save(); ctx.font = `500 ${hs}px ${P.MONO}`; ctx.textAlign = 'center';
    hist.forEach((name, j) => {
      const fresh = j === 0 ? prog(t, FLIPS[L.k], 0.1, E.out) : 1;
      const y = lerp(r.ly, r.ly + gap * (j + 1) + 6, fresh);
      ctx.globalAlpha = alpha * (0.62 - j * 0.14);
      ctx.fillStyle = 'rgba(236,236,236,.8)'; ctx.fillText(name, r.lx, y);
      const ww = ctx.measureText(name).width;
      ctx.strokeStyle = C.orange; ctx.lineWidth = 2.5;
      ctx.beginPath(); ctx.moveTo(r.lx - ww / 2 - 4, y - hs * 0.32); ctx.lineTo(r.lx + ww / 2 + 4, y - hs * 0.32); ctx.stroke();
    });
    ctx.restore();
  }
  // "your repository": a bracket under the root
  function drawBracket(t, now) {
    const a = ss(T_WAVE + 0.15, T_WAVE + 0.35, t) * (1 - ss(T_UNGROW - 0.15, T_UNGROW + 0.05, t));
    if (a <= 0) return;
    const ctx = view.ctx, y = view.proj(0, G + 225)[1], x0 = Math.max(24, view.w * 0.05), x1 = view.w - x0;
    const size = Math.round(clamp(view.cam.z * 36, 18, 38));
    const s = P.decode('your repository', clamp((t - T_WAVE - 0.15) / 0.3), now, 11);
    ctx.save(); ctx.globalAlpha = a;
    ctx.font = `700 ${size}px ${P.MONO}`; const tw = ctx.measureText(s).width, mid = view.w / 2;
    ctx.strokeStyle = 'rgba(242,184,75,.8)'; ctx.lineWidth = 2;
    ctx.beginPath(); ctx.moveTo(x0, y - 16); ctx.lineTo(x0, y); ctx.lineTo(mid - tw / 2 - 24, y); ctx.moveTo(mid + tw / 2 + 24, y); ctx.lineTo(x1, y); ctx.lineTo(x1, y - 16); ctx.stroke();
    ctx.shadowColor = 'rgba(242,184,75,.6)'; ctx.shadowBlur = 14 * view.dpr; ctx.fillStyle = C.gold; ctx.textAlign = 'center';
    ctx.fillText(s, mid, y + size * 0.36);
    ctx.restore();
  }

  // the pains, around the one trunk
  function placeCallouts(t, L) {
    const on = (el, a, x, y) => {
      if (!el) return;
      css(el, 'opacity', a);
      css(el, 'visibility', a <= 0.01 ? 'hidden' : 'visible');
      if (a > 0.01) css(el, 'transform', `translate(${Math.round(x)}px, ${Math.round(y)}px)`);
    };
    const out = 1 - ss(T_BURST - 0.2, T_BURST - 0.05, t);
    if (!L || out <= 0) { Object.values(co).forEach(el => on(el, 0, 0, 0)); return; }
    const [sx, gy] = view.proj(L.x, G);
    const topY = view.proj(L.x, G - 400)[1];
    const narrow = view.w < 760;
    const size = el => { let s = sizes.get(el); if (!s) sizes.set(el, (s = [el.offsetWidth, el.offsetHeight])); return s; };
    const W_ = el => size(el)[0], H_ = el => size(el)[1];
    // EADDRINUSE: left of the trunk, at its middle
    let x = narrow ? 12 : sx - W_(co.err) - Math.max(60, view.w * 0.06);
    let y = narrow ? topY - H_(co.err) - 10 : lerp(topY, gy, 0.55);
    on(co.err, ss(6.8, 6.88, t) * out, Math.max(12, x), y);
    on(co.port, ss(6.8, 6.88, t) * out, narrow ? sx - W_(co.port) - 14 : sx + 18, gy - H_(co.port) - 10);
    // git stash / pop: right of the trunk
    x = narrow ? view.w - W_(co.pop) - 12 : sx + Math.max(60, view.w * 0.06);
    y = lerp(topY, gy, 0.55);
    on(co.stash, ss(6.92, 7.0, t) * out, x, y);
    on(co.pop, ss(7.45, 7.53, t) * out, x, y + H_(co.stash) * 1.25);
    // the teammate: top right
    x = narrow ? view.w - W_(co.chat) - 12 : sx + Math.max(80, view.w * 0.08);
    y = narrow ? gy + 120 : topY + 10;
    on(co.chat, ss(7.78, 7.88, t) * out, Math.min(x, view.w - W_(co.chat) - 12), y);
  }

  /* ---------- start ---------- */
  measure();
  window.addEventListener('resize', () => { view.resize(); measure(); });
  window.addEventListener('pando:fonts', measure);
  new ResizeObserver(measure).observe(document.body);
  // Full rate while the grove grows in; with reduced motion the clock stands
  // still, so a frame is drawn only when the scroll, the size or the pointer moved.
  const growing = () => loadT0 == null || performance.now() / 1000 - loadT0 < 3.5;
  const lp = P.loop(story.querySelector('.stage'), now => {
    if (P.reduced) {
      const k = `${window.scrollY}|${innerWidth}|${innerHeight}|${view.pointerScreen}`;
      if (k === stillKey) return;
      stillKey = k;
    }
    frame(now);
  }, { idle: true, busy: growing });
  P.fontsReady.then(() => {
    loadT0 = performance.now() / 1000;
    measure();
    lp.kick();
  });
})();
