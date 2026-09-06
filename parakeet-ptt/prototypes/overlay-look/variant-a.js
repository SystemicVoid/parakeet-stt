/* PROTOTYPE (throwaway). Variant A — "SUBTITLE".
 *
 * BOLD BET: there is no container. The transcript is set straight onto the desktop
 * like a film subtitle, held legible by a soft dark halo rasterized from a blurred
 * copy of the glyph mask. Nothing else on screen but the words and one hairline.
 *
 * PALETTE (4, each with a job)
 *   #F2EFE8 ivory  — your words (warm subtitle white, never pure #fff)
 *   #06060A ink    — the halo/substrate; the only reason a frameless design survives a white doc
 *   #E9B44C amber  — the machine's own light: level mark, seal hairline, warning, error mark
 *   #8FB8CE slate  — the second voice: the LLM answer and its rule. Never used for your words.
 *
 * TYPE  Fira Sans Medium 500 / 27px / 36px leading, centered (you).
 *       TeX Gyre Pagella Italic 400 / 25px / 34px, left-aligned with a 2px slate rule (the model).
 *       Fira Sans 400 / 14px, +.05em caption slot (question echo, time left, error reason).
 *
 * SILHOUETTE  A centered text column (max 960px, 4 lines, older lines clipped and
 *   masked off at the TOP so the newest words always survive) sitting on ONE hairline
 *   at 32px from the screen bottom. The hairline is the state machine.
 *
 * STATES AT A GLANCE (form/motion first, hue second)
 *   listening  short amber dash, breathing with level. No word, no text block.
 *   speaking   same dash, wide and alive; words stack above it.
 *   sealing    the dash stops breathing, jumps to the left edge of the text and draws
 *              rightward at seal speed; the text tightens (scale .994, halo pulls in,
 *              draft tail firms to full ivory). The transcript NEVER says "finalizing".
 *   done ok    hairline completes the full text width, holds 260ms, then everything
 *              lifts 11px and dissolves in 240ms.
 *   done empty no lift: the mark contracts to a dot, caption reads "nothing heard".
 *   error      the hairline BREAKS into two segments, the transcript dims to 42%,
 *              the reason sits in the caption in amber. Static where listening breathes.
 *   llm        left-aligned slate italic serif behind a rule — a different silhouette,
 *              not just a different colour; your question echoes small above it.
 *
 * A BURST LANDS  appended chars fade 0->1 over 140ms with a 16ms stagger while their
 *   halo pulls from a 44px diffuse blur to the tight one: words come into focus rather
 *   than typing. Draft tail is dimmed ivory, not a different weight (no reflow).
 *
 * FOR THE RUST IMPLEMENTER  Bundle Fira Sans Medium + Pagella Italic + Fira Sans Regular.
 *   Rasterize the whole text layer once per change into an A8 mask, box-blur it 3x at
 *   r=2/5/12 into a black layer at .95/.75/.45, composite under the glyphs; per-char alpha
 *   is a multiply on the mask. Everything else is 3 rounded rects. Only the hairline and
 *   the per-char alpha ramp change per frame, so 60fps costs one small memcpy plus a rect.
 *   Vertical alpha ramp on the top 30px of the text layer gives the top-clip fade.
 *
 * RISK  On a busy light backdrop the halo is doing all the work; if the blur is cheapened
 *   in Rust the whole direction collapses. Budget the 3-pass blur before anything else.
 */
(function () {
  'use strict';

  const CSS = `
.variant-a .wrap{position:absolute;left:50%;bottom:32px;transform:translateX(-50%);
  width:min(960px,calc(100vw - 96px));display:flex;flex-direction:column;align-items:center;
  opacity:0;will-change:opacity,transform}
.variant-a .cap{display:none;margin:0 0 11px;max-width:100%;white-space:nowrap;overflow:hidden;
  text-overflow:ellipsis;font:400 14px/1.2 'Fira Sans','Noto Sans','DejaVu Sans',sans-serif;
  letter-spacing:.05em;color:rgba(242,239,232,.52);
  text-shadow:0 0 2px rgba(6,6,10,.95),0 0 6px rgba(6,6,10,.82),0 0 15px rgba(6,6,10,.55)}
.variant-a .cap.on{display:block}
.variant-a .cap.alert{color:#E9B44C}
.variant-a .text{display:flex;justify-content:center;align-items:flex-end;width:100%;
  max-height:144px;overflow:hidden;margin-bottom:15px}
.variant-a .text.clip{-webkit-mask-image:linear-gradient(to bottom,rgba(0,0,0,0) 0,#000 30px);
  mask-image:linear-gradient(to bottom,rgba(0,0,0,0) 0,#000 30px)}
.variant-a .inner{max-width:100%;text-align:center;letter-spacing:.004em;color:#F2EFE8;
  font:500 27px/36px 'Fira Sans','Noto Sans','DejaVu Sans',sans-serif;
  transform-origin:50% 100%;transition:transform .22s cubic-bezier(.2,.75,.25,1)}
.variant-a .inner.llm{text-align:left;letter-spacing:0;color:#8FB8CE;padding-left:18px;
  border-left:2px solid rgba(143,184,206,.7);
  font:italic 400 25px/34px 'TeX Gyre Pagella','URW Palladio L','Bitstream Charter','Noto Serif',serif}
.variant-a .inner.seal{transform:scale(.994)}
.variant-a .inner.err{color:rgba(242,239,232,.42)}
.variant-a .pre,.variant-a .ch{
  text-shadow:0 0 2px rgba(6,6,10,.98),0 0 5px rgba(6,6,10,.92),0 0 12px rgba(6,6,10,.72),0 0 26px rgba(6,6,10,.44);
  transition:text-shadow .2s ease-out}
.variant-a .inner.seal .pre,.variant-a .inner.seal .ch{
  text-shadow:0 0 1px rgba(6,6,10,.98),0 0 4px rgba(6,6,10,.9),0 0 9px rgba(6,6,10,.6),0 0 18px rgba(6,6,10,.3)}
.variant-a .d{color:rgba(242,239,232,.55);transition:color .2s ease-out}
.variant-a .inner.llm .d{color:rgba(143,184,206,.58)}
.variant-a .inner.seal .d{color:#F2EFE8}
.variant-a .inner.err .d{color:rgba(242,239,232,.3)}
.variant-a .ch{animation:va-settle 140ms ease-out backwards}
@keyframes va-settle{from{opacity:0;
  text-shadow:0 0 7px rgba(6,6,10,.45),0 0 16px rgba(6,6,10,.4),0 0 30px rgba(6,6,10,.3),0 0 44px rgba(6,6,10,.2)}
  to{opacity:1}}
.variant-a .line{position:relative;width:100%;height:3px}
.variant-a .line i{position:absolute;left:0;bottom:0;height:3px;border-radius:2px;background:#E9B44C;
  box-shadow:0 0 5px 1px rgba(6,6,10,.72),0 0 12px rgba(233,180,76,.26)}
.variant-a .line i.b2{display:none}
.variant-a .line i.on{display:block}
`;

  const AMBER = '#E9B44C';
  const SLATE = '#8FB8CE';
  const MARK_MIN = 16;
  const MARK_MAX = 164;

  let root, wrap, cap, textBox, inner, b1, b2;
  let lvl = 0, lastT = 0, wrapW = 900;
  let sealP = 0, sealX = 0, sealW = 0, sealSet = false;
  let lastText = null, lastCommitted = -1, lastVoice = null;
  let prevPhase = 'hidden', prevPress = -1, enterAt = 0, hiddenAt = 0, visAtHide = 0, vis = 0;

  const clamp = (v, a, b) => (v < a ? a : v > b ? b : v);
  const eIn = (t) => 1 - Math.pow(1 - t, 3);

  function measureWrap() { if (wrap) wrapW = wrap.getBoundingClientRect().width || 900; }

  function reset() {
    lvl = 0; sealP = 0; sealSet = false; sealW = 0; sealX = 0;
    hiddenAt = 0; visAtHide = 0;
    lastText = null; lastCommitted = -1; lastVoice = null;
    inner.textContent = ''; inner.className = 'inner';
    textBox.classList.remove('clip');
    cap.className = 'cap';
    b2.classList.remove('on');
  }

  function buildText(s, isAnswer) {
    const text = s.text || '';
    const committed = clamp(s.committedLen, 0, text.length);
    const chStart = clamp((s.changed && s.changed[0]) || 0, 0, text.length);
    const split = Math.min(chStart, committed);
    const frag = document.createDocumentFragment();
    if (split > 0) {
      const p = document.createElement('span');
      p.className = 'pre';
      p.textContent = text.slice(0, split);
      frag.appendChild(p);
    }
    for (let i = split; i < text.length; i++) {
      const c = document.createElement('span');
      c.className = i >= committed ? 'ch d' : 'ch';
      c.textContent = text[i];
      c.style.animationDelay = (i >= chStart ? Math.min(280, (i - chStart) * 16) : 0) + 'ms';
      frag.appendChild(c);
    }
    inner.textContent = '';
    inner.appendChild(frag);
    inner.classList.toggle('llm', !!isAnswer);
    textBox.classList.toggle('clip', inner.offsetHeight > textBox.clientHeight + 1);
  }

  function timeLeft(s) {
    const r = Math.max(0, (s.capMs - s.sessionMs) / 1000);
    const m = Math.floor(r / 60);
    const sec = Math.floor(r % 60);
    return m + ':' + String(sec).padStart(2, '0') + ' left';
  }

  function setCap(txt, alert) {
    const want = txt || '';
    if (cap.dataset.v !== want) { cap.textContent = want; cap.dataset.v = want; }
    cap.classList.toggle('on', !!want);
    cap.classList.toggle('alert', !!alert);
  }

  function placeBar(el, x, w, color, alpha) {
    el.style.transform = 'translateX(' + x.toFixed(1) + 'px)';
    el.style.width = Math.max(3, w).toFixed(1) + 'px';
    el.style.background = color;
    el.style.opacity = alpha.toFixed(3);
  }

  OverlayProto.register('a', {
    name: 'Subtitle',

    mount(r) {
      root = r;
      const st = document.createElement('style');
      st.textContent = CSS;
      root.appendChild(st);
      wrap = document.createElement('div');
      wrap.className = 'wrap';
      wrap.innerHTML =
        '<div class="cap"></div>' +
        '<div class="text"><div class="inner"></div></div>' +
        '<div class="line"><i class="b1 on"></i><i class="b2"></i></div>';
      root.appendChild(wrap);
      cap = wrap.querySelector('.cap');
      textBox = wrap.querySelector('.text');
      inner = wrap.querySelector('.inner');
      b1 = wrap.querySelector('.b1');
      b2 = wrap.querySelector('.b2');
      measureWrap();
      window.addEventListener('resize', measureWrap);
      reset();
    },

    render(s, now) {
      const dt = lastT ? Math.min(0.064, (now - lastT) / 1000) : 0.016;
      lastT = now;

      if (s.pressedAt !== prevPress) { prevPress = s.pressedAt; enterAt = now; reset(); }
      if (s.phase !== prevPhase) {
        if (prevPhase === 'hidden') enterAt = now;
        if (s.phase === 'hidden') { hiddenAt = now; visAtHide = vis; }
        prevPhase = s.phase;
      }

      const p = s.phase;
      const isAnswer = s.mode === 'llm' && !!s.question;

      /* ---- visibility: entrance <=160ms, lift+dissolve <=240ms ---- */
      let lift = 0;
      if (p === 'hidden') {
        vis = visAtHide * (1 - clamp((now - hiddenAt) / 200, 0, 1));
      } else if (p === 'done') {
        const t = now - s.phaseAt;
        const e = clamp((t - 260) / 240, 0, 1);
        vis = 1 - eIn(e);
        if (s.success && s.text) lift = -11 * eIn(e);
      } else {
        vis = 1;
      }
      vis *= eIn(clamp((now - enterAt) / 160, 0, 1));
      wrap.style.opacity = vis.toFixed(3);
      wrap.style.transform = 'translateX(-50%) translateY(' + lift.toFixed(2) + 'px)';
      if (vis <= 0.002) { b2.classList.remove('on'); return; }

      /* ---- text ---- */
      if (s.text !== lastText || s.committedLen !== lastCommitted || isAnswer !== lastVoice) {
        lastText = s.text; lastCommitted = s.committedLen; lastVoice = isAnswer;
        buildText(s, isAnswer);
      }
      const sealing = p === 'finalizing' || (p === 'done' && s.success);
      inner.classList.toggle('seal', sealing);
      inner.classList.toggle('err', p === 'error');

      /* ---- caption slot ---- */
      if (p === 'error') setCap(s.reason || 'session failed', true);
      else if (p === 'done' && !s.success) setCap('nothing heard', false);
      else if (s.warning) setCap(timeLeft(s), true);
      else if (isAnswer && s.question) setCap(s.question, false);
      else setCap('', false);

      /* ---- the hairline: the state machine ---- */
      const W = wrapW;
      b2.classList.toggle('on', p === 'error');

      if (p === 'listening' || p === 'interim') {
        const target = Math.pow(clamp((s.levelDb + 60) / 54, 0, 1), 0.85);
        const k = target > lvl ? 26 : 7;
        lvl += (target - lvl) * (1 - Math.exp(-k * dt));
        const w = MARK_MIN + MARK_MAX * lvl;
        placeBar(b1, (W - w) / 2, w, AMBER, 0.5 + 0.5 * lvl);
      } else if (p === 'error') {
        const g = eIn(clamp((now - s.phaseAt) / 160, 0, 1)) * 20;
        placeBar(b1, W / 2 - g / 2 - 32, 32, AMBER, 0.9);
        placeBar(b2, W / 2 + g / 2, 32, AMBER, 0.9);
      } else if (p === 'answering') {
        if (s.llmState) {
          const g = (Math.sin(((now - s.phaseAt) / 1800) * 2 * Math.PI) + 1) / 2;
          placeBar(b1, (W - 90) / 2 + (g - 0.5) * 200, 90, SLATE, 0.75);
        } else {
          placeBar(b1, (W - 90) / 2, 90, SLATE, 0.5);
        }
      } else {
        /* finalizing / done: the seal draws under the text at seal speed */
        if (!sealSet) {
          const ir = inner.getBoundingClientRect();
          const wr = wrap.getBoundingClientRect();
          sealW = ir.width;
          sealX = ir.left - wr.left;
          sealSet = true;
          sealP = 0;
        }
        const color = isAnswer ? SLATE : AMBER;
        if (!s.text || sealW < 8) {
          const w = p === 'done' ? 4 + 12 * (1 - eIn(clamp((now - s.phaseAt) / 200, 0, 1))) : MARK_MIN;
          placeBar(b1, (W - w) / 2, w, color, 0.85);
        } else {
          const tau = p === 'done' ? 0.07 : 0.34;
          sealP += (1 - sealP) * (1 - Math.exp(-dt / tau));
          if (p === 'done') {
            const back = eIn(clamp((now - s.phaseAt - 260) / 240, 0, 1));
            const w = sealW * sealP * (1 - 0.55 * back);
            placeBar(b1, sealX + (sealW * sealP - w) / 2, w, color, 1);
          } else {
            placeBar(b1, sealX, Math.max(4, sealW * (0.03 + 0.97 * sealP)), color, 1);
          }
        }
      }
    },

    unmount() {
      window.removeEventListener('resize', measureWrap);
    },
  });
})();
