/* PROTOTYPE (throwaway). Variant B -- "TAPE".
 *
 * BOLD BET: the overlay is not an object on the screen, it is the screen's bottom edge
 *   becoming a transport. A full-bleed strip flush to the edge (no margin, radius, shadow),
 *   40 px of solid ink fading out upward over 22 px so it bleeds off rather than bounding a
 *   panel. Three cells, no dividers, alignment does the work: scope | tape | truth.
 *   This is the only direction that puts runtime truth on screen (elapsed, mode, seal ms).
 *
 * PALETTE (4, each with one job)
 *   ink   #0C0E11  the scrim only. Never a border, never a fill.
 *   paper #E6E9EC  text. .95 committed, .55 revisable draft, .42 meta.
 *   live  #57D08E  the wire is hot: scope trace, write head, success rule.
 *   amber #F0A93B  attention: seal sweep, warning clock, error (error is told apart by FORM --
 *                  the flatline breaks into dashes and the reason replaces the mode cell).
 *
 * TYPE (deliberate pair, both installed locally, both OFL)
 *   Fira Mono 400/500 -- transcript 14 px, meta 11 px uppercase +.09em. Mono because the tape
 *     must not reflow as the draft tail is revised, and because char width is a known constant
 *     (see burst, below). Fira Sans 400 15px/1.35 -- the model's voice, only ever the model's.
 *
 * SILHOUETTE: 1920 x 62 (40 px strip + 22 px ramp). Scope 124 px at x=24. Tape flexes. Truth right-aligned at x=-24.
 *   Text is RIGHT-anchored and overflows to the LEFT under a 90 px mask -- the newest words
 *   are pinned next to the write head; the oldest dissolve off the left. Never wraps.
 *
 * READS AT A GLANCE (form/motion first, hue second)
 *   listening  live trace, near-flat, scrolling left at 38 px/s. Head lit. Tape empty.
 *   speaking   same trace, 6-10 px tall. Words arriving at the head.
 *   sealing    trace COLLAPSES to a dead flat line (120 ms) while an amber rule sweeps the
 *              full 1920 px. Vertical motion stops, horizontal motion starts. Text stays put.
 *   done       rule snaps solid green full width, truth cell adds SEAL 284 MS, exit in 200 ms.
 *   empty      no green: NO SPEECH in the tape, amber rule at half strength.
 *   error      dashed amber flatline, text at 45 %, reason replaces the mode cell.
 *   warning    the clock turns amber. Nothing else moves.
 *   ASK        mode cell reads ASK; on seal the question stays on the tape (mono, dimmed) and
 *              the ink field UNROLLS UPWARD to hold up to 3 lines of Fira Sans. Different face,
 *              different alignment (left, you read it), different growth axis. Not your words.
 *
 * A BURST LANDS: mono means added_px = added_chars * charW exactly. The tape starts at
 *   translateX(+dx) (old words still where the eye left them, new words hidden behind the right
 *   clip) and eases to 0 over 190 ms -- the words are pulled in from the right, not stamped.
 *   The appended range fades in per char, 90 ms each, 14 ms stagger clamped to 200 ms total.
 *
 * FOR THE RUST IMPLEMENTER: bundle FiraMono-Regular/Medium + FiraSans-Regular. Shapes: one
 *   rect with a vertical alpha ramp (top 22 px), one 1 px rule rect, one 2 px head rect, one
 *   stroked polyline (124 samples, 1.4 px, round join) -- no shadows, no blurs, no radii, so a
 *   damage rect of 1920x62 at 60 fps is a memset plus ~1500 glyph-cache blits worst case.
 *   The scope is the only per-frame redraw; the glyph run only changes on a burst. Cache the
 *   laid-out run and translate it; do not re-shape per frame.
 * RISK: a strip on the bottom edge is one styling mistake away from reading as a taskbar. It
 *   survives on: no top border, a soft fading top edge, no icons, and a 2-15 s life. If the
 *   user runs a bottom dock, flip the anchor to top-center -- the design mirrors exactly.
 */
(function () {
  'use strict';

  const INK = '12,14,17';
  const PAPER = '230,233,236';
  const LIVE = '87,208,142';
  const AMBER = '240,169,59';

  const SCOPE_W = 124, SCOPE_H = 26, N = 64, MAXCH = 220;

  let root, tape, fill, canvas, ctx, txtInner, lineEl, head, mClock, mSeal, mMode, ansWrap, ansInner;
  let charW = 8.4;
  let amp = 0, lastFrame = 0;
  let lastText = null, lastCommitted = -1, lastDim = null;
  let lastAnsText = null, lastAnsH = 0;

  const clamp = (v, a, b) => (v < a ? a : v > b ? b : v);
  const esc = (c) => (c === '&' ? '&amp;' : c === '<' ? '&lt;' : c === '>' ? '&gt;' : c);

  function clock(ms) {
    // Round to tenths before the minute split so 59.95 s reads 01:00.0, not 00:60.0.
    const tenths = Math.round(Math.max(0, ms) / 100);
    const m = Math.floor(tenths / 600);
    const r = (tenths - m * 600) / 10;
    return String(m).padStart(2, '0') + ':' + (r < 10 ? '0' : '') + r.toFixed(1);
  }

  function paintLine(text, committedLen, changedStart, dimAll) {
    const t = text.length > MAXCH ? text.slice(text.length - MAXCH) : text;
    const off = text.length - t.length;
    const cl = clamp(committedLen - off, 0, t.length);
    const cs = clamp(changedStart - off, 0, t.length);
    let html = '';
    for (let i = 0; i < t.length; i++) {
      const cls = (dimAll ? 'x' : i < cl ? 'c' : 'd');
      if (i >= cs && !dimAll) {
        const d = Math.min((i - cs) * 14, 200);
        html += '<span class="' + cls + ' f" style="animation-delay:' + d + 'ms">' + esc(t[i]) + '</span>';
      } else {
        html += '<span class="' + cls + '">' + esc(t[i]) + '</span>';
      }
    }
    lineEl.innerHTML = html;
    return t.length - cs;
  }

  function slide(px) {
    if (px <= 0) return;
    const dx = Math.min(px, 150);
    txtInner.style.transition = 'none';
    txtInner.style.transform = 'translateX(' + dx + 'px)';
    void txtInner.offsetWidth;
    txtInner.style.transition = 'transform .19s cubic-bezier(.22,.61,.36,1)';
    txtInner.style.transform = 'translateX(0)';
  }

  function drawScope(s, now, dt) {
    const liveMic = s.phase === 'listening' || s.phase === 'interim';
    const k = 1 - Math.exp(-dt / 45);
    amp += ((liveMic ? 1 : 0) - amp) * k;
    ctx.clearRect(0, 0, SCOPE_W, SCOPE_H);
    const cy = SCOPE_H / 2, half = SCOPE_H / 2 - 1.5;

    if (amp < 0.02) {
      // dead flat line: the mic is closed. Form, not hue, carries this.
      ctx.setLineDash(s.phase === 'error' ? [3, 4] : []);
      ctx.strokeStyle = s.phase === 'error'
        ? 'rgba(' + AMBER + ',.85)'
        : (s.phase === 'done' && s.success)
          ? 'rgba(' + LIVE + ',.9)'
          : 'rgba(' + PAPER + ',.34)';
      ctx.lineWidth = 1.4;
      ctx.beginPath();
      ctx.moveTo(0.5, cy);
      ctx.lineTo(SCOPE_W - 0.5, cy);
      ctx.stroke();
      ctx.setLineDash([]);
      return;
    }

    const base = now / 50;
    ctx.setLineDash([]);
    ctx.strokeStyle = 'rgba(' + LIVE + ',' + (0.55 + 0.4 * amp).toFixed(3) + ')';
    ctx.lineWidth = 1.4;
    ctx.lineJoin = 'round';
    ctx.lineCap = 'round';
    ctx.beginPath();
    for (let x = 0; x <= SCOPE_W; x++) {
      const t = (x / SCOPE_W) * (N - 1);
      const i0 = Math.floor(t), f = t - i0;
      const a = s.levels[i0] === undefined ? -60 : s.levels[i0];
      const b = s.levels[Math.min(N - 1, i0 + 1)] === undefined ? -60 : s.levels[Math.min(N - 1, i0 + 1)];
      const db = a + (b - a) * f;
      let env = clamp((db + 60) / 54, 0, 1);
      env = Math.min(1, Math.pow(env, 1.4) * 1.25);
      const abs = base - (N - 1 - t);
      const y = cy + amp * env * half * Math.sin(abs * 0.95);
      if (x === 0) ctx.moveTo(x + 0.5, y); else ctx.lineTo(x + 0.5, y);
    }
    ctx.stroke();
  }

  OverlayProto.register('b', {
    name: 'Tape',
    mount(r) {
      root = r;
      root.innerHTML = `
        <style>
          .variant-b .tape {
            position: absolute; left: 0; right: 0; bottom: 0; height: 62px;
            background: linear-gradient(to top,
              rgba(${INK},.94) 0, rgba(${INK},.94) calc(100% - 22px), rgba(${INK},0) 100%);
            opacity: 0; transform: translateY(10px);
            transition: opacity .2s ease-in, transform .2s ease-in, height .18s cubic-bezier(.22,.61,.36,1);
            will-change: opacity, transform;
          }
          .variant-b .tape.on { opacity: 1; transform: translateY(0);
            transition: opacity .18s ease-out, transform .18s cubic-bezier(.22,.61,.36,1), height .18s cubic-bezier(.22,.61,.36,1); }
          .variant-b .rule { position: absolute; left: 0; right: 0; bottom: 40px; height: 1px;
            background: rgba(${PAPER},.10); overflow: hidden; }
          .variant-b .fill { position: absolute; left: 0; top: 0; height: 1px; width: 0; background: rgba(${AMBER},.95); }
          .variant-b .row { position: absolute; left: 0; right: 0; bottom: 0; height: 40px;
            display: flex; align-items: center; padding: 0 24px; box-sizing: border-box; gap: 20px; }
          .variant-b canvas { display: block; width: ${SCOPE_W}px; height: ${SCOPE_H}px; flex: none; }
          .variant-b .txtwrap { flex: 1 1 auto; min-width: 0; display: flex; align-items: center; gap: 7px; }
          .variant-b .txtcell { flex: 1 1 auto; min-width: 0; position: relative; height: 20px; overflow: hidden;
            -webkit-mask-image: linear-gradient(to right, transparent 0, #000 90px);
            mask-image: linear-gradient(to right, transparent 0, #000 90px); }
          .variant-b .inner { position: absolute; right: 0; top: 0; white-space: pre;
            font: 400 14px/20px "Fira Mono", "DejaVu Sans Mono", ui-monospace, monospace;
            letter-spacing: .005em; }
          .variant-b .inner .c { color: rgba(${PAPER},.95); }
          .variant-b .inner .d { color: rgba(${PAPER},.55); }
          .variant-b .inner .x { color: rgba(${PAPER},.45); }
          .variant-b .inner .f { animation: b-in .09s linear backwards; }
          @keyframes b-in { from { opacity: 0 } to { opacity: 1 } }
          .variant-b .note { color: rgba(${AMBER},.85); font: 500 11px/20px "Fira Mono", monospace;
            letter-spacing: .09em; }
          .variant-b .head { flex: none; width: 2px; height: 16px; background: rgba(${LIVE},.9);
            transition: background .12s linear, opacity .12s linear; }
          .variant-b .meta { flex: none; display: flex; align-items: baseline; gap: 14px;
            font: 500 11px/20px "Fira Mono", "DejaVu Sans Mono", monospace;
            letter-spacing: .09em; color: rgba(${PAPER},.42); }
          .variant-b .meta .mode { color: rgba(${PAPER},.70); }
          .variant-b .meta .amber { color: rgba(${AMBER},.95); }
          .variant-b .ans { position: absolute; left: 168px; right: 24px; bottom: 44px; max-width: 960px;
            height: 0; overflow: hidden; opacity: 0;
            transition: height .18s cubic-bezier(.22,.61,.36,1), opacity .16s ease-out; }
          .variant-b .ans.on { opacity: 1; }
          .variant-b .ansinner { font: 400 15px/21px "Fira Sans", "Noto Sans", sans-serif;
            color: rgba(${PAPER},.94); transition: transform .18s cubic-bezier(.22,.61,.36,1); }
          .variant-b .measure { position: absolute; visibility: hidden; white-space: pre;
            font: 400 14px/20px "Fira Mono", "DejaVu Sans Mono", ui-monospace, monospace; letter-spacing: .005em; }
        </style>
        <div class="tape">
          <div class="ans"><div class="ansinner"></div></div>
          <div class="rule"><div class="fill"></div></div>
          <div class="row">
            <canvas></canvas>
            <div class="txtwrap">
              <div class="txtcell"><div class="inner"></div></div>
              <i class="head"></i>
            </div>
            <div class="meta"><span class="clock"></span><span class="seal"></span><span class="mode"></span></div>
          </div>
        </div>
        <span class="measure">MMMMMMMMMMMMMMMMMMMM</span>`;

      tape = root.querySelector('.tape');
      fill = root.querySelector('.fill');
      canvas = root.querySelector('canvas');
      txtInner = root.querySelector('.inner');
      lineEl = txtInner;
      head = root.querySelector('.head');
      mClock = root.querySelector('.meta .clock');
      mSeal = root.querySelector('.meta .seal');
      mMode = root.querySelector('.meta .mode');
      ansWrap = root.querySelector('.ans');
      ansInner = root.querySelector('.ansinner');

      canvas.width = SCOPE_W * 2;
      canvas.height = SCOPE_H * 2;
      ctx = canvas.getContext('2d');
      ctx.setTransform(2, 0, 0, 2, 0, 0);

      const measure = root.querySelector('.measure');
      const remeasure = () => {
        const w = measure.getBoundingClientRect().width;
        if (w > 0) charW = w / 20;
      };
      remeasure();
      if (document.fonts && document.fonts.ready) document.fonts.ready.then(remeasure);
      lastFrame = 0;
    },

    render(s, now) {
      const dt = lastFrame ? Math.min(64, now - lastFrame) : 16;
      lastFrame = now;

      const visible = s.phase !== 'hidden';
      tape.classList.toggle('on', visible);
      if (!visible) {
        lastText = null; lastCommitted = -1; lastAnsText = null;
        drawScope(s, now, dt);
        return;
      }

      const llmAnswer = s.mode === 'llm' && !!s.question && (s.phase === 'answering' || s.phase === 'done');

      // ---- tape line: your words. Right-anchored, oldest dissolve off the left.
      const tapeText = llmAnswer ? s.question : s.text;
      const tapeCommitted = llmAnswer ? s.question.length : s.committedLen;
      const dimAll = llmAnswer || s.phase === 'error';
      const emptyFail = s.phase === 'done' && !s.success && !s.text;

      if (emptyFail) {
        if (lastText !== ' empty') {
          lineEl.innerHTML = '<span class="note">NO SPEECH</span>';
          lastText = ' empty'; lastCommitted = -1; lastDim = null;
          txtInner.style.transform = 'translateX(0)';
        }
      } else if (tapeText !== lastText || tapeCommitted !== lastCommitted || dimAll !== lastDim) {
        const prev = (typeof lastText === 'string' && lastText[0] !== ' ') ? lastText : '';
        let common = 0;
        const max = Math.min(prev.length, tapeText.length);
        while (common < max && prev[common] === tapeText[common]) common += 1;
        const added = paintLine(tapeText, tapeCommitted, dimAll ? tapeText.length : common, dimAll);
        if (!dimAll && added > 0 && prev.length) slide(added * charW);
        lastText = tapeText; lastCommitted = tapeCommitted; lastDim = dimAll;
      }

      // ---- write head: lit while the mic is open, parked amber during the seal.
      if (s.phase === 'listening' || s.phase === 'interim') {
        head.style.background = 'rgba(' + LIVE + ',.9)';
        head.style.opacity = '1';
      } else if (s.phase === 'finalizing') {
        head.style.background = 'rgba(' + AMBER + ',.95)';
        head.style.opacity = '1';
      } else {
        head.style.opacity = '0';
      }

      // ---- the rule: horizontal motion only ever means "sealing / finished".
      let w = 0, col = 'rgba(' + AMBER + ',.95)', x = 0;
      if (s.phase === 'finalizing') {
        w = (1 - Math.exp(-(now - s.phaseAt) / 380)) * 100;
      } else if (s.phase === 'answering' && s.llmState) {
        w = 16; x = (((now - s.phaseAt) / 1200) % 1) * 116 - 16;
      } else if (s.phase === 'done') {
        w = 100;
        col = s.success ? 'rgba(' + LIVE + ',.95)' : 'rgba(' + AMBER + ',.5)';
      } else if (s.phase === 'error') {
        w = 100; col = 'rgba(' + AMBER + ',.75)';
      }
      fill.style.width = w.toFixed(2) + '%';
      fill.style.transform = 'translateX(' + x.toFixed(2) + 'vw)';
      fill.style.background = col;

      // ---- runtime truth.
      mClock.textContent = clock(s.sessionMs);
      mClock.className = 'clock' + (s.warning ? ' amber' : '');
      if (s.phase === 'error') {
        mSeal.textContent = '';
        mMode.textContent = String(s.reason || 'error').toUpperCase();
        mMode.className = 'mode amber';
      } else {
        mSeal.textContent = (s.phase === 'done' && s.sealMs) ? 'SEAL ' + Math.round(s.sealMs) + ' MS' : '';
        mMode.textContent = s.mode === 'llm' ? 'ASK' : 'STT';
        mMode.className = 'mode';
      }

      // ---- the model's voice: a different face, unrolling upward out of the tape.
      const showAns = llmAnswer && (s.text || s.llmState);
      if (showAns) {
        const body = s.llmState || s.text;
        if (body !== lastAnsText) {
          ansInner.textContent = body;
          lastAnsText = body;
        }
        ansWrap.classList.add('on');
        const full = ansInner.scrollHeight;
        const h = Math.min(63, full) + 10;
        if (h !== lastAnsH) {
          ansWrap.style.height = h + 'px';
          tape.style.height = (68 + h) + 'px';
          lastAnsH = h;
        }
        ansInner.style.transform = 'translateY(' + Math.min(0, (h - 10) - full) + 'px)';
      } else if (lastAnsH !== 0) {
        ansWrap.classList.remove('on');
        ansWrap.style.height = '0px';
        tape.style.height = '62px';
        lastAnsH = 0;
        lastAnsText = null;
      }

      drawScope(s, now, dt);
    },

    unmount() {
      lastText = null; lastCommitted = -1; lastAnsText = null; lastAnsH = 0; amp = 0; lastFrame = 0;
    },
  });
})();
