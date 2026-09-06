/* PROTOTYPE (throwaway). Variant D - "Instrument": a small dictation deck.
 *
 * BOLD BET: the level meter is a real instrument, not decoration. A segmented
 *   bargraph with VU ballistics (fast attack + slight overshoot, slow release)
 *   and a latched peak-hold marker. It spans the whole top row and is the only
 *   thing on the deck allowed to move while you speak. Everything else is a
 *   legend: a lamp, a mode word, a tally counter, a readout window.
 *
 * PALETTE (4 roles; the enclosure tints itself for hairlines)
 *   #14161A  enclosure (near-black panel, 96%), inset window #0A0B0C
 *   #EFE3C8  cream  - backlight: readout text, live meter segments, counter
 *   #E8A33D  amber  - latched/derived: peak-hold marker, HOLD lamp, cap warning, ASK rail
 *   #E0322A  red    - armed & wrong: REC lamp, meter hot zone (> -12 dBFS), FAULT, NO TEXT
 *   (The brief's "green-white" done lamp is collapsed into full-bright cream to hold four
 *    colours; done is signalled by FORM - a full-scale meter peg - not by a new hue.)
 *
 * TYPE: one superfamily, deliberate pair. Fira Sans Condensed 600/700 for every panel
 *   legend (10-11 px, uppercase, .11-.14em tracking) and the tally counter (15 px, tnum).
 *   Fira Sans 400 at 14 px / 18 px for the readout. Both installed locally, OFL.
 *
 * LAYOUT / SILHOUETTE: 720 x 120 wide-low deck, bottom-centre, 32 px up, 4 px radius.
 *   Row 1 (instrument strip, 22 px): [LAMP] [MODE] [====== METER ======] [00:07.4]
 *   hairline
 *   Row 2 (display window, inset, 3 lines): the transcript, quiet and second.
 *   No gutter, no accent stripe, no card silhouette. A panel, not a dialog.
 *
 * STATES AT A GLANCE (form/motion first, hue second)
 *   listening: solid RED lamp block, meter idling at the noise floor (~4 segments), counter runs.
 *   speaking : meter swinging 20-26 segments with an amber peak marker riding ahead. Only motion.
 *   sealing  : meter falls to rest (ballistic decay), lamp flips to solid AMBER "HOLD", the
 *              transcript STAYS on screen (baseline bug fixed), amber hairline sweeps the base.
 *   done     : meter pegs full-scale in cream and falls in ~450 ms, lamp bright cream "PASTED".
 *              A full bar cannot be confused with any speech level.
 *   empty    : no peg, dead meter, red "NO TEXT", window reads NO SPEECH CAPTURED.
 *   error    : red "FAULT" lamp, red rail + reason in the annotation row, partial text dimmed.
 *   llm      : mode legend reads ASK, amber rail down the window, your question demoted to the
 *              small uppercase annotation row, the model's answer is the big cream readout.
 *
 * BURST LANDING: the appended range fades in per character (110 ms, stagger capped so any burst
 *   finishes within ~140 ms), the draft tail past committedLen sits at 62 % alpha, and the
 *   readout eases up 180 ms when a burst pushes a new line. No typewriter.
 *
 * FOR THE RUST IMPLEMENTER: bundle FiraSansCondensed-SemiBold/Bold + FiraSans-Regular (3 faces).
 *   Every shape is an axis-aligned rounded rect (r <= 4) or a 1 px hairline - no SDF needed.
 *   Meter is ~43 rects/frame; redraw only the strip rect. Text layout changes only on a burst,
 *   so cache the shaped glyph run and re-blit with per-glyph alpha for the fade. Ballistics and
 *   peak-hold are two floats per frame. 60 fps only while listening/interim; 20 fps elsewhere.
 *   The window's top fade is a 12 px vertical alpha ramp over our own pixels.
 *
 * RISK: 43 hard-edged segments in a 438 px strip can moire on a fractional-scale Wayland output.
 *   Snap the segment pitch to whole device pixels; never scale the meter bitmap.
 */
(function () {
  'use strict';

  const CREAM = '239,227,200';
  const AMBER = '232,163,61';
  const RED = '224,50,42';

  const DECK_W = 720;
  const SEG_PITCH = 10; // 6 px lit + 4 px gap, snapped
  const SEG_W = 6;
  const SEG_H = 16;
  const LINE_H = 18;
  const LINES = 3;

  let root, deck, lamp, lampTxt, modeEl, meterEl, mctx, counterEl;
  let win, annot, textEl, sweep;
  let segCount = 0, meterW = 0;

  // meter ballistics
  let cur = 0, vel = 0, peak = 0, peakAt = 0, lastFrame = 0;
  let sessionKey = -1;
  let lastText = null, lastUpdate = -1, lastAnnotated = false;

  const clamp = (v, a, b) => (v < a ? a : v > b ? b : v);
  const norm = (db) => clamp((db + 60) / 54, 0, 1);

  function fmtClock(ms) {
    const t = Math.max(0, ms) / 1000;
    const m = Math.floor(t / 60);
    const s = Math.floor(t % 60);
    const d = Math.floor((t * 10) % 10);
    return String(m).padStart(2, '0') + ':' + String(s).padStart(2, '0') + '.' + d;
  }

  function lampSpec(s) {
    switch (s.phase) {
      case 'listening':
      case 'interim': return ['REC', RED];
      case 'finalizing': return ['HOLD', AMBER];
      case 'answering': return ['ANSWER', AMBER];
      case 'error': return ['FAULT', RED];
      case 'done': return s.success ? ['PASTED', CREAM] : ['NO TEXT', RED];
      default: return ['REC', RED];
    }
  }

  function resetSession() {
    cur = 0; vel = 0; peak = 0; peakAt = 0;
    lastText = null; lastUpdate = -1; lastAnnotated = false;
    textEl.style.transform = 'translateY(0px)';
    win.classList.remove('annotated', 'ask', 'fault', 'scrolled');
    annot.textContent = '';
  }

  /** Keep the newest lines in the window; truncate from the start. */
  function applyScroll() {
    const visibleLines = win.classList.contains('annotated') ? LINES - 1 : LINES;
    const shift = Math.max(0, textEl.scrollHeight - visibleLines * LINE_H);
    textEl.style.transform = 'translateY(' + -shift + 'px)';
    win.classList.toggle('scrolled', shift > 1);
  }

  // ---- readout ----------------------------------------------------------
  const esc = (t) => t.replace(/&/g, '&amp;').replace(/</g, '&lt;');

  function buildText(s) {
    const text = s.text;
    const start = clamp(s.changed[0], 0, text.length);
    const committed = clamp(s.committedLen, 0, text.length);
    let html = '';
    if (start > 0) {
      const stable = text.slice(0, Math.min(committed, start));
      const draft = text.slice(Math.min(committed, start), start);
      if (stable) html += '<span class="st">' + esc(stable) + '</span>';
      if (draft) html += '<span class="dr">' + esc(draft) + '</span>';
    }
    const tail = text.slice(start);
    const stagger = tail.length ? Math.min(16, 140 / tail.length) : 16;
    for (let i = 0; i < tail.length; i++) {
      const ch = tail[i];
      const isDraft = start + i >= committed;
      const glyph = ch === ' ' ? '&nbsp;' : esc(ch);
      html += '<span class="ink' + (isDraft ? ' dr' : '') + '" style="animation-delay:' +
        (i * stagger).toFixed(1) + 'ms">' + glyph + '</span>';
    }
    return html;
  }

  function legend(word, cursor) {
    if (lastText === ' ' + word) return;
    lastText = ' ' + word;
    textEl.innerHTML = '<span class="legend">' + word + (cursor ? '<b class="cursor"></b>' : '') + '</span>';
    textEl.style.transform = 'translateY(0px)';
    win.classList.remove('scrolled');
  }

  function setReadout(s) {
    const llmAnnot = s.mode === 'llm' && s.question &&
      (s.phase === 'answering' || s.phase === 'done' || s.phase === 'finalizing');

    if (s.phase === 'error') {
      win.classList.add('annotated', 'fault');
      win.classList.remove('ask');
      if (annot.textContent !== (s.reason || '')) annot.textContent = s.reason || 'fault';
    } else if (llmAnnot) {
      win.classList.add('annotated', 'ask');
      win.classList.remove('fault');
      if (annot.textContent !== s.question) annot.textContent = s.question;
    } else if (win.classList.contains('annotated')) {
      win.classList.remove('annotated', 'ask', 'fault');
      annot.textContent = '';
    }

    if (s.phase === 'listening' && !s.text) return legend('pre-roll 2.5 s captured', false);
    if (s.phase === 'answering' && s.llmState) return legend('generating answer', true);
    if (!s.text && (s.phase === 'finalizing' || s.phase === 'done')) return legend('no speech captured', false);
    if (!s.text) return;

    const annotated = win.classList.contains('annotated');
    if (s.text === lastText && s.lastUpdateAt === lastUpdate) {
      // the annotation row stole a line: re-seat the readout without rebuilding it
      if (annotated !== lastAnnotated) { lastAnnotated = annotated; applyScroll(); }
      return;
    }
    lastText = s.text;
    lastUpdate = s.lastUpdateAt;
    lastAnnotated = annotated;
    textEl.innerHTML = buildText(s);
    applyScroll();
  }

  // ---- meter ------------------------------------------------------------
  function drawMeter(s, now, powerOn) {
    const dt = lastFrame ? clamp(now - lastFrame, 1, 100) : 16;
    lastFrame = now;
    const dtn = dt / 16.7;

    const live = s.phase === 'listening' || s.phase === 'interim';
    let target = live ? norm(s.levelDb) : 0;

    let pegged = false;
    if (s.phase === 'done' && s.success) {
      // full-scale peg on paste, falling back over ~450 ms
      const p = clamp((now - s.phaseAt) / 450, 0, 1);
      cur = 1 - p * p;
      vel = 0;
      pegged = true;
    } else {
      // VU ballistics: fast attack with a touch of overshoot, slow release
      const k = (target > cur ? 0.30 : 0.105) * dtn;
      vel += (target - cur) * k;
      vel *= Math.pow(0.66, dtn);
      cur = clamp(cur + vel * dtn, 0, 1.04);
    }

    if (live && cur > peak) { peak = cur; peakAt = now; }
    if (!live && peak > cur) peak = Math.max(cur, peak - 1.4 * (dt / 1000));
    if (now - peakAt > 650) peak = Math.max(0, peak - 0.85 * (dt / 1000));

    mctx.clearRect(0, 0, meterW, SEG_H);
    const lit = Math.round(clamp(cur, 0, 1) * segCount);
    const hotFrom = Math.floor(segCount * 0.82);
    const peakIdx = peak > 0.05 ? clamp(Math.round(peak * segCount) - 1, 0, segCount - 1) : -1;
    const gate = powerOn < 1 ? Math.ceil(powerOn * segCount) : segCount;

    for (let i = 0; i < gate; i++) {
      let fill;
      if (i === peakIdx && !pegged) {
        fill = 'rgba(' + AMBER + ',.95)';
      } else if (i < lit) {
        fill = pegged
          ? 'rgba(' + CREAM + ',.95)'
          : (i >= hotFrom ? 'rgba(' + RED + ',.95)' : 'rgba(' + CREAM + ',.86)');
      } else {
        fill = i >= hotFrom ? 'rgba(' + RED + ',.16)' : 'rgba(' + CREAM + ',.10)';
      }
      mctx.fillStyle = fill;
      mctx.fillRect(i * SEG_PITCH, 0, SEG_W, SEG_H);
    }
  }

  OverlayProto.register('d', {
    name: 'Instrument',

    mount(r) {
      root = r;
      root.innerHTML = `
        <style>
          .variant-d .deck {
            position: absolute; bottom: 32px; left: 50%;
            width: ${DECK_W}px; box-sizing: border-box;
            padding: 9px 14px 10px; border-radius: 4px;
            background: rgba(20,22,26,.96);
            box-shadow: 0 1px 0 rgba(${CREAM},.06) inset,
                        0 0 0 1px rgba(0,0,0,.4),
                        0 8px 26px rgba(0,0,0,.46);
            font-family: "Fira Sans Condensed", "Fira Sans", sans-serif;
            opacity: 0; transform: translate(-50%, 6px) scale(.994);
            transition: opacity .16s linear, transform .18s cubic-bezier(.2,.7,.3,1);
          }
          .variant-d .deck.on { opacity: 1; transform: translate(-50%, 0) scale(1); }
          .variant-d .deck.off {
            opacity: 0; transform: translate(-50%, 4px) scale(.996);
            transition: opacity .2s linear, transform .2s ease-in;
          }

          .variant-d .strip { display: flex; align-items: center; height: 22px; gap: 10px; }
          .variant-d .lamp {
            width: 66px; height: 18px; border-radius: 2px;
            display: flex; align-items: center; justify-content: center;
            font-size: 10px; font-weight: 700; letter-spacing: .11em; text-transform: uppercase;
            color: #14161A; background: rgb(${RED}); transition: background .12s linear;
          }
          .variant-d .mode {
            width: 34px; font-size: 10px; font-weight: 600; letter-spacing: .13em;
            text-transform: uppercase; color: rgba(${CREAM},.42);
          }
          .variant-d canvas.meter { display: block; height: ${SEG_H}px; flex: 0 0 auto; }
          .variant-d .tally { margin-left: auto; display: flex; align-items: center; gap: 8px; }
          .variant-d .cap {
            font-size: 9.5px; font-weight: 700; letter-spacing: .12em; text-transform: uppercase;
            color: rgb(${AMBER}); opacity: 0; transition: opacity .2s linear;
          }
          .variant-d .deck.warn .cap { opacity: 1; }
          .variant-d .clock {
            font-size: 15px; font-weight: 600; color: rgba(${CREAM},.88);
            font-variant-numeric: tabular-nums; font-feature-settings: "tnum" 1;
            min-width: 60px; text-align: right;
          }
          .variant-d .deck.warn .clock { color: rgb(${AMBER}); }

          .variant-d .hr { height: 1px; background: rgba(${CREAM},.09); margin: 8px 0 7px; }

          .variant-d .win {
            position: relative; overflow: hidden; border-radius: 2px;
            background: #0A0B0C; box-shadow: 0 0 0 1px rgba(${CREAM},.055) inset;
            padding: 6px 12px; box-sizing: border-box;
            height: ${LINES * LINE_H + 12}px;
          }
          .variant-d .win.ask, .variant-d .win.fault { padding-left: 14px; }
          .variant-d .win .rail {
            position: absolute; left: 0; top: 0; bottom: 0; width: 2px;
            background: rgb(${AMBER}); opacity: 0; transition: opacity .14s linear;
          }
          .variant-d .win.ask .rail { opacity: 1; }
          .variant-d .win.fault .rail { opacity: 1; background: rgb(${RED}); }

          .variant-d .annot {
            display: none; font-size: 10.5px; font-weight: 600; letter-spacing: .1em;
            text-transform: uppercase; color: rgba(${CREAM},.40); line-height: 14px;
            white-space: nowrap; overflow: hidden; text-overflow: ellipsis; margin-bottom: 4px;
          }
          .variant-d .win.annotated .annot { display: block; }
          .variant-d .win.fault .annot { color: rgb(${RED}); }

          .variant-d .twrap { overflow: hidden; height: ${LINES * LINE_H}px; }
          .variant-d .win.annotated .twrap { height: ${(LINES - 1) * LINE_H}px; }
          .variant-d .win.scrolled .twrap {
            -webkit-mask-image: linear-gradient(180deg, transparent 0, #000 12px);
            mask-image: linear-gradient(180deg, transparent 0, #000 12px);
          }
          .variant-d .txt {
            font-family: "Fira Sans", sans-serif; font-size: 14px; line-height: ${LINE_H}px;
            font-weight: 400; color: rgba(${CREAM},.94); word-break: break-word;
            transform: translateY(0); transition: transform .18s cubic-bezier(.2,.7,.3,1);
          }
          .variant-d .deck.sealing .txt { color: rgba(${CREAM},.80); }
          .variant-d .txt .dr { opacity: .62; }
          .variant-d .txt .ink { animation: vd-in 110ms linear both; }
          .variant-d .txt .ink.dr { animation-name: vd-in-dr; }
          @keyframes vd-in { from { opacity: 0 } to { opacity: 1 } }
          @keyframes vd-in-dr { from { opacity: 0 } to { opacity: .62 } }
          .variant-d .txt .legend {
            font-family: "Fira Sans Condensed", sans-serif; font-size: 11px; font-weight: 600;
            letter-spacing: .14em; text-transform: uppercase; color: rgba(${CREAM},.34);
          }
          .variant-d .txt .cursor {
            display: inline-block; width: 6px; height: 11px; margin-left: 7px;
            background: rgb(${AMBER}); vertical-align: -1px;
            animation: vd-blink 1.1s steps(1) infinite;
          }
          @keyframes vd-blink { 50% { opacity: 0 } }

          .variant-d .sweep {
            position: absolute; left: 0; bottom: 0; height: 2px; width: 30%;
            background: linear-gradient(90deg, transparent, rgb(${AMBER}), transparent);
            opacity: 0;
          }
          .variant-d .deck.sealing .sweep { opacity: 1; }
        </style>
        <div class="deck">
          <div class="strip">
            <div class="lamp"><span>REC</span></div>
            <div class="mode">DICT</div>
            <canvas class="meter"></canvas>
            <div class="tally"><span class="cap">cap 80%</span><span class="clock">00:00.0</span></div>
          </div>
          <div class="hr"></div>
          <div class="win">
            <div class="rail"></div>
            <div class="annot"></div>
            <div class="twrap"><div class="txt"></div></div>
            <div class="sweep"></div>
          </div>
        </div>`;

      deck = root.querySelector('.deck');
      lamp = root.querySelector('.lamp');
      lampTxt = lamp.querySelector('span');
      modeEl = root.querySelector('.mode');
      meterEl = root.querySelector('canvas.meter');
      counterEl = root.querySelector('.clock');
      win = root.querySelector('.win');
      annot = root.querySelector('.annot');
      textEl = root.querySelector('.txt');
      sweep = root.querySelector('.sweep');

      // the meter takes whatever the strip has left, snapped to whole segment pitches
      const avail = DECK_W - 28 - 66 - 34 - 20 - 84 - 8;
      segCount = Math.floor((avail + SEG_PITCH - SEG_W) / SEG_PITCH);
      meterW = segCount * SEG_PITCH - (SEG_PITCH - SEG_W);
      meterEl.style.width = meterW + 'px';
      const dpr = Math.max(1, Math.round(window.devicePixelRatio || 1));
      meterEl.width = meterW * dpr;
      meterEl.height = SEG_H * dpr;
      mctx = meterEl.getContext('2d');
      mctx.scale(dpr, dpr);
    },

    render(s, now) {
      const hidden = s.phase === 'hidden';
      if (hidden && now - s.phaseAt > 320) {
        deck.classList.remove('on', 'off', 'sealing', 'warn');
        lastFrame = 0;
        return;
      }
      if (s.pressedAt !== sessionKey) { sessionKey = s.pressedAt; resetSession(); }

      deck.classList.toggle('on', !hidden);
      deck.classList.toggle('off', hidden);
      deck.classList.toggle('sealing', s.phase === 'finalizing');
      deck.classList.toggle('warn', !!s.warning);

      const spec = lampSpec(s);
      if (lampTxt.textContent !== spec[0]) lampTxt.textContent = spec[0];
      lamp.style.background = 'rgb(' + spec[1] + ')';

      const modeWord = s.mode === 'llm' ? 'ASK' : 'DICT';
      if (modeEl.textContent !== modeWord) modeEl.textContent = modeWord;
      modeEl.style.color = s.mode === 'llm' ? 'rgba(' + AMBER + ',.85)' : 'rgba(' + CREAM + ',.42)';

      counterEl.textContent = fmtClock(s.sessionMs);

      // power-on segment sweep: the scale lights left to right over 170 ms from the press
      drawMeter(s, now, clamp((now - s.pressedAt) / 170, 0, 1));
      setReadout(s);

      if (s.phase === 'finalizing') {
        const p = ((now - s.phaseAt) / 900) % 1;
        sweep.style.transform = 'translateX(' + (p * 430 - 100) + '%)';
      }
    },

    unmount() { lastFrame = 0; sessionKey = -1; },
  });
})();
