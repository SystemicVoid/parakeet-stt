/* PROTOTYPE (throwaway). Variant E — "Ambient".
 *
 * BOLD BET: the overlay is not an object, it is LIGHT. A wide, soft field spills up from the
 * bottom edge of the screen; its brightness is the mic, its shape is the phase. The transcript is
 * a small quiet caption sitting inside that light. Nothing has an edge, a border or a radius.
 *
 * PALETTE (4, all with jobs — the two working hues are COLOUR TEMPERATURES, not "brand colours":
 *   the field is light, so its source is encoded the way real light is: cold vs warm.)
 *   DAY   #8FC2F2  your voice: daylight-cold field + a 95 % tint (#EAF1F8) as caption ink
 *   LAMP  #F2C98D  the model: lamplight-warm field + a 95 % tint (#FBF0E0) as answer ink
 *   EMBER #FF6A55  failure only: the error bloom, and the cap-warning clock. Never decorative.
 *   SHADE #060A0E  the shelf: our own soft darkening under the caption (we cannot blur the desktop).
 * Both hues are near-white and low-saturation, so the field reads as illumination, never as a
 * toast/battery chip — those are saturated, small, and have edges.
 *
 * TYPE: Fira Sans only (400 caption 17/25.5, 400 question 13, 400 clock 12 tabular). No labels,
 * no state words, no rotating phrases: the light says the state, the text only says the words.
 *
 * SILHOUETTE: full-bleed light band 360 px tall at the bottom edge; caption in a 960 px column,
 * LEFT aligned (centred text jitters horizontally on every 2.4 s burst), 4 lines max, bottom at
 * 52 px; cap clock at 32 px. Anchor stays bottom-centre: a light field only makes sense on an edge.
 *
 * STATE AT A GLANCE (form/motion first, hue second):  listening = low flat cold sill, barely there.
 * speaking = the same field breathing with the level. sealing = it CONTRACTS to a narrow bright
 * column and goes still. done = one outward release pulse (fail = a collapse, no pulse).
 * answering = warm, wide, still, flickering once per token. error = one fast red bloom, then decay.
 * Press→light is < 90 ms plus a swell peaking at 110 ms: that swell is the pre-roll landing.
 *
 * A BURST LANDS: appended chars fade in at 16 ms stagger over ~100 ms; the revisable tail is the
 * same ink at 55 %, so a revision is a change of weight, not a jump. The seal keeps the text.
 *
 * FOR THE RUST IMPLEMENTER: bundle Fira Sans Regular only. Every pixel here is alpha(x,y) =
 * f(y)*g(x)*intensity over one RGB — a separable gradient: build one row LUT and one column LUT
 * per frame (≈2 k adds) and blend saturating-add into the shm buffer; no blur pass, no shadow, no
 * SDF. Cost is one full-width 360 px band of adds; 30 fps is plenty, cap dirty rect to the band.
 * The shelf is the same formula in black with source-over. Text is per-glyph alpha as today.
 *
 * RISK: with no frame at all, on a busy light desktop the caption leans entirely on the shelf; if
 * the shelf is too weak it turns to mud, too strong and I have re-invented a card.
 */
(function () {
  'use strict';

  const FIELD_H = 360;      // height of the light band, px
  const OW = 320, OH = 180; // offscreen field resolution (upsampled; Rust does it per-pixel)
  const DAY = [143, 194, 242];
  const LAMP = [242, 201, 141];
  const EMBER = [255, 106, 85];
  const SHADE = '6,10,14';
  const MAX_LINES = 4, LINE_H = 25.5;

  let root, cv, ctx, off, octx, wrap, qEl, clipEl, capEl, metaEl;
  let W = 0, H = 0, canvasTop = 0;
  let last = 0, lvl = 0, inten = 0, half = 0.46, fh = 120, warm = 0, ember = 0, capA = 0;
  let capRect = null, sessionKey = -1, textStamp = -1, lastBody = null, visStart = 0;
  let lastQ = '', lastMeta = '', lastInk = '';

  const clamp = (v, a, b) => (v < a ? a : v > b ? b : v);
  const ease = (t) => 1 - Math.pow(1 - t, 3);
  const mix = (a, b, t) => a + (b - a) * t;
  const mixc = (a, b, t) => [mix(a[0], b[0], t), mix(a[1], b[1], t), mix(a[2], b[2], t)];
  const app = (cur, tgt, dt, tau) => cur + (tgt - cur) * (1 - Math.exp(-dt / tau));
  const esc = (s) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  const fmt = (ms) => Math.floor(ms / 60000) + ':' + String(Math.floor(ms / 1000) % 60).padStart(2, '0');
  const rgba = (c, a) => 'rgba(' + (c[0] | 0) + ',' + (c[1] | 0) + ',' + (c[2] | 0) + ',' + a.toFixed(3) + ')';

  function resize() {
    W = window.innerWidth; H = window.innerHeight; canvasTop = H - FIELD_H;
    const dpr = Math.min(1.5, window.devicePixelRatio || 1);
    cv.width = Math.round(W * dpr); cv.height = Math.round(FIELD_H * dpr);
    cv.style.width = W + 'px'; cv.style.height = FIELD_H + 'px';
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    capRect = null;
  }

  /* One separable soft field: vertical profile x horizontal envelope, blitted to the band. */
  function blit(stops, dstY, dstH, halfFrac, comp, alpha) {
    if (alpha <= 0.002 || dstH <= 0) return;
    octx.setTransform(1, 0, 0, 1, 0, 0);
    octx.globalCompositeOperation = 'source-over';
    octx.clearRect(0, 0, OW, OH);
    const g = octx.createLinearGradient(0, 0, 0, OH);
    for (let i = 0; i < stops.length; i++) g.addColorStop(stops[i][0], stops[i][1]);
    octx.fillStyle = g; octx.fillRect(0, 0, OW, OH);
    octx.globalCompositeOperation = 'destination-in';
    const hw = clamp(halfFrac, 0.03, 0.5);
    const l = 0.5 - hw, r = 0.5 + hw, soft = hw * 0.5;
    const hg = octx.createLinearGradient(0, 0, OW, 0);
    hg.addColorStop(0, 'rgba(0,0,0,0)');
    hg.addColorStop(clamp(l, 0, 1), 'rgba(0,0,0,0)');
    hg.addColorStop(clamp(l + soft * 0.5, 0, 1), 'rgba(0,0,0,0.55)');
    hg.addColorStop(0.5, 'rgba(0,0,0,1)');
    hg.addColorStop(clamp(r - soft * 0.5, 0, 1), 'rgba(0,0,0,0.55)');
    hg.addColorStop(clamp(r, 0, 1), 'rgba(0,0,0,0)');
    hg.addColorStop(1, 'rgba(0,0,0,0)');
    octx.fillStyle = hg; octx.fillRect(0, 0, OW, OH);
    ctx.globalCompositeOperation = comp;
    ctx.globalAlpha = clamp(alpha, 0, 1);
    ctx.drawImage(off, 0, dstY, W, dstH);
    ctx.globalAlpha = 1;
    ctx.globalCompositeOperation = 'source-over';
  }

  // ------------------------------------------------------------------ caption
  function bodyText(s) {
    if (s.phase === 'error') return s.reason || 'session failed';
    if (s.phase === 'answering' && s.llmState) return '';
    if (s.phase === 'done' && !s.success && !s.text) return 'no speech detected';
    return s.text;
  }

  function paint(t, from, animFrom, committedLen) {
    let out = from > 0 ? '<span class="c">… </span>' : '';
    const staticEnd = Math.max(from, animFrom);
    const stat = t.slice(from, staticEnd);
    if (stat) {
      const split = clamp(committedLen - from, 0, stat.length);
      if (split > 0) out += '<span class="c">' + esc(stat.slice(0, split)) + '</span>';
      if (split < stat.length) out += '<span class="d">' + esc(stat.slice(split)) + '</span>';
    }
    let k = 0;
    for (let i = staticEnd; i < t.length; i++) {
      const cls = i < committedLen ? 'c' : 'd';
      out += '<span class="' + cls + ' in" style="animation-delay:' + Math.min(150, k * 16) + 'ms">'
        + esc(t[i]) + '</span>';
      k += 1;
    }
    return out;
  }

  function layout(s, t, animate) {
    const committedLen = t === s.text ? clamp(s.committedLen, 0, t.length) : t.length;
    let animFrom = t.length;
    if (animate) {
      animFrom = clamp(s.changed[0], 0, t.length);
      if (t.length - animFrom > 200) animFrom = t.length - 200; // never re-animate the whole screen
    }
    if (visStart > t.length) visStart = 0;
    for (let guard = 0; guard < 48; guard++) {
      capEl.innerHTML = paint(t, visStart, Math.max(animFrom, visStart), committedLen);
      if (capEl.offsetHeight <= MAX_LINES * LINE_H + 1) break;
      const nxt = t.indexOf(' ', visStart + 1);
      if (nxt < 0 || nxt > t.length - 8) break;
      visStart = nxt + 1;
    }
    capRect = null;
  }

  // ------------------------------------------------------------------ variant
  OverlayProto.register('e', {
    name: 'Ambient',
    mount(r) {
      root = r;
      root.innerHTML = [
        '<style>',
        '.variant-e .field { position:absolute; left:0; bottom:0; }',
        '.variant-e .wrap { position:absolute; left:50%; bottom:52px; transform:translateX(-50%);',
        '  width:960px; opacity:0; font-family:"Fira Sans","Lato","Noto Sans",sans-serif;',
        '  color:#EAF1F8; -webkit-font-smoothing:antialiased; }',
        '.variant-e .q { font-size:13px; line-height:1.4; margin-bottom:7px; letter-spacing:.012em;',
        '  color:rgba(184,210,238,.52); white-space:nowrap; overflow:hidden; text-overflow:ellipsis;',
        '  text-shadow:0 1px 2px rgba(0,0,0,.5); display:none; }',
        '.variant-e .clip { height:' + MAX_LINES * LINE_H + 'px; overflow:hidden; display:flex; align-items:flex-end; }',
        '.variant-e .cap { width:100%; font-size:17px; line-height:' + LINE_H + 'px; font-weight:400;',
        '  white-space:pre-wrap; word-break:break-word; text-shadow:0 1px 2px rgba(0,0,0,.62); }',
        '.variant-e .cap .c { opacity:.97; }',
        '.variant-e .cap .d { opacity:.55; }',
        '.variant-e .cap .in { animation:ve-in .1s linear both; }',
        '@keyframes ve-in { from { opacity:0; } }',
        '.variant-e .meta { position:absolute; right:0; bottom:-20px; font-size:12px; letter-spacing:.05em;',
        '  color:#FF6A55; opacity:.6; font-variant-numeric:tabular-nums; display:none; }',
        '</style>',
        '<canvas class="field"></canvas>',
        '<div class="wrap"><div class="q"></div><div class="clip"><div class="cap"></div></div>',
        '<div class="meta"></div></div>',
      ].join('\n');
      cv = root.querySelector('.field');
      ctx = cv.getContext('2d');
      off = document.createElement('canvas'); off.width = OW; off.height = OH;
      octx = off.getContext('2d');
      wrap = root.querySelector('.wrap');
      qEl = root.querySelector('.q');
      clipEl = root.querySelector('.clip');
      capEl = root.querySelector('.cap');
      metaEl = root.querySelector('.meta');
      this._onResize = resize;
      window.addEventListener('resize', this._onResize);
      resize();
    },

    render(s, now) {
      const dt = clamp(now - (last || now - 16), 1, 64);
      last = now;
      if (s.pressedAt !== sessionKey) {
        sessionKey = s.pressedAt; visStart = 0; lastBody = null; textStamp = -1; capRect = null;
      }
      const p = s.phase;
      const t0 = now - s.phaseAt;

      // ---- caption (rebuilt only when the text actually changes)
      const body = bodyText(s);
      if (body !== lastBody || s.lastUpdateAt !== textStamp) {
        layout(s, body, body === s.text && body.length > 0);
        lastBody = body; textStamp = s.lastUpdateAt;
      }
      const showQ = !!s.question && (p === 'answering' || (p === 'done' && s.mode === 'llm'));
      const qTxt = showQ ? s.question : '';
      if (qTxt !== lastQ) { qEl.textContent = qTxt; qEl.style.display = showQ ? 'block' : 'none'; lastQ = qTxt; }
      const metaTxt = s.warning && p !== 'hidden' ? fmt(s.sessionMs) + ' / ' + fmt(s.capMs) : '';
      if (metaTxt !== lastMeta) { metaEl.textContent = metaTxt; metaEl.style.display = metaTxt ? 'block' : 'none'; lastMeta = metaTxt; }

      // ---- field targets: one number per property, per phase
      let tIn = 0, tH = fh, tHalf = half, tWarm = 0, tEm = 0, tau = 90, tCap = 1;
      if (p === 'listening' || p === 'interim') {
        const n = clamp((s.levelDb + 60) / 54, 0, 1);
        lvl = app(lvl, n, dt, n > lvl ? 45 : 150);
        const g = Math.pow(lvl, 1.25);
        const e = now - s.pressedAt;
        const arrive = e < 110 ? ease(e / 110) : Math.max(0, 1 - ease(clamp((e - 110) / 230, 0, 1)));
        const up = Math.min(1, e / 90);
        tIn = (0.15 + 0.55 * g) * up + 0.26 * arrive;
        tH = (84 + 205 * g) * up + 95 * arrive;
        tHalf = 0.46; tau = 55;
      } else if (p === 'finalizing') {
        const k = ease(clamp(t0 / 200, 0, 1));
        tHalf = mix(0.46, 0.085, k);
        tIn = mix(0.32, 0.58, k) + 0.10 * clamp(t0 / 900, 0, 1);
        tH = mix(160, 215, k);
        tWarm = s.mode === 'llm' ? 0 : 0; tau = 70;
      } else if (p === 'answering') {
        tWarm = 1; tHalf = 0.34; tau = 120;
        if (s.llmState) {
          tIn = 0.27 + 0.045 * Math.sin((t0 / 1000) * 2 * Math.PI * 0.35);
          tH = 145;
        } else {
          const pulse = Math.max(0, 1 - (now - s.lastUpdateAt) / 260);
          tIn = 0.28 + 0.15 * pulse; tH = 150 + 26 * pulse;
        }
      } else if (p === 'done') {
        const k = clamp(t0 / 240, 0, 1);
        tWarm = s.mode === 'llm' ? 1 : 0; tau = 60;
        if (s.success) {
          tHalf = mix(half, 0.52, ease(k));
          tIn = 0.60 * Math.pow(1 - k, 1.6);
          tH = mix(205, 125, ease(k));
        } else {
          tHalf = 0.26; tIn = 0.15 * (1 - k); tH = mix(145, 0, ease(k));
        }
        tCap = 1 - ease(clamp((t0 - 280) / 200, 0, 1));
      } else if (p === 'error') {
        tEm = 1; tHalf = 0.5; tau = 55;
        tIn = 0.64 * ease(clamp(t0 / 120, 0, 1)) * Math.exp(-Math.max(0, t0 - 120) / 950);
        tH = 235 - 80 * clamp(t0 / 1300, 0, 1);
      } else { // hidden: exit runs here
        tIn = 0; tH = fh * 0.82; tCap = 0; tau = 150;
      }

      inten = app(inten, tIn, dt, tau);
      fh = app(fh, tH, dt, tau);
      half = app(half, tHalf, dt, tau);
      warm = app(warm, tWarm, dt, 130);
      ember = app(ember, tEm, dt, 70);
      capA = app(capA, tCap, dt, p === 'hidden' ? 130 : 70);
      wrap.style.opacity = capA.toFixed(3);

      const ink = ember > 0.5 ? '#FFD9D2' : (warm > 0.5 ? '#FBF0E0' : '#EAF1F8');
      if (ink !== lastInk) { wrap.style.color = ink; lastInk = ink; }

      // ---- draw
      ctx.clearRect(0, 0, W, FIELD_H);
      if (inten < 0.004 && capA < 0.01) return;
      const col = mixc(mixc(DAY, LAMP, warm), EMBER, ember);

      // shelf: our own soft darkening, only where words are, so light desktops stay legible
      if (capA > 0.02 && (body.length > 0 || qTxt)) {
        if (!capRect) capRect = wrap.getBoundingClientRect();
        const top = clamp(capRect.top - canvasTop - 34, 0, FIELD_H - 20);
        blit([[0, 'rgba(' + SHADE + ',0)'], [0.30, 'rgba(' + SHADE + ',0.44)'],
             [0.72, 'rgba(' + SHADE + ',0.62)'], [1, 'rgba(' + SHADE + ',0.40)']],
             top, FIELD_H - top, 0.38, 'source-over', capA * 0.95);
      }

      // the field itself
      const h = clamp(fh, 0, FIELD_H);
      blit([[0, rgba(col, 0)], [0.45, rgba(col, 0.10)], [0.72, rgba(col, 0.30)],
           [0.90, rgba(col, 0.66)], [1, rgba(col, 1)]],
           FIELD_H - h, h, half, 'lighter', inten);

      // sill: 2.5 px of hard light on the very edge, so the field is anchored, not floating
      const sillA = clamp(inten * 1.45, 0, 0.92);
      if (sillA > 0.01) {
        const hw = clamp(half, 0.03, 0.5), cx = W / 2;
        const sg = ctx.createLinearGradient(cx - hw * W, 0, cx + hw * W, 0);
        sg.addColorStop(0, rgba(col, 0)); sg.addColorStop(0.22, rgba(col, sillA * 0.75));
        sg.addColorStop(0.5, rgba(col, sillA)); sg.addColorStop(0.78, rgba(col, sillA * 0.75));
        sg.addColorStop(1, rgba(col, 0));
        ctx.globalCompositeOperation = 'lighter';
        ctx.fillStyle = sg;
        ctx.fillRect(cx - hw * W, FIELD_H - 2.5, hw * 2 * W, 2.5);
        if (s.warning && (p === 'listening' || p === 'interim')) {
          const x = cx + hw * W * 0.58, w = 190;
          const wg = ctx.createLinearGradient(x - w / 2, 0, x + w / 2, 0);
          wg.addColorStop(0, rgba(EMBER, 0)); wg.addColorStop(0.5, rgba(EMBER, 0.5)); wg.addColorStop(1, rgba(EMBER, 0));
          ctx.fillStyle = wg;
          ctx.fillRect(x - w / 2, FIELD_H - 2.5, w, 2.5);
        }
        ctx.globalCompositeOperation = 'source-over';
      }
    },

    unmount() {
      window.removeEventListener('resize', this._onResize);
      last = 0; lvl = 0; inten = 0; fh = 120; half = 0.46; warm = 0; ember = 0; capA = 0;
      sessionKey = -1; textStamp = -1; lastBody = null; visStart = 0; lastQ = ''; lastMeta = ''; lastInk = '';
    },
  });
})();
