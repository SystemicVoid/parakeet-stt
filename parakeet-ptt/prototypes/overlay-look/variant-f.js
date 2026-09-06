/* PROTOTYPE (throwaway). Variant F — "Galley" (the refined direction, built from feedback on A–E).
 *
 * WHAT IT KEEPS
 *   From C: the paper sheet, Newsreader roman + true italic, pencil draft tail that "sets" into
 *           ink when a burst commits, the question / rule / answer split for LLM mode.
 *   From B: a live instrument at the left edge, a running counter, one tiny colour indicator.
 *   From D: the confirmation vocabulary. One short word says what the machine is doing:
 *           REC · ASK · DECODING · ANSWER · PASTED · NO TEXT · FAILED · FAULT.
 *
 * WHAT IT DROPS
 *   The ink blot (weak, and it looked like a stain). D's dark deck. E's shade. B's raw scope.
 *
 * THE INSTRUMENT COLUMN (left, 22..180 px)
 *   A 60 px square voice visualiser, and beside it a three-line stack in Fira Code:
 *   [• REC] the lamp word with its colour dot, [0:07.4] the session counter (counts up;
 *   turns to remaining time in rubric past the 80 % cap warning), and an aux line (seal
 *   time after paste). Prose is serif; the machine speaks in monospace. Two families, no more.
 *
 * THE VISUALISER: the coil (chosen). One continuous wire wound eight turns along a horizontal
 * axis. Its radius at each point is the mic level from the last two seconds, oldest at the left,
 * newest at the right, so a phrase travels through the spring as a swell. It turns slowly, which
 * makes it screw along the axis like a spring rolled in the fingers. Silence is a fine, even
 * spring at rest. DECODING freezes the last shape (the utterance as an object); PASTED lets it
 * fade. Errors draw it in rubric, the model's answer in slate.
 *   Alternate, kept for reference (`v` or ?vis=harmonograph): a two-pendulum drawing machine
 *   whose swing is your voice. Tried and cut: a vertical lathe (too much girth), the lathe on
 *   its side (a whisk), onset ripples (a bullseye), plucked strings, a brush ribbon (a smudge).
 *
 * PALETTE (paper + 3 inks + 3 state inks, all muted so they sit *in* the paper)
 *   #f4efe5 paper · #1a1712 ink · #8c8377 pencil · #a33a22 rubric (trouble)
 *   #5f8f6c sage (live, pasted) · #c2953f ochre (seal) · #5f7ea6 slate (the model)
 *
 * FOR THE RUST IMPLEMENTER
 *   Faces: Newsreader Regular + Italic (OFL; static instances at opsz 16 via fonttools
 *   instancer), Fira Code Regular (already on the machine). Shapes: rounded rect r=3 + shadow,
 *   1 px rules, a 6 px disc, and one 60x60 canvas of anti-aliased 1 px polylines (one
 *   420-segment polyline per frame for the coil). Text: per-glyph (alpha, dy, face) exactly as C. Height is content-driven with an
 *   88 px floor so the instrument column always fits.
 */
(function () {
  'use strict';

  const W = 920, PAD_L = 180, PAD_R = 40, PAD_T = 18, PAD_B = 14, MIN_H = 88;
  const FS = 17, LH = 25.5, MAXL = 4, RULE_GAP = 14;
  const MEASURE = W - PAD_L - PAD_R;
  const INK = '#1a1712', PENCIL = '#8c8377', RUBRIC = '#a33a22';
  const SAGE = '#5f8f6c', OCHRE = '#c2953f', SLATE = '#5f7ea6';
  const VIS_X = 22, VIS_Y = 14, VIS_D = 60, COL_X = VIS_X + VIS_D + 12, COL_Y = 24;
  const VIS_KINDS = ['coil', 'harmonograph'];

  let root, sheet, qEl, inkEl, trackEl, metaEl, txtEl, qbar, visEl, vctx, dotEl, wordEl, timerEl, auxEl;
  let chars = [], offset = 0, curWord = null, ellEl = null;
  let prevText = ' ', prevCommitted = -1, prevRoman = null;
  let timers = new Set(), gen = 0;
  let curLines = 1, prevPhase = 'hidden', needIn = 0, lastNow = 0, sealStartMs = 0, sealTook = 0;

  let curKey = 'f', lastKey = null;
  const vis = { kind: 'coil', E: 0, t: 0, trail: [], rot: 0, fade: 1, color: INK, frozen: null };

  const clamp = (v, a, b) => (v < a ? a : v > b ? b : v);
  const norm = (db) => clamp((db + 55) / 30, 0, 1); // owner's mic: noise ≈ -55, speech ≈ -32
  const fmtUp = (ms) => {
    const s = Math.max(0, ms) / 1000, m = Math.floor(s / 60), r = s - m * 60;
    return m + ':' + (r < 10 ? '0' : '') + r.toFixed(1);
  };
  const fmtDown = (ms) => {
    const s = Math.max(0, Math.ceil(ms / 1000));
    return '-' + Math.floor(s / 60) + ':' + String(s % 60).padStart(2, '0');
  };
  const rgb = (hex) => [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16)).join(',');

  // ---------------------------------------------------------------- text (from C)
  function clearTimers() { gen += 1; timers.forEach(clearTimeout); timers.clear(); }

  function resetText() {
    clearTimers();
    chars = []; offset = 0; curWord = null; ellEl = null;
    txtEl.textContent = '';
    prevText = ''; prevCommitted = -1;
  }

  function appendChar(ch, roman, delay) {
    if (ch === ' ' || ch === '\n') {
      const node = document.createTextNode(' ');
      txtEl.appendChild(node);
      curWord = null;
      chars.push({ ch, el: null, node, w: null, draft: !roman });
      return;
    }
    if (!curWord || !curWord.isConnected) {
      curWord = document.createElement('span');
      curWord.className = 'w';
      txtEl.appendChild(curWord);
    }
    const el = document.createElement('span');
    el.className = roman ? 'c a' : 'c d a';
    el.textContent = ch;
    el.style.animationDelay = delay + 'ms';
    curWord.appendChild(el);
    chars.push({ ch, el, node: el, w: curWord, draft: !roman });
  }

  function dropFrom(k) {
    for (let i = chars.length - 1; i >= k; i--) {
      const c = chars[i];
      if (c.node && c.node.parentNode) c.node.remove();
      if (c.w && !c.w.firstChild) c.w.remove();
    }
    chars.length = Math.max(0, k);
    curWord = null;
    for (let i = chars.length - 1; i >= 0; i--) {
      if (chars[i].w) { curWord = chars[i].w; break; }
      if (chars[i].ch === ' ') break;
    }
  }

  function dropFront(n) {
    for (let i = 0; i < n && i < chars.length; i++) {
      const c = chars[i];
      if (c.node && c.node.parentNode) c.node.remove();
      if (c.w && !c.w.firstChild) c.w.remove();
    }
    chars.splice(0, n);
    offset += n;
    if (curWord && !curWord.isConnected) curWord = null;
    if (!ellEl) {
      ellEl = document.createElement('span');
      ellEl.className = 'ell';
      ellEl.textContent = '… ';
    }
    if (txtEl.firstChild !== ellEl) txtEl.insertBefore(ellEl, txtEl.firstChild);
  }

  function commit(c, delay) {
    if (!c.el) { c.draft = false; return; }
    c.el.style.animationDelay = delay + 'ms';
    c.el.classList.remove('a');
    c.el.classList.add('s');
    const g = gen;
    const id = setTimeout(() => {
      timers.delete(id);
      if (g !== gen || !c.el.isConnected) return;
      c.el.classList.remove('d');
    }, delay + 92);
    timers.add(id);
    c.draft = false;
  }

  function syncText(s) {
    const roman = s.mode === 'llm' && (s.phase === 'answering' || (s.phase === 'done' && !!s.question));
    if (roman !== prevRoman) { prevRoman = roman; resetText(); }
    const text = s.text || '';
    if (text === prevText && s.committedLen === prevCommitted) return;

    let k = 0;
    const m = Math.min(prevText.length, text.length);
    while (k < m && prevText[k] === text[k]) k += 1;
    if (k < offset) { resetText(); k = 0; } else { dropFrom(k - offset); }
    for (let i = k; i < text.length; i++) appendChar(text[i], roman, Math.min(560, (i - k) * 16));

    const cl = s.committedLen - offset;
    let first = -1;
    for (let i = 0; i < chars.length && i < cl; i++) {
      if (!chars[i].draft) continue;
      if (first < 0) first = i;
      commit(chars[i], Math.min(420, (i - first) * 10));
    }

    const maxH = MAXL * LH + 1;
    let guard = 0;
    while (txtEl.scrollHeight > maxH && chars.length > 4 && guard++ < 400) {
      let n = 1;
      while (n < chars.length - 3 && chars[n].ch !== ' ') n += 1;
      dropFront(n + 1);
    }
    const lines = clamp(Math.round(txtEl.scrollHeight / LH), 1, MAXL);
    if (lines !== curLines) { curLines = lines; txtEl.style.height = lines * LH + 'px'; }
    prevText = text;
    prevCommitted = s.committedLen;
  }

  // ---------------------------------------------------------------- visualisers
  function resetVis() {
    vis.E = 0; vis.t = Math.random() * 40; vis.trail = []; vis.fade = 1; vis.frozen = null;
    vis.rings = null; vis.str = null;
  }

  function drawHarmonograph(ctx, s, now, dt, live, sealing) {
    const target = live ? Math.pow(norm(s.levelDb), 0.9) : 0;
    vis.E += (target - vis.E) * (target > vis.E ? 0.3 : sealing ? 0.14 : 0.035);
    const A = 2.6 + (VIS_D / 2 - 5.5) * vis.E;
    const speed = sealing ? 0.45 : 0.85 + 2.3 * vis.E;
    const sub = 3;
    for (let k = 0; k < sub; k++) {
      vis.t += (Math.min(dt, 50) / 1000) * speed / sub;
      const t = vis.t;
      const x = A * (0.62 * Math.sin(4.4 * t + 0.3) + 0.38 * Math.sin(6.62 * t + 1.9));
      const y = A * (0.62 * Math.sin(6.6 * t + 1.1) + 0.38 * Math.sin(4.415 * t + 0.4));
      vis.trail.push(x, y);
    }
    while (vis.trail.length > 2 * 300) vis.trail.splice(0, 2);
    const n = vis.trail.length / 2, cx = VIS_D / 2, cy = VIS_D / 2;
    ctx.strokeStyle = vis.color;
    ctx.lineCap = 'round'; ctx.lineJoin = 'round';
    for (let i = 1; i < n; i++) {
      const f = i / n;
      ctx.globalAlpha = Math.pow(f, 1.8) * 0.92 * vis.fade;
      ctx.lineWidth = 0.75 + 0.65 * f;
      ctx.beginPath();
      ctx.moveTo(cx + vis.trail[2 * i - 2], cy + vis.trail[2 * i - 1]);
      ctx.lineTo(cx + vis.trail[2 * i], cy + vis.trail[2 * i + 1]);
      ctx.stroke();
    }
    ctx.globalAlpha = 1;
  }


  // level history, oldest at the left, newest at the right edge
  function hist(s, N) {
    const src = s.levels, out = [];
    for (let i = 0; i < N; i++) {
      const v = src[src.length - N + i];
      out.push(norm(v == null ? -60 : v));
    }
    const sm = (a) => a.map((v, i) => 0.25 * (a[i - 1] == null ? v : a[i - 1]) + 0.5 * v + 0.25 * (a[i + 1] == null ? v : a[i + 1]));
    return sm(sm(sm(out))); // three passes: a turned form, not a string of syllables
  }

  // coil: one continuous wire wound along the axis; its radius is the level history. Rotation
  // makes it screw along, like a spring being turned in the fingers.
  function drawCoil(ctx, s, now, dt, live, sealing) {
    vis.rot -= (Math.min(dt, 50) / 1000) * (live ? 2.2 : 0.8);
    const N = 40, M = 420, TURNS = 8, R0 = 2.2, SQUASH = 0.22;
    if (live || !vis.frozen) vis.frozen = hist(s, N);
    const prof = vis.frozen;
    const cy = VIS_D / 2, x0 = 4, x1 = VIS_D - 4, RMAX = VIS_D / 2 - 7;
    ctx.strokeStyle = vis.color;
    ctx.lineCap = 'round';
    let px = 0, py = 0;
    for (let j = 0; j <= M; j++) {
      const u = j / M, fi = u * (N - 1), i0 = Math.floor(fi), f = fi - i0;
      const v = prof[i0] * (1 - f) + (prof[Math.min(N - 1, i0 + 1)]) * f;
      const r = R0 + (RMAX - R0) * Math.pow(v, 0.85);
      const th = vis.rot + u * 2 * Math.PI * TURNS, sn = Math.sin(th), cs = Math.cos(th);
      const x = x0 + u * (x1 - x0) + r * SQUASH * sn, y = cy + r * cs;
      if (j) {
        ctx.globalAlpha = (0.12 + 0.62 * Math.max(0, sn)) * vis.fade;
        ctx.lineWidth = 0.7 + 0.5 * Math.max(0, sn);
        ctx.beginPath(); ctx.moveTo(px, py); ctx.lineTo(x, y); ctx.stroke();
      }
      px = x; py = y;
    }
    ctx.globalAlpha = 1;
  }

  const DRAW = { coil: drawCoil, harmonograph: drawHarmonograph };

  function setVis(kind) {
    vis.kind = kind;
    resetVis();
    const url = new URL(location.href);
    url.searchParams.set('vis', kind);
    history.replaceState(null, '', url);
    const lbl = document.getElementById('proto-variant');
    if (lbl) lbl.textContent = curKey.toUpperCase() + ' · Galley · ' + kind + ' (v)';
  }

  function onKey(e) {
    if (e.key === 'v') setVis(VIS_KINDS[(VIS_KINDS.indexOf(vis.kind) + 1) % VIS_KINDS.length]);
  }

  // ---------------------------------------------------------------- lamp vocabulary (from D)
  function lampSpec(s) {
    switch (s.phase) {
      case 'listening':
      case 'interim': return s.mode === 'llm' ? ['ASK', SLATE, PENCIL] : ['REC', SAGE, PENCIL];
      case 'finalizing': return ['DECODING', OCHRE, PENCIL];
      case 'answering': return ['ANSWER', SLATE, SLATE];
      case 'error': return ['FAULT', RUBRIC, RUBRIC];
      case 'done':
        if (s.success) return ['PASTED', SAGE, SAGE];
        return s.text || s.question ? ['FAILED', RUBRIC, RUBRIC] : ['NO TEXT', RUBRIC, RUBRIC];
      default: return ['REC', SAGE, PENCIL];
    }
  }

  const makeDef = (key, name, defaultVis) => ({
    name,
    mount(r) {
      root = r;
      curKey = key;
      root.classList.add('galley');
      if (!document.getElementById('c-font')) {
        const l = document.createElement('link');
        l.id = 'c-font';
        l.rel = 'stylesheet';
        l.href = 'https://fonts.googleapis.com/css2?family=Newsreader:ital,opsz,wght@0,6..72,400;1,6..72,400&display=swap';
        document.head.appendChild(l);
      }
      root.innerHTML = `
        <style>
          .galley .wrap { position: absolute; bottom: 32px; left: 50%; transform: translateX(-50%); }
          .galley .sheet {
            width: ${W}px; min-height: ${MIN_H}px; box-sizing: border-box;
            padding: ${PAD_T}px ${PAD_R}px ${PAD_B}px ${PAD_L}px;
            display: flex; flex-direction: column; position: relative;
            background: linear-gradient(#f7f2e9, #f0eadf);
            border-radius: 3px;
            box-shadow: 0 18px 38px rgba(18,12,4,.34), 0 2px 6px rgba(18,12,4,.20),
                        inset 0 0 0 1px rgba(26,23,18,.10);
            color: ${INK};
            font-family: 'Newsreader', 'TeX Gyre Pagella', 'Bitstream Charter', Charter, 'Noto Serif', Georgia, serif;
            font-size: ${FS}px; line-height: ${LH}px; font-weight: 400;
            opacity: 0; transform: translateY(7px);
            transition: opacity .14s ease-out, transform .17s cubic-bezier(.2,.8,.2,1);
          }
          .galley .sheet.in { opacity: 1; transform: translateY(0); }
          .galley .sheet.in.out-lift { opacity: 0; transform: translateY(-13px);
            transition: opacity .22s ease-out, transform .22s ease-out; }
          .galley .sheet.in.out-fall { opacity: 0; transform: translateY(9px);
            transition: opacity .2s ease-in, transform .2s ease-in; }

          .galley .inst { position: absolute; left: 0; top: 0; width: ${PAD_L}px; height: 100%; }
          .galley .vis { position: absolute; left: ${VIS_X}px; top: ${VIS_Y}px; width: ${VIS_D}px; height: ${VIS_D}px; }
          .galley .col { position: absolute; left: ${COL_X}px; top: ${COL_Y}px;
            font-family: 'Fira Code', 'Fira Mono', ui-monospace, monospace; color: ${PENCIL};
            font-variant-numeric: tabular-nums; white-space: nowrap; }
          .galley .lamp { font-size: 10.5px; line-height: 14px; letter-spacing: .13em; font-weight: 500; }
          .galley .dot { display: inline-block; width: 6px; height: 6px; border-radius: 50%;
            margin-right: 6px; vertical-align: 1px; background: ${SAGE}; transform-origin: center;
            transition: background .15s linear; }
          .galley .timer { font-size: 11px; line-height: 14px; margin-top: 2px; letter-spacing: .02em; }
          .galley .aux { font-size: 9.5px; line-height: 13px; margin-top: 1px; letter-spacing: .08em;
            opacity: 0; transition: opacity .15s linear; }

          .galley .body { order: 3; position: relative; padding-bottom: ${RULE_GAP}px; }
          .galley .txt { height: ${LH}px; overflow: hidden; transition: height .18s ease-out;
            word-break: normal; overflow-wrap: break-word; }
          .galley .w { display: inline-block; white-space: pre; }
          .galley .c { display: inline-block; color: ${INK}; }
          .galley .c.d { font-style: italic; color: ${PENCIL}; }
          .galley .ell { color: ${PENCIL}; }
          .galley .c.a { animation: f-arrive .11s linear both; }
          .galley .c.s { animation: f-set .21s cubic-bezier(.3,.7,.3,1) both; }
          @keyframes f-arrive { from { opacity: 0 } to { opacity: 1 } }
          @keyframes f-set {
            0%   { opacity: 1;   transform: translateY(0) }
            38%  { opacity: .10; transform: translateY(-1.6px) }
            62%  { opacity: .80; transform: translateY(0) }
            100% { opacity: 1;   transform: translateY(0) }
          }

          .galley .q { order: 1; display: none; font-style: italic; font-size: 13.5px;
            line-height: 20px; color: ${PENCIL}; height: 20px; overflow: hidden; white-space: nowrap;
            text-overflow: ellipsis; }
          .galley .rule { order: 4; position: relative; height: 1px; margin-top: auto; width: ${MEASURE}px; }
          .galley .sheet.llm .q { display: block; }
          .galley .sheet.llm .rule { order: 2; margin: 9px 0 13px; }
          .galley .sheet.llm .body { padding-left: 22px; }
          .galley .track { position: absolute; inset: 0; background: rgba(26,23,18,.14);
            opacity: 0; transition: opacity .12s linear; }
          .galley .ink { position: absolute; left: 0; top: 0; height: 1px; width: 0; background: ${INK}; }
          .galley .meta { position: absolute; right: 0; bottom: 7px; font-size: 12px;
            line-height: 14px; font-variant: small-caps; letter-spacing: .05em; color: ${PENCIL};
            white-space: nowrap; opacity: 0; transition: opacity .15s linear; }
          .galley .qbar { position: absolute; left: 0; top: 0; bottom: ${RULE_GAP}px; width: 1px;
            background: ${SLATE}; transform: scaleY(0); transform-origin: top;
            transition: transform .22s ease-out; display: none; }
          .galley .sheet.llm .qbar { display: block; }
          .galley .sheet.llm.ans .qbar { transform: scaleY(1); }
        </style>
        <div class="wrap"><div class="sheet">
          <div class="inst">
            <canvas class="vis" width="${VIS_D * 2}" height="${VIS_D * 2}"></canvas>
            <div class="col">
              <div class="lamp"><span class="dot"></span><span class="word">REC</span></div>
              <div class="timer">0:00.0</div>
              <div class="aux"></div>
            </div>
          </div>
          <div class="q"></div>
          <div class="body"><div class="qbar"></div><div class="txt"></div></div>
          <div class="rule"><div class="track"></div><div class="ink"></div><div class="meta"></div></div>
        </div></div>`;
      sheet = root.querySelector('.sheet');
      qEl = root.querySelector('.q');
      txtEl = root.querySelector('.txt');
      qbar = root.querySelector('.qbar');
      inkEl = root.querySelector('.ink');
      trackEl = root.querySelector('.track');
      metaEl = root.querySelector('.meta');
      visEl = root.querySelector('.vis');
      vctx = visEl.getContext('2d');
      dotEl = root.querySelector('.dot');
      wordEl = root.querySelector('.word');
      timerEl = root.querySelector('.timer');
      auxEl = root.querySelector('.aux');
      prevPhase = 'hidden';
      prevRoman = null;
      lastNow = performance.now();
      resetText();
      const want = lastKey === null || lastKey === key ? new URLSearchParams(location.search).get('vis') : null;
      lastKey = key;
      setVis(VIS_KINDS.includes(want) ? want : defaultVis);
      setTimeout(() => setVis(vis.kind), 0); // re-apply the label after the harness writes its own
      window.addEventListener('keydown', onKey);
    },

    render(s, now) {
      const dt = now - lastNow;
      lastNow = now;
      const visible = s.phase !== 'hidden';

      if (s.phase !== prevPhase) {
        if (prevPhase === 'hidden' && visible) {
          sheet.classList.remove('out-lift', 'out-fall', 'in', 'llm', 'ans');
          resetText();
          resetVis();
          curLines = 1;
          txtEl.style.height = LH + 'px';
          inkEl.style.width = '0px';
          sealTook = 0;
          needIn = 2;
        } else if (!visible) {
          const fall = s.success === false || !!s.reason;
          sheet.classList.add(fall ? 'out-fall' : 'out-lift');
        }
        if (s.phase === 'finalizing') sealStartMs = now;
        if (prevPhase === 'finalizing' && s.phase !== 'finalizing') sealTook = Math.round(now - sealStartMs);
        if (s.phase === 'answering') sheet.classList.add('llm', 'ans');
        prevPhase = s.phase;
      }
      if (needIn > 0 && visible) {
        needIn -= 1;
        if (needIn === 1) void sheet.offsetHeight;
        else sheet.classList.add('in');
      }
      if (!visible) return;

      syncText(s);

      if (s.mode === 'llm' && s.question) {
        sheet.classList.add('llm');
        if (qEl.textContent !== s.question) qEl.textContent = s.question;
      }

      const live = s.phase === 'listening' || s.phase === 'interim';
      const sealing = s.phase === 'finalizing';
      const answering = s.phase === 'answering' || (s.phase === 'done' && s.mode === 'llm' && s.question);

      // ---- instrument column
      const [word, dotColor, wordColor] = lampSpec(s);
      if (wordEl.textContent !== word) wordEl.textContent = word;
      wordEl.style.color = wordColor;
      dotEl.style.background = dotColor;
      const lv = live ? norm(s.levelDb) : 0;
      const pulse = live ? 0.85 + 0.75 * lv : (s.phase === 'done' && s.success ? 1.25 : 1);
      dotEl.style.transform = `scale(${pulse.toFixed(3)})`;
      dotEl.style.opacity = sealing ? String(0.55 + 0.45 * Math.abs(Math.sin(now / 160))) : '1';

      let timer, timerColor = PENCIL;
      if (s.warning) { timer = fmtDown(s.capMs - s.sessionMs); timerColor = RUBRIC; }
      else timer = fmtUp(s.sessionMs);
      if (timerEl.textContent !== timer) timerEl.textContent = timer;
      timerEl.style.color = timerColor;

      let aux = '';
      if (s.phase === 'done' && sealTook > 0) aux = 'seal ' + sealTook + 'ms';
      if (auxEl.textContent !== aux) auxEl.textContent = aux;
      auxEl.style.opacity = aux ? '1' : '0';

      // ---- visualiser
      vis.color = s.phase === 'error' ? RUBRIC : (answering ? SLATE : INK);
      if (s.phase === 'done' || s.phase === 'error') vis.fade = Math.max(0, vis.fade - dt / 900);
      else vis.fade = 1;
      vctx.setTransform(2, 0, 0, 2, 0, 0);
      vctx.clearRect(0, 0, VIS_D, VIS_D);
      DRAW[vis.kind](vctx, s, now, dt, live, sealing);

      // ---- rule: the measure line, then the seal drawn under the text (text never leaves)
      let inkW = 0, trackOn = 0, ruleColor = INK;
      if (s.phase === 'listening' && !s.text) {
        trackOn = 1;
      } else if (sealing) {
        trackOn = 1;
        inkW = MEASURE * (1 - Math.exp(-Math.max(0, now - s.phaseAt - 110) / 330));
      } else if (s.phase === 'error') {
        trackOn = 1; ruleColor = RUBRIC;
        inkW = MEASURE * (1 - Math.exp(-(now - s.phaseAt) / 110));
      } else if (s.phase === 'done' || answering) {
        trackOn = 1;
        if (s.text || s.question) inkW = MEASURE;
        else { ruleColor = PENCIL; inkW = MEASURE * 0.5; }
      }
      inkEl.style.width = inkW.toFixed(1) + 'px';
      inkEl.style.background = ruleColor;
      trackEl.style.opacity = String(trackOn);

      // ---- meta: only the error reason lives here now (cap and empty moved into the column)
      let meta = '';
      if (s.phase === 'error') meta = s.reason || 'session failed';
      if (metaEl.textContent !== meta) metaEl.textContent = meta;
      metaEl.style.color = RUBRIC;
      metaEl.style.opacity = meta ? '1' : '0';
    },

    unmount() {
      window.removeEventListener('keydown', onKey);
      clearTimers();
      chars = []; offset = 0; curWord = null; ellEl = null;
      prevText = ' '; prevRoman = null;
    },
  });

  OverlayProto.register('f', makeDef('f', 'Galley', 'coil'));
})();
