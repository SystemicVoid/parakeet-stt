/* PROTOTYPE (throwaway). Variant Z: a plain reproduction of the current overlay for comparison.
 * Mirrors overlay_renderer.rs constants: 22,22,26 card @ 90 %, 12 px radius, 1 px border,
 * 72 px left gutter, Sans 16, 4 fixed lines, teal pixel meter bottom-left, accent stripe. */
(function () {
  let root, card, textEl, stripe, meter, ctx;
  const PHRASES = ['Listening', 'Go ahead', 'Ready when you are', 'Speak freely', "I'm all ears", 'Say the word'];
  const ACCENT = { listening: 'rgb(66,133,244)', interim: 'rgb(52,199,137)', finalizing: 'rgb(255,179,64)' };
  OverlayProto.register('z', {
    name: 'Baseline (current)',
    mount(r) {
      root = r;
      root.innerHTML = `
        <style>
          .variant-z .card { position: absolute; bottom: 32px; left: 50%; transform: translateX(-50%);
            min-width: 240px; max-width: 960px; height: ${16 * 1.45 * 4 + 32}px; background: rgba(22,22,26,.9);
            border: 1px solid rgb(58,58,68); border-radius: 12px; box-shadow: 0 4px 8px rgba(0,0,0,.31);
            padding: 16px 24px 16px 72px; box-sizing: border-box; color: rgb(245,245,250);
            font: 16px/1.45 "DejaVu Sans", sans-serif; text-shadow: 0 1px 0 rgba(0,0,0,.24);
            opacity: 0; transition: opacity .25s ease-out, transform .25s ease-out; transform: translate(-50%, 5px); }
          .variant-z .card.on { opacity: 1; transform: translate(-50%, 0); }
          .variant-z .stripe { position: absolute; left: 4px; top: 8px; bottom: 8px; width: 3px; border-radius: 2px; }
          .variant-z canvas { position: absolute; left: 4px; bottom: 6px; image-rendering: pixelated; }
          .variant-z .bar { position: absolute; left: 0; right: 0; bottom: 0; height: 2px; }
        </style>
        <div class="card"><div class="stripe"></div><canvas width="60" height="72"></canvas><span class="txt"></span><div class="bar"></div></div>`;
      card = root.querySelector('.card');
      textEl = root.querySelector('.txt');
      stripe = root.querySelector('.stripe');
      meter = root.querySelector('canvas');
      ctx = meter.getContext('2d');
    },
    render(s, now) {
      const visible = s.phase !== 'hidden';
      card.classList.toggle('on', visible);
      if (!visible) return;
      if (s.phase === 'listening') {
        const i = Math.floor((now - s.phaseAt) / 3000) % PHRASES.length;
        const dots = '.'.repeat(1 + Math.floor((now / 300) % 3));
        textEl.textContent = PHRASES[i] + dots;
      } else if (s.phase === 'answering' && s.llmState) {
        textEl.textContent = s.llmState;
      } else {
        textEl.textContent = s.text || (s.phase === 'error' ? s.reason : 'Finalizing...');
      }
      const showMeter = s.phase === 'listening' || s.phase === 'interim';
      meter.style.display = showMeter ? 'block' : 'none';
      stripe.style.display = showMeter ? 'none' : 'block';
      stripe.style.background = s.warning ? 'rgb(255,165,0)' : (ACCENT[s.phase] || ACCENT.finalizing);
      if (s.phase === 'done' && s.success) stripe.style.background = 'rgb(80,220,120)';
      if (showMeter) {
        ctx.clearRect(0, 0, 60, 72);
        ctx.fillStyle = 'rgb(0,220,210)';
        const cols = 20, h = 24;
        for (let c = 0; c < cols; c++) {
          const db = s.levels[s.levels.length - cols + c] ?? -60;
          const lvl = Math.max(0, Math.min(1, (db + 60) / 55)) * 1.2;
          const rows = Math.round(lvl * h);
          for (let r = 0; r < rows; r++) {
            if (r > rows * 0.65 && ((r + c) % 2)) continue;
            ctx.fillRect(c * 3, 72 - (r + 1) * 3, 3, 3);
          }
        }
      }
      root.querySelector('.bar').style.background = s.phase === 'finalizing'
        ? `linear-gradient(90deg, transparent ${((now / 15) % 130) - 30}%, rgb(255,179,64) ${((now / 15) % 130)}%, transparent ${((now / 15) % 130) + 5}%)`
        : 'none';
    },
    unmount() {},
  });
})();
