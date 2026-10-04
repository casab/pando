/* pando · the grove engine
 *
 * The trees, the one root and everything that moves along it, drawn on a
 * canvas from seeded random numbers: the same seeds grow the same grove on
 * every visit. A grove is built once (geometry), then every frame only
 * decides how much of each tree exists, what colour it is, and where the
 * camera is. Ported from the amber story film, where the whole film was a
 * pure function of time; here a section drives it from scroll, from key
 * presses, or from the clock.
 */
(function () {
  'use strict';
  const P = (window.PANDO = window.PANDO || {});

  const MONO = '"JetBrains Mono", ui-monospace, SFMono-Regular, Menlo, Consolas, monospace';
  const C = {
    gold: '#F2B84B', pale: '#FCE4A5', amber: '#D9822B', orange: '#E89A3C',
    green: '#7CFFB2', red: '#FF6B6B', bg: '#050505',
    fg: 'rgba(236,236,236,.94)', dim: 'rgba(236,236,236,.52)',
  };

  /* ---------- math ---------- */
  function mulberry(a) {
    return function () {
      a |= 0; a = (a + 0x6d2b79f5) | 0;
      let t = Math.imul(a ^ (a >>> 15), 1 | a);
      t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
      return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
  }
  function hash(a, b = 0, c = 0) {
    let h = (Math.imul(a | 0, 374761393) + Math.imul(b | 0, 668265263) + Math.imul(c | 0, 1440662683)) | 0;
    h = Math.imul(h ^ (h >>> 13), 1274126177); h ^= h >>> 16;
    return (h >>> 0) / 4294967296;
  }
  const clamp = (v, a = 0, b = 1) => (v < a ? a : v > b ? b : v);
  const lerp = (a, b, k) => a + (b - a) * k;
  const ss = (a, b, t) => { const k = clamp((t - a) / (b - a)); return k * k * (3 - 2 * k); };
  const E = {
    lin: k => k,
    io: k => (k < 0.5 ? 4 * k * k * k : 1 - Math.pow(-2 * k + 2, 3) / 2),
    out: k => 1 - Math.pow(1 - k, 3),
    in: k => k * k * k,
    outExpo: k => (k >= 1 ? 1 : 1 - Math.pow(2, -10 * k)),
    ioSine: k => -(Math.cos(Math.PI * k) - 1) / 2,
  };
  const prog = (t, a, d, e = E.io) => e(clamp((t - a) / d));
  const kick = (t, t0, k = 3) => (t > t0 ? Math.exp(-(t - t0) * k) : 0);

  /* ---------- text that decodes into place ---------- */
  const GL = 'abcdefghijklmnopqrstuvwxyz0123456789/-_#%$&*+=<>{}';
  // k: how much of the text has resolved (0..1); the rest flickers with the clock
  function decode(txt, k, now, seed = 0) {
    if (k >= 1) return txt;
    const c = Array.from(txt), n = Math.floor(clamp(k) * c.length);
    return c.map((ch, j) => (ch === ' ' || ch === '/' || j < n ? ch : GL[Math.floor(hash(j + seed, Math.floor(now * 30), 7) * GL.length)])).join('');
  }

  /* ---------- a tree ---------- */
  // Segments carry [x1, y1, x2, y2, growStart, growEnd, heightFraction], by depth.
  function makeTree(seed, hk, maxD) {
    const r = mulberry(seed), S = [], T = [];
    function grow(x, y, a, len, d, t) {
      const x2 = x + Math.cos(a) * len, y2 = y + Math.sin(a) * len, du = 0.08 + len / 260;
      S.push([x, y, x2, y2, t, t + du, d]);
      if (d >= maxD) { T.push([x2, y2, t + du]); return; }
      const n = d < 2 ? 2 : r() < 0.3 ? 3 : 2;
      for (let k = 0; k < n; k++) {
        let b = a + (k - (n - 1) / 2) * (0.36 + r() * 0.34) + (r() - 0.5) * 0.24;
        b += (-Math.PI / 2 - b) * 0.1; // upward tropism
        grow(x2, y2, b, len * (0.7 + r() * 0.12), d + 1, t + du);
      }
    }
    let x = 0, y = 0, t = 0;
    const nT = 4, tl = (150 + r() * 25) * hk;
    for (let k = 0; k < nT; k++) {
      const len = tl / nT, a = -Math.PI / 2 + (r() - 0.5) * 0.07;
      const x2 = x + Math.cos(a) * len, y2 = y + Math.sin(a) * len, d = len / 240;
      S.push([x, y, x2, y2, t, t + d, 0]);
      if (k >= 2 && maxD > 7) {
        const side = r() < 0.5 ? -1 : 1;
        grow(x2, y2, -Math.PI / 2 + side * (0.95 + r() * 0.3), 21 * hk, 6, t + d);
      }
      x = x2; y = y2; t += d;
    }
    grow(x, y, -Math.PI / 2 + (r() - 0.5) * 0.12, (54 + r() * 8) * hk, 1, t);
    let tm = 0, top = 0;
    for (const s of S) { tm = Math.max(tm, s[5]); top = Math.min(top, s[1], s[3]); }
    const Ht = -top, byD = [];
    for (let d = 0; d <= maxD; d++) byD.push([]);
    for (const s of S) byD[s[6]].push(s[0], s[1], s[2], s[3], s[4] / tm, s[5] / tm, -Math.min(s[1], s[3]) / Ht);
    const tips = [];
    T.forEach((p, i) => tips.push(p[0], p[1], p[2] / tm, hash(seed, i)));
    const w = [], al = [];
    for (let d = 0; d <= maxD; d++) {
      w.push(d === 0 ? 6.2 * hk : Math.max(0.55, 5.2 * Math.pow(0.7, d)));
      al.push(Math.max(0.34, 0.95 - d * 0.065));
    }
    let bx0 = 0, bx1 = 0;
    for (const s of S) { bx0 = Math.min(bx0, s[0], s[2]); bx1 = Math.max(bx1, s[0], s[2]); }
    const bb = [bx0 - 24, -Ht - 24, bx1 + 24, 24];
    return { byD: byD.map(a => new Float32Array(a)), tips: new Float32Array(tips), H: Ht, n: S.length, w, al, bb, spr: {}, cdx: (hash(seed, 5) - 0.5) * 60 };
  }

  /* ---------- a grove: trunks that matter, ghosts behind them, one root ---------- */
  const HK = [1.0, 1.08, 0.95, 1.04, 0.93, 1.02, 0.98, 1.06];
  function Grove(opts) {
    const o = Object.assign({ ground: 680, trunks: [310, 570, 830, 1090, 1350, 1610], ghosts: 20, x0: -300, x1: 2220, seed: 0 }, opts || {});
    const G = (this.G = o.ground);
    this.X = o.trunks.slice();
    this.x0 = o.x0; this.x1 = o.x1;
    this.trees = this.X.map((x, i) => makeTree(101 + i * 37 + o.seed, HK[i % HK.length], 9));
    const gr = mulberry(999 + o.seed), span = (o.x1 - o.x0) / Math.max(1, o.ghosts);
    this.ghosts = [];
    for (let k = 0; k < o.ghosts; k++) {
      const x = o.x0 + (k + 0.5) * span + (gr() - 0.5) * span * 0.4;
      if (this.X.some(m => Math.abs(m - x) < 75)) continue;
      this.ghosts.push({ x, y: G - 10 - gr() * 12, s: 0.62 + gr() * 0.26, tr: makeTree(500 + k * 13 + o.seed, 1, 7), gs: gr(), a: 0.7 + gr() * 0.3 });
    }
    this.segments = this.trees.reduce((a, t) => a + t.n, 0) + this.ghosts.reduce((a, g) => a + g.tr.n, 0);

    // the root: two strands, links between them, rootlets
    const RX0 = (this.RX0 = o.x0 - 400), RX1 = (this.RX1 = o.x1 + 400);
    this.rootY = x => G + 95 + 16 * Math.sin(x * 0.0065 + 1) + 7 * Math.sin(x * 0.021);
    this.deepY = x => G + 185 + 14 * Math.sin(x * 0.0051 + 2.3) + 6 * Math.sin(x * 0.017 + 0.7);
    const main = [], deep = [];
    for (let x = RX0; x <= RX1; x += 8) { main.push([x, this.rootY(x)]); deep.push([x, this.deepY(x)]); }
    const pMain = new Path2D(), pDeep = new Path2D(), pLinks = new Path2D(), pLets = new Path2D();
    main.forEach((p, i) => (i ? pMain.lineTo(p[0], p[1]) : pMain.moveTo(p[0], p[1])));
    deep.forEach((p, i) => (i ? pDeep.lineTo(p[0], p[1]) : pDeep.moveTo(p[0], p[1])));
    const rr = mulberry(21 + o.seed);
    for (let x = RX0 + 150; x < RX1 - 100; x += 125) {
      const xx = x + (rr() - 0.5) * 60, dx = (rr() - 0.5) * 90, y0 = this.rootY(xx), x1 = xx + dx, y1 = this.deepY(x1);
      pLinks.moveTo(xx, y0);
      pLinks.bezierCurveTo(xx + (rr() - 0.5) * 60, lerp(y0, y1, 0.35), x1 + (rr() - 0.5) * 60, lerp(y0, y1, 0.7), x1, y1);
    }
    const rlet = (x, y, a, len, d) => {
      const x2 = x + Math.cos(a) * len, y2 = y + Math.sin(a) * len;
      pLets.moveTo(x, y); pLets.lineTo(x2, y2);
      if (d >= 3) return;
      for (let k = 0; k < 2; k++) rlet(x2, y2, a + (k ? 0.5 : -0.5) + (rr() - 0.5) * 0.35, len * 0.7, d + 1);
    };
    for (let x = RX0 + 100; x < RX1; x += 55) { const xx = x + rr() * 30; rlet(xx, this.rootY(xx), Math.PI / 2 + (rr() - 0.5) * 0.9, 16 + rr() * 10, 0); }
    for (let x = RX0 + 120; x < RX1; x += 66) { const xx = x + rr() * 30; rlet(xx, this.deepY(xx), Math.PI / 2 + (rr() - 0.5) * 0.8, 14 + rr() * 8, 1); }
    this.roots = { main, deep, pMain, pDeep, pLinks, pLets };
    const pr = mulberry(55 + o.seed);
    this.rp = [];
    const nRP = Math.round((RX1 - RX0) / 13);
    for (let k = 0; k < nRP; k++) this.rp.push({ u: pr(), v: (pr() < 0.5 ? -1 : 1) * (0.004 + pr() * 0.01), s: 1.6 + pr() * 2.2, strand: 0 });
    for (let k = 0; k < nRP * 0.6; k++) this.rp.push({ u: pr(), v: (pr() < 0.5 ? -1 : 1) * (0.003 + pr() * 0.008), s: 1.3 + pr() * 1.6, strand: 1 });

    // soil: a dot field under the ground that a scan can dissolve
    const soil = [[], [], []];
    for (let y = G + 14; y < G + 430; y += 13) for (let x = RX0; x < RX1; x += 13) {
      const n = hash(x, y, 3); if (n < 0.35) continue;
      const a = (0.05 + 0.18 * n) * (1 - (y - G) / 560), lv = a > 0.12 ? 2 : a > 0.075 ? 1 : 0;
      soil[lv].push(x + (hash(x, y, 4) - 0.5) * 5, y + (hash(x, y, 5) - 0.5) * 5);
    }
    this.soil = soil.map(a => new Float32Array(a));

    // a pocket of its own: the ring drawn around a database
    const qr = mulberry(88 + o.seed), ring = [];
    for (let k = 0; k <= 64; k++) {
      const a = -Math.PI / 2 + (k / 64) * Math.PI * 2, j = 1 + (qr() - 0.5) * 0.16;
      ring.push([Math.cos(a) * 96 * j, Math.sin(a) * 52 * j]);
    }
    const lets = [];
    for (let k = 0; k < 18; k++) { const a = qr() * Math.PI * 2; lets.push([Math.cos(a) * 96, Math.sin(a) * 52, a, 10 + qr() * 14]); }
    this.pocketShape = { ring, lets };

    // a burst of sparks for the moment everything comes back
    const br = mulberry(77 + o.seed);
    this.burstP = [];
    for (let k = 0; k < 260; k++) { const a = br() * Math.PI * 2; this.burstP.push({ dx: Math.cos(a), dy: Math.sin(a) * 0.8, sp: 500 + br() * 1300, s: 1.5 + br() * 2.5, cy: br() < 0.6, l: 0.6 + br() * 0.8 }); }

    this._cache = {};
  }

  /* ---------- the view: a canvas, a camera, a device pixel ratio ---------- */
  function View(canvas, opts) {
    this.cv = canvas;
    this.ctx = canvas.getContext('2d');
    this.opts = opts || {};
    this.w = 1; this.h = 1; this.dpr = 1;
    this.cam = { x: 960, y: 540, z: 1 };
    this.pointer = null; // world coords of the pointer, when it is over the canvas
    this.resize();
  }
  View.prototype.resize = function () {
    const r = this.cv.getBoundingClientRect();
    const dpr = Math.min(window.devicePixelRatio || 1, this.opts.maxDpr || 2);
    const w = Math.max(1, Math.round(r.width)), h = Math.max(1, Math.round(r.height));
    if (w === this.w && h === this.h && dpr === this.dpr) return false;
    this.w = w; this.h = h; this.dpr = dpr;
    this.cv.width = Math.round(w * dpr); this.cv.height = Math.round(h * dpr);
    return true;
  };
  View.prototype.world = function () {
    const { ctx, dpr, cam, w, h } = this;
    ctx.setTransform(dpr * cam.z, 0, 0, dpr * cam.z, dpr * (w / 2 - cam.x * cam.z), dpr * (h / 2 - cam.y * cam.z));
  };
  View.prototype.screen = function () { this.ctx.setTransform(this.dpr, 0, 0, this.dpr, 0, 0); };
  View.prototype.proj = function (wx, wy) { const c = this.cam; return [(wx - c.x) * c.z + this.w / 2, (wy - c.y) * c.z + this.h / 2]; };
  View.prototype.unproj = function (sx, sy) { const c = this.cam; return [(sx - this.w / 2) / c.z + c.x, (sy - this.h / 2) / c.z + c.y]; };
  // fit a world rectangle into the view; `minZ` stops a narrow screen from shrinking it to nothing
  View.prototype.fit = function (x0, y0, x1, y1, o) {
    o = o || {};
    const pad = o.pad == null ? 0 : o.pad;
    let z = Math.min((this.w - pad * 2) / (x1 - x0), (this.h - pad * 2) / (y1 - y0));
    if (o.minZ) z = Math.max(z, o.minZ * (this.w < 700 ? 1 : 1));
    if (o.maxZ) z = Math.min(z, o.maxZ);
    return { x: (x0 + x1) / 2, y: (y0 + y1) / 2, z };
  };
  View.prototype.clear = function () {
    const { ctx } = this;
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.clearRect(0, 0, this.cv.width, this.cv.height);
  };
  View.prototype.trackPointer = function (el) {
    const target = el || this.cv;
    const set = e => {
      const r = this.cv.getBoundingClientRect();
      const p = e.touches ? e.touches[0] : e;
      this.pointerScreen = [p.clientX - r.left, p.clientY - r.top];
    };
    target.addEventListener('pointermove', set, { passive: true });
    target.addEventListener('pointerdown', set, { passive: true });
    target.addEventListener('pointerleave', () => { this.pointerScreen = null; }, { passive: true });
  };

  /* ---------- drawing ---------- */
  // o: { x, y, s, F (growth 0..1), alpha, green (0..1), gold (0..1), shimmer (0..1), glow }
  function drawTree(view, tr, o, now) {
    if (o.F <= 0.0005 || o.alpha <= 0.003) return;
    const ctx = view.ctx, zs = view.cam.z * o.s, F = o.F, g = o.green || 0, gold = o.gold || 0;
    ctx.save(); ctx.translate(o.x, o.y); ctx.scale(o.s, o.s); ctx.lineCap = 'round';
    const cr = Math.round(lerp(236 + (124 - 236) * g, 246, gold)), cg = Math.round(lerp(236 + (255 - 236) * g, 190, gold)), cb = Math.round(lerp(236 + (178 - 236) * g, 88, gold));
    // A fully grown tree in a settled colour is drawn once into a sprite and
    // reused; the leaves stay live so they can still quake.
    const settled = !view.noSprite && F >= 1 && (g < 0.02 || g > 0.98) && gold < 0.02 && o.alpha > 0.98;
    if (settled) {
      const sc = clamp(Math.round(zs * view.dpr * 4) / 4, 0.25, 2.5);
      const key = (g > 0.5 ? 'g' : 'w') + sc + (o.glow === 0 ? 'n' : '');
      let sp = tr.spr[key];
      if (!sp) {
        if (Object.keys(tr.spr).length > 5) tr.spr = {};
        const bb = tr.bb, cv = document.createElement('canvas');
        cv.width = Math.ceil((bb[2] - bb[0]) * sc); cv.height = Math.ceil((bb[3] - bb[1]) * sc);
        const cx = cv.getContext('2d');
        cx.setTransform(sc, 0, 0, sc, -bb[0] * sc, -bb[1] * sc); cx.lineCap = 'round';
        strokeBranches(cx, tr, 1, g, 0, o.glow, view.dpr, zs);
        sp = tr.spr[key] = cv;
      }
      const bb = tr.bb;
      ctx.drawImage(sp, bb[0], bb[1], bb[2] - bb[0], bb[3] - bb[1]);
    } else {
      strokeBranches(ctx, tr, F, g, gold, o.glow, view.dpr, zs, o.alpha);
    }
    ctx.shadowBlur = 0;
    // leaves: the tips, which quake. Aspen leaves have flat stems and tremble in
    // the slightest wind; near the pointer they tremble harder and catch the light.
    const T = tr.tips, sz = Math.max(2.8, 1.4 / zs);
    const px = view._treePointer && view._treePointer[0] - o.x, py = view._treePointer && view._treePointer[1] - o.y;
    const sh0 = o.shimmer || 0;
    for (let q = 0; q < 3; q++) {
      ctx.beginPath(); let any = false;
      const hot = [];
      for (let i = 0; i < T.length; i += 4) {
        if (T[i + 2] >= F || ((T[i + 3] * 3) | 0) !== q) continue;
        let x = T[i], y = T[i + 1];
        const wob = Math.sin(now * 2.1 + x * 0.05 + q) * 0.7;
        x += wob;
        if (px != null) {
          const dx = x - px / o.s, dy = y - py / o.s, d2 = dx * dx + dy * dy;
          if (d2 < 19600) {
            const k = 1 - Math.sqrt(d2) / 140, ph = now * 26 + T[i + 3] * 40;
            x += Math.sin(ph) * 3.2 * k; y += Math.cos(ph * 0.8) * 1.6 * k;
            if (k > 0.25) { hot.push(x, y); continue; }
          }
        }
        ctx.rect(x - sz / 2, y - sz / 2, sz, sz); any = true;
      }
      const sh = sh0 ? sh0 * (0.5 + 0.5 * Math.sin(now * 23 + q * 2.1)) : 0;
      const a = Math.min(1, (0.5 + 0.45 * g + sh * 0.9) * o.alpha);
      if (any) {
        if (sh > 0.05) { ctx.shadowColor = 'rgba(124,255,178,.9)'; ctx.shadowBlur = 10 * sh * view.dpr; }
        ctx.fillStyle = sh > 0.05 ? `rgba(${Math.round(lerp(cr, 210, sh))},255,${Math.round(lerp(cb, 225, sh))},${a})` : `rgba(${cr},${cg},${cb},${a})`;
        ctx.fill(); ctx.shadowBlur = 0;
      }
      if (hot.length) {
        ctx.beginPath();
        for (let j = 0; j < hot.length; j += 2) ctx.rect(hot[j] - sz * 0.6, hot[j + 1] - sz * 0.6, sz * 1.2, sz * 1.2);
        ctx.fillStyle = g > 0.5 ? `rgba(200,255,225,${Math.min(1, a + 0.3)})` : `rgba(252,228,165,${Math.min(1, a + 0.35)})`;
        ctx.fill();
      }
    }
    ctx.restore();
  }

  function strokeBranches(ctx, tr, F, g, gold, glow, dpr, zs, alpha) {
    alpha = alpha == null ? 1 : alpha;
    const cr = Math.round(lerp(236 + (124 - 236) * g, 246, gold)), cg = Math.round(lerp(236 + (255 - 236) * g, 190, gold)), cb = Math.round(lerp(236 + (178 - 236) * g, 88, gold));
    if (gold > 0.05) { ctx.shadowColor = 'rgba(242,184,75,.9)'; ctx.shadowBlur = 18 * gold * dpr; }
    else if (g > 0.4 && glow !== 0) { ctx.shadowColor = 'rgba(124,255,178,.7)'; ctx.shadowBlur = (glow || 10) * g * dpr; }
    for (let d = 0; d < tr.byD.length; d++) {
      const A = tr.byD[d]; ctx.beginPath(); let any = false;
      for (let i = 0; i < A.length; i += 7) {
        const g0 = A[i + 4]; if (F <= g0) continue;
        const g1 = A[i + 5], p = F >= g1 ? 1 : (F - g0) / (g1 - g0), x1 = A[i], y1 = A[i + 1];
        ctx.moveTo(x1, y1); ctx.lineTo(x1 + (A[i + 2] - x1) * p, y1 + (A[i + 3] - y1) * p); any = true;
      }
      if (!any) continue;
      ctx.lineWidth = Math.max(tr.w[d], 0.9 / zs);
      ctx.strokeStyle = `rgba(${cr},${cg},${cb},${tr.al[d] * alpha})`;
      ctx.stroke();
    }
    ctx.shadowBlur = 0;
  }

  // a band of gold that climbs a tree when it is named
  function drawLit(view, tr, o, front, rgb) {
    if (front <= 0 || front > 1.35 || o.F < 0.3) return;
    const ctx = view.ctx;
    ctx.save(); ctx.translate(o.x, o.y); ctx.scale(o.s, o.s); ctx.lineCap = 'round';
    ctx.shadowColor = `rgb(${rgb})`; ctx.shadowBlur = 14 * view.dpr;
    const fade = 1 - ss(1.05, 1.35, front);
    for (let b = 0; b < 3; b++) {
      const lo = front - 0.11 * (b + 1), hi = front - 0.11 * b;
      ctx.beginPath();
      for (let d = 0; d < tr.byD.length; d++) {
        const A = tr.byD[d];
        for (let i = 0; i < A.length; i += 7) { const hf = A[i + 6]; if (hf <= lo || hf > hi || A[i + 5] > o.F) continue; ctx.moveTo(A[i], A[i + 1]); ctx.lineTo(A[i + 2], A[i + 3]); }
      }
      ctx.lineWidth = 3.4 - b * 0.8;
      ctx.strokeStyle = b === 0 ? `rgba(255,244,214,${fade})` : `rgba(${rgb},${(0.95 - 0.3 * b) * fade})`;
      ctx.stroke();
    }
    ctx.restore();
  }

  // Draw into an offscreen canvas that covers a world rectangle, at a scale that
  // follows the view's zoom in steps; reused while the zoom stays in its step.
  function cached(view, grove, key, rect, paint) {
    const scale = Math.min(1.6, Math.max(0.25, Math.round(view.cam.z * view.dpr * 4) / 4));
    let c = grove._cache[key];
    if (!c || c.scale !== scale) {
      const cv = document.createElement('canvas');
      cv.width = Math.ceil((rect[2] - rect[0]) * scale); cv.height = Math.ceil((rect[3] - rect[1]) * scale);
      const cx = cv.getContext('2d');
      cx.setTransform(scale, 0, 0, scale, -rect[0] * scale, -rect[1] * scale);
      paint(cx, scale);
      c = grove._cache[key] = { cv, scale, rect };
    }
    view.ctx.drawImage(c.cv, c.rect[0], c.rect[1], c.rect[2] - c.rect[0], c.rect[3] - c.rect[1]);
  }

  // soil above `scanY` is gone; below it, the dots are still there
  Grove.prototype.drawSoil = function (view, scanY, alpha) {
    if (alpha <= 0) return;
    const G = this.G, ctx = view.ctx, rect = [this.RX0, G, this.RX1, G + 440];
    const top = Math.max(G, scanY);
    if (top >= G + 440) return;
    ctx.save();
    ctx.beginPath(); ctx.rect(this.RX0, top, this.RX1 - this.RX0, G + 440 - top); ctx.clip();
    ctx.globalAlpha = alpha;
    cached(view, this, 'soil', rect, (cx, sc) => {
      const al = [0.08, 0.12, 0.18], s = Math.max(2.3, 1.2 / sc);
      for (let lv = 0; lv < 3; lv++) {
        const A = this.soil[lv]; cx.beginPath();
        for (let i = 0; i < A.length; i += 2) cx.rect(A[i], A[i + 1], s, s);
        cx.fillStyle = `rgba(236,236,236,${al[lv]})`; cx.fill();
      }
    });
    ctx.restore();
    if (scanY > G && scanY < G + 440) {
      ctx.save(); ctx.shadowColor = C.gold; ctx.shadowBlur = 16 * view.dpr;
      ctx.strokeStyle = 'rgba(242,184,75,.9)'; ctx.lineWidth = 2 / view.cam.z;
      ctx.beginPath(); ctx.moveTo(this.RX0, scanY); ctx.lineTo(this.RX1, scanY); ctx.stroke();
      ctx.restore();
    }
  };

  // the ground line, faint, with a tick under every trunk
  Grove.prototype.drawGround = function (view, alpha) {
    const ctx = view.ctx, G = this.G;
    ctx.save();
    ctx.strokeStyle = `rgba(236,236,236,${0.16 * alpha})`; ctx.lineWidth = 1 / view.cam.z;
    ctx.beginPath(); ctx.moveTo(this.RX0, G); ctx.lineTo(this.RX1, G); ctx.stroke();
    ctx.restore();
  };

  /* st: { reveal (world y the roots are drawn down to), alpha, glow, trunks: [{x, y, cdx, pres}] } */
  Grove.prototype.drawRoots = function (view, st, now) {
    const A = st.alpha == null ? 1 : st.alpha; if (A <= 0) return;
    const ctx = view.ctx, gl = st.glow || 1, G = this.G, z = view.cam.z;
    ctx.save();
    if (st.reveal != null && st.reveal < G + 440) { ctx.beginPath(); ctx.rect(this.RX0, G - 2, this.RX1 - this.RX0, st.reveal - G + 2); ctx.clip(); }
    ctx.lineCap = 'round';
    ctx.globalAlpha = A;
    cached(view, this, 'roots', [this.RX0, G + 40, this.RX1, G + 300], (cx, sc) => {
      cx.lineCap = 'round';
      cx.strokeStyle = 'rgba(217,130,43,.3)'; cx.lineWidth = Math.max(0.8, 0.9 / sc); cx.stroke(this.roots.pLets);
      cx.strokeStyle = 'rgba(242,184,75,.34)'; cx.lineWidth = Math.max(1, 1.1 / sc); cx.stroke(this.roots.pLinks);
      cx.shadowColor = 'rgba(242,184,75,.9)';
      cx.shadowBlur = 5 * sc; cx.strokeStyle = 'rgba(242,184,75,.5)'; cx.lineWidth = 1.6; cx.stroke(this.roots.pDeep);
      cx.shadowBlur = 11 * sc; cx.strokeStyle = 'rgba(242,184,75,.82)'; cx.lineWidth = 2.5; cx.stroke(this.roots.pMain);
    });
    // extra glow: the cached root again, added on top
    if (gl > 1.02) {
      ctx.globalCompositeOperation = 'lighter';
      ctx.globalAlpha = A * Math.min(1, (gl - 1) * 0.55);
      ctx.drawImage(this._cache.roots.cv, this.RX0, G + 40, this.RX1 - this.RX0, 260);
      ctx.globalCompositeOperation = 'source-over';
    }
    ctx.globalAlpha = 1;
    // connectors: every trunk drinks from the one root; one path per colour
    ctx.shadowColor = 'rgba(242,184,75,.9)'; ctx.shadowBlur = 5 * gl * view.dpr; ctx.lineWidth = 1.7;
    const paths = new Map();
    for (const c of st.trunks || []) {
      if (c.pres <= 0) continue;
      const ex = c.x + c.cdx, ey = this.rootY(ex);
      const col = c.green > 0.5 ? `rgba(124,255,178,${(0.55 * A * c.pres).toFixed(2)})` : `rgba(242,184,75,${Math.min(1, 0.62 * A * c.pres * (c.flare ? 1 + c.flare : 1)).toFixed(2)})`;
      let p = paths.get(col); if (!p) paths.set(col, (p = new Path2D()));
      p.moveTo(c.x, c.y); p.bezierCurveTo(c.x, c.y + 45, ex, ey - 35, ex, ey);
    }
    paths.forEach((p, col) => { ctx.strokeStyle = col; ctx.stroke(p); });
    ctx.shadowBlur = 0;
    // particles riding the root, and climbing each connector into its trunk
    const pa = (st.particles == null ? 1 : st.particles) * A;
    if (pa > 0) {
      ctx.fillStyle = `rgba(250,222,150,${0.85 * pa})`; ctx.beginPath();
      const s0 = 1 / Math.max(z, 0.8);
      for (const p of this.rp) {
        const u = (((p.u + p.v * now) % 1) + 1) % 1, q = at(p.strand ? this.roots.deep : this.roots.main, u), s = p.s * s0;
        ctx.rect(q[0] - s / 2, q[1] - s / 2, s, s);
      }
      ctx.fill();
      ctx.beginPath();
      for (const c of st.trunks || []) {
        if (c.pres < 0.5) continue;
        const ex = c.x + c.cdx, ey = this.rootY(ex);
        for (let j = 0; j < 2; j++) {
          const u = 1 - (((now + j * 0.85 + c.x * 0.001) / 1.7) % 1);
          const q = bez(c.x, c.y, c.x, c.y + 45, ex, ey - 35, ex, ey, u), s = 3 * s0;
          ctx.rect(q[0] - s / 2, q[1] - s / 2, s, s);
        }
      }
      ctx.fillStyle = `rgba(255,234,178,${0.9 * pa})`; ctx.fill();
    }
    ctx.restore();
  };

  // a pulse running along the root; u: 0..1 across the visible span [xa, xb]
  Grove.prototype.drawWave = function (view, u, rgb, xa, xb) {
    if (u <= 0 || u >= 1.02) return;
    const ctx = view.ctx, xw = lerp(xa, xb, u);
    ctx.save();
    // one soft glow around the head, then the trail as flat dots
    for (const [fn, off, sc] of [[this.rootY, 0, 1], [this.deepY, 170, 0.7]]) {
      const hx = xw - off - 90, hy = fn(hx), R = 170 * sc;
      const rg = ctx.createRadialGradient(hx, hy, 0, hx, hy, R);
      rg.addColorStop(0, `rgba(${rgb},${0.28 * sc})`); rg.addColorStop(1, `rgba(${rgb},0)`);
      ctx.fillStyle = rg; ctx.fillRect(hx - R, hy - R, R * 2, R * 2);
      for (let j = 0; j < 42; j++) {
        const x = xw - off - j * 13, y = fn(x), a = (1 - j / 42) * sc;
        ctx.fillStyle = `rgba(${rgb},${a.toFixed(3)})`; ctx.beginPath(); ctx.arc(x, y, (5.5 - j * 0.1) * sc, 0, 6.283); ctx.fill();
      }
    }
    ctx.shadowColor = `rgb(${rgb})`; ctx.shadowBlur = 16 * view.dpr;
    ctx.fillStyle = '#FFFFFF'; ctx.beginPath(); ctx.arc(xw, this.rootY(xw), 4, 0, 6.283); ctx.fill();
    ctx.restore();
  };

  // A pocket of its own under a trunk: a line down past the shared root, a ring, a
  // database. kind: 'isolated' (its own server) or 'namespaced' (its own database,
  // drawn inside the main checkout's pocket).
  Grove.prototype.drawPocket = function (view, x0, k, now, o) {
    if (k <= 0) return;
    o = o || {};
    const ctx = view.ctx, depth = o.depth || 250, sc = o.scale || 1, A = o.alpha == null ? 1 : o.alpha;
    const col = o.rgb || '242,184,75';
    ctx.save(); ctx.translate(x0, this.G); ctx.lineCap = 'round';
    ctx.shadowColor = `rgb(${col})`; ctx.shadowBlur = 12 * view.dpr;
    if (!o.noStem) {
      ctx.strokeStyle = `rgba(${col},${0.8 * A})`; ctx.lineWidth = 2;
      const kk = clamp(k * 2.2), len = depth - 52 * sc;
      ctx.beginPath(); ctx.moveTo(0, 0);
      for (let j = 1; j <= 20; j++) { const u = (j / 20) * kk; ctx.lineTo(Math.sin(u * 5) * 6 * u, u * len); }
      ctx.stroke();
    }
    ctx.translate(0, depth); ctx.scale(sc, sc);
    const R = this.pocketShape.ring, n = Math.floor(R.length * clamp(k * 1.3 - 0.25));
    if (n > 1) {
      ctx.fillStyle = `rgba(${col},${0.07 * A})`;
      ctx.beginPath(); for (let j = 0; j < n; j++) (j ? ctx.lineTo(R[j][0], R[j][1]) : ctx.moveTo(R[j][0], R[j][1]));
      if (n >= R.length) { ctx.closePath(); ctx.fill(); }
      ctx.strokeStyle = `rgba(${col},${0.9 * A})`; ctx.lineWidth = 2.2 / sc; ctx.stroke();
      ctx.shadowBlur = 0; ctx.strokeStyle = `rgba(${col},${0.35 * A})`; ctx.lineWidth = 1.1 / sc; ctx.beginPath();
      const m = clamp(k * 2 - 1);
      for (const l of this.pocketShape.lets) { ctx.moveTo(l[0], l[1]); ctx.lineTo(l[0] + Math.cos(l[2]) * l[3] * m, l[1] + Math.sin(l[2]) * l[3] * m); }
      ctx.stroke();
    }
    const c = clamp(k * 1.6 - 0.5);
    if (c > 0) drawCylinder(ctx, 0, 0, c * A, view.dpr, o.cylinders || 1, o.cylColor);
    if (k > 0.6) {
      ctx.fillStyle = `rgba(250,222,150,${(k - 0.6) * 2 * A})`; ctx.beginPath();
      for (let j = 0; j < 12; j++) { const a = now * 0.9 + j * 0.5236; ctx.rect(Math.cos(a) * 80 - 1.8, Math.sin(a) * 40 - 1.8, 3.6, 3.6); }
      ctx.fill();
    }
    ctx.restore();
  };

  function drawCylinder(ctx, cx, cy, a, dpr, count, color) {
    ctx.save();
    ctx.globalAlpha = a; ctx.strokeStyle = color || '#FBE3A6'; ctx.lineWidth = 2.2; ctx.shadowColor = C.gold; ctx.shadowBlur = 10 * dpr;
    const rw = 30, rh = 9, hh = 38;
    const one = (x) => {
      ctx.beginPath(); ctx.ellipse(x, cy - hh / 2, rw, rh, 0, 0, 6.283); ctx.stroke();
      ctx.beginPath(); ctx.moveTo(x - rw, cy - hh / 2); ctx.lineTo(x - rw, cy + hh / 2); ctx.ellipse(x, cy + hh / 2, rw, rh, 0, Math.PI, 0, true); ctx.lineTo(x + rw, cy - hh / 2); ctx.stroke();
      ctx.beginPath(); ctx.ellipse(x, cy, rw, rh, 0, Math.PI, 0, true); ctx.stroke();
    };
    if (count === 1) one(cx);
    else for (let j = 0; j < count; j++) one(cx + (j - (count - 1) / 2) * 72);
    ctx.restore();
  }

  // the moment everything comes back: a ring and sparks from the seed; e in seconds
  Grove.prototype.drawBurst = function (view, x, y, e) {
    if (e < 0 || e > 1.6) return;
    const ctx = view.ctx;
    ctx.save();
    const R = 1900 * E.out(clamp(e / 1.2));
    ctx.strokeStyle = `rgba(242,184,75,${0.8 * (1 - clamp(e / 1.2))})`; ctx.lineWidth = 3 / view.cam.z; ctx.shadowColor = C.gold; ctx.shadowBlur = 20 * view.dpr;
    ctx.beginPath(); ctx.ellipse(x, y, R, R * 0.55, 0, 0, 6.283); ctx.stroke();
    ctx.shadowBlur = 0;
    for (const p of this.burstP) {
      if (e > p.l) continue;
      const d = (p.sp * (1 - Math.exp(-e * 3.2))) / 3.2, a = 1 - e / p.l;
      ctx.fillStyle = p.cy ? `rgba(242,184,75,${a})` : `rgba(236,236,236,${a})`;
      ctx.fillRect(x + p.dx * d - p.s / 2, y - 20 + p.dy * d - p.s / 2, p.s, p.s);
    }
    const f = 1 - clamp(e / 0.35);
    if (f > 0) {
      const rg = ctx.createRadialGradient(x, y, 0, x, y, 260);
      rg.addColorStop(0, `rgba(252,228,165,${0.7 * f})`); rg.addColorStop(1, 'rgba(242,184,75,0)');
      ctx.fillStyle = rg; ctx.fillRect(x - 260, y - 260, 520, 520);
    }
    ctx.restore();
  };

  // the ghosts: the grove behind the named trunks. F: growth, a: alpha
  Grove.prototype.drawGhosts = function (view, F, alpha, now, perGhost) {
    if (alpha <= 0) return;
    const stable = !perGhost && F >= 1;
    if (stable && !view.noCache) {
      const G = this.G;
      view.ctx.save(); view.ctx.globalAlpha = alpha;
      cached(view, this, 'ghosts', [this.x0 - 160, G - 330, this.x1 + 160, G + 4], cx => {
        const fake = { ctx: cx, cam: { z: 1 }, dpr: 1, noSprite: true };
        for (const g of this.ghosts) drawTree(fake, g.tr, { x: g.x, y: g.y, s: g.s, F: 1, alpha: g.a }, 0);
      });
      view.ctx.restore();
      return;
    }
    for (let k = 0; k < this.ghosts.length; k++) {
      const g = this.ghosts[k], f = perGhost ? perGhost(g, k) : F;
      drawTree(view, g.tr, { x: g.x, y: g.y, s: g.s, F: f, alpha: alpha * g.a }, now);
    }
  };

  // a label under a trunk: a leader line from the trunk's foot, the name in a dark box
  function drawLabel(view, sx, sy, text, o) {
    o = o || {};
    const ctx = view.ctx, size = o.size || 22, row = o.row || 0, a = o.alpha == null ? 1 : o.alpha;
    if (a <= 0 || !text) return;
    const ly = sy + (row ? size * 2.95 : size * 1.6);
    ctx.save();
    ctx.font = `500 ${size}px ${MONO}`; const w = ctx.measureText(text).width;
    const lx = clamp(sx, w / 2 + 12, view.w - w / 2 - 12);
    ctx.globalAlpha = a;
    ctx.fillStyle = 'rgba(5,5,5,.8)'; ctx.fillRect(lx - w / 2 - 8, ly - size * 0.95, w + 16, size * 1.3);
    const green = o.green > 0.5;
    ctx.strokeStyle = o.gold ? 'rgba(242,184,75,.7)' : green ? 'rgba(124,255,178,.55)' : 'rgba(236,236,236,.35)'; ctx.lineWidth = 1;
    ctx.beginPath(); ctx.moveTo(sx, sy + 3); ctx.lineTo(sx, ly - size * 0.95); ctx.stroke();
    ctx.fillStyle = o.gold ? C.gold : green ? C.green : C.fg; ctx.textAlign = 'center'; ctx.textBaseline = 'alphabetic';
    if (green || o.gold) { ctx.shadowColor = o.gold ? C.gold : C.green; ctx.shadowBlur = 10 * view.dpr; }
    ctx.fillText(text, lx, ly);
    ctx.shadowBlur = 0;
    if (o.strike > 0) {
      ctx.strokeStyle = C.orange; ctx.lineWidth = 3; ctx.shadowColor = C.orange; ctx.shadowBlur = 8 * view.dpr;
      ctx.beginPath(); ctx.moveTo(lx - w / 2 - 4, ly - size * 0.32); ctx.lineTo(lx - w / 2 - 4 + (w + 8) * o.strike, ly - size * 0.32); ctx.stroke();
    }
    ctx.restore();
    return { lx, ly, w };
  }

  // a spark that flies from one point to another along an arc, leaving a trail
  function comet(view, x0, y0, x1, y1, k, rgb, size, lift) {
    if (k <= 0 || k > 1.25) return;
    const ctx = view.ctx; size = size || 6; lift = lift == null ? 150 : lift;
    ctx.save(); ctx.shadowColor = `rgb(${rgb})`; ctx.shadowBlur = 14 * view.dpr;
    const pt = u => { const mx = (x0 + x1) / 2, my = Math.min(y0, y1) - lift; const v = 1 - u; return [v * v * x0 + 2 * v * u * mx + u * u * x1, v * v * y0 + 2 * v * u * my + u * u * y1]; };
    const kk = Math.min(1, k);
    for (let j = 0; j < 14; j++) {
      const u = kk - j * 0.025; if (u < 0) break;
      const [x, y] = pt(E.io(u)), a = 1 - j / 14;
      ctx.fillStyle = j === 0 ? `rgba(255,255,255,${a})` : `rgba(${rgb},${a})`;
      ctx.beginPath(); ctx.arc(x, y, size * (1 - j / 18), 0, 6.283); ctx.fill();
    }
    if (k >= 1) { const e = (k - 1) / 0.25; ctx.strokeStyle = `rgba(${rgb},${1 - e})`; ctx.lineWidth = 2.5; ctx.beginPath(); ctx.arc(x1, y1, 8 + e * 50, 0, 6.283); ctx.stroke(); }
    ctx.restore();
  }

  // floating dust in screen space
  function Dust(n, seed) {
    const dr = mulberry(seed || 33);
    this.p = [];
    for (let k = 0; k < n; k++) this.p.push({ x: dr(), y: dr(), vx: (dr() - 0.5) * 14, vy: -4 - dr() * 10, s: 0.8 + dr() * 1.6, a: 0.06 + dr() * 0.18, ph: dr() * 6.28 });
  }
  Dust.prototype.draw = function (view, now, alpha) {
    const ctx = view.ctx; view.screen();
    ctx.fillStyle = '#ECECEC';
    for (const p of this.p) {
      const x = (((p.x * view.w + p.vx * now) % view.w) + view.w) % view.w;
      const y = (((p.y * view.h + p.vy * now) % view.h) + view.h) % view.h;
      ctx.globalAlpha = p.a * (0.6 + 0.4 * Math.sin(now * 1.3 + p.ph)) * (alpha == null ? 1 : alpha);
      ctx.fillRect(x, y, p.s, p.s);
    }
    ctx.globalAlpha = 1;
  };

  function at(pts, u) { const f = u * (pts.length - 1), k = Math.floor(f), a = pts[k], b = pts[Math.min(k + 1, pts.length - 1)], e = f - k; return [a[0] + (b[0] - a[0]) * e, a[1] + (b[1] - a[1]) * e]; }
  function bez(x0, y0, x1, y1, x2, y2, x3, y3, u) { const v = 1 - u; return [v * v * v * x0 + 3 * v * v * u * x1 + 3 * v * u * u * x2 + u * u * u * x3, v * v * v * y0 + 3 * v * v * u * y1 + 3 * v * u * u * y2 + u * u * u * y3]; }

  /* ---------- a loop that only runs while its canvas is on screen ---------- */
  // The reader scrolling, pointing or typing: while they do, a loop with an
  // `idle` option draws every frame; between, the slow motion that is always
  // there (leaves, dust, the root's particles) reads the same at half the rate,
  // for half the work. `busy()` keeps the full rate for a loop's own quick motion.
  let activeAt = -1e9;
  const touch = () => { activeAt = performance.now(); };
  ['scroll', 'wheel', 'pointermove', 'pointerdown', 'touchmove', 'keydown', 'resize'].forEach(t => window.addEventListener(t, touch, { passive: true, capture: true }));
  function loop(el, frame, o) {
    o = o || {};
    let visible = false, raf = 0, last = 0, drawn = 0;
    const tick = ts => {
      raf = 0;
      if (!visible || document.hidden) return;
      if (o.idle && drawn && ts - drawn < 25 && ts - activeAt > 400 && !(o.busy && o.busy())) { raf = requestAnimationFrame(tick); return; }
      drawn = ts;
      // with reduced motion the clock stands still: nothing drifts, quakes or flows
      const now = reduced ? 0 : ts / 1000;
      frame(now, last ? Math.min(0.1, now - last) : 0);
      last = now;
      raf = requestAnimationFrame(tick);
    };
    const start = () => { if (!raf) { last = 0; drawn = 0; raf = requestAnimationFrame(tick); } };
    new IntersectionObserver(es => { visible = es[es.length - 1].isIntersecting; if (visible) start(); }, { rootMargin: '120px' }).observe(el);
    document.addEventListener('visibilitychange', () => { if (!document.hidden && visible) start(); });
    return { kick: start, isVisible: () => visible };
  }

  const reduced = window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches;

  P.MONO = MONO; P.C = C;
  P.m = { mulberry, hash, clamp, lerp, ss, E, prog, kick };
  P.decode = decode;
  P.Grove = Grove; P.View = View; P.Dust = Dust;
  P.drawTree = drawTree; P.drawLit = drawLit; P.drawLabel = drawLabel; P.drawCylinder = drawCylinder; P.comet = comet;
  P.loop = loop; P.reduced = reduced;
  // Draw once the font is here, or after a moment without it; when it arrives
  // late, 'pando:fonts' tells whoever measured text to measure again.
  const fontLoad = (document.fonts && document.fonts.load)
    ? Promise.all(['400 16px "JetBrains Mono"', '500 16px "JetBrains Mono"', '700 16px "JetBrains Mono"'].map(f => document.fonts.load(f))).catch(() => {})
    : Promise.resolve();
  fontLoad.then(() => window.dispatchEvent(new Event('pando:fonts')));
  P.fontsReady = Promise.race([fontLoad, new Promise(r => setTimeout(r, 1200))]);
})();
