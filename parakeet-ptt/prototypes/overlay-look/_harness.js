/* PROTOTYPE (throwaway). Shared harness for overlay look-and-feel variants.
 *
 * Plan: five structurally different variants of the session Overlay, switchable via
 * ?variant=<key> (and the top bar / arrow keys), rendered over a mocked desktop.
 * The harness owns the simulation so every variant is driven by the same event
 * stream the real daemon/client produce:
 *   press -> listening (pre-roll already captured)
 *   audio_level at 20 Hz (dBFS; speech ~ -32, room noise ~ -55 on the owner's mic)
 *   interim_text bursts every 2.4 s (chunk_secs) with a revisable draft tail
 *   release -> stop_replay burst (~150 ms) -> finalizing (seal path, 150..900 ms)
 *   -> injection (~120 ms) -> done (success flash) -> hidden (auto-hide 600 ms)
 *   LLM mode: transcript seals, then "Generating answer..." then answer deltas.
 *   session_warning at 80 % of the 600 s cap.
 *
 * Variant contract:
 *   OverlayProto.register('a', {
 *     name: 'Subtitle',
 *     mount(root)          // root is #overlay-root (full-screen, pointer-events none)
 *     render(state, nowMs) // called every animation frame; state is shared, read-only
 *     unmount()
 *   })
 * See STATE_DOC below for the state shape.
 */
(function () {
  'use strict';

  const CAP_MS = 600000;
  const WARN_MS = CAP_MS * 0.8;
  const AUTO_HIDE_MS = 600;
  const CHUNK_MS = 2400;
  const LEVEL_HZ = 20;
  const LEVEL_HISTORY = 64;
  const WORDS_PER_SEC = 2.6;

  const STATE_DOC = {
    phase: "'hidden' | 'listening' | 'interim' | 'finalizing' | 'answering' | 'done' | 'error'",
    mode: "'stt' | 'llm'",
    text: 'transcript so far (stt) or answer so far (llm, during answering/done)',
    question: 'finalized question text in llm mode, else empty',
    committedLen: 'leading chars of text considered stable; the rest is a revisable draft tail',
    changed: '[start, end] char range of the most recent text update',
    lastUpdateAt: 'ms timestamp of the most recent text update',
    levelDb: 'latest audio level in dBFS (-60..0)',
    levels: 'last 64 levels at 20 Hz, oldest first',
    sessionMs: 'elapsed since press; frozen after release',
    capMs: 'hard session cap (600000)',
    warning: 'true once the session passes 80 % of the cap',
    reason: "error reason string when phase === 'error'",
    success: "true/false when phase === 'done'",
    llmState: "'Generating answer...' while waiting for the first delta, else null",
    phaseAt: 'ms timestamp of the last phase change',
    pressedAt: 'ms timestamp of the press',
    releasedAt: 'ms timestamp of the release (0 until released)',
    sealMs: 'how long the seal path took (known once done)',
    prerollMs: 'pre-roll buffer length (2500)',
  };

  const now = () => performance.now();

  function freshState() {
    return {
      phase: 'hidden',
      mode: 'stt',
      text: '',
      question: '',
      committedLen: 0,
      changed: [0, 0],
      lastUpdateAt: 0,
      levelDb: -60,
      levels: new Array(LEVEL_HISTORY).fill(-60),
      sessionMs: 0,
      capMs: CAP_MS,
      warning: false,
      reason: null,
      success: null,
      llmState: null,
      phaseAt: 0,
      pressedAt: 0,
      releasedAt: 0,
      sealMs: 0,
      prerollMs: 2500,
    };
  }

  const state = freshState();
  const registry = new Map();
  const order = [];
  let current = null;
  let currentKey = null;

  // ---------------------------------------------------------------- sim
  let runToken = 0;
  let speaking = false;
  let speakStartedAt = 0;
  let sessionBaseMs = 0;
  let levelTimer = null;

  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  function setPhase(phase) {
    if (state.phase !== phase) {
      state.phase = phase;
      state.phaseAt = now();
    }
  }

  function setText(text, committedLen) {
    const prev = state.text;
    let start = 0;
    const max = Math.min(prev.length, text.length);
    while (start < max && prev[start] === text[start]) start += 1;
    state.text = text;
    state.committedLen = Math.min(committedLen, text.length);
    state.changed = [start, text.length];
    state.lastUpdateAt = now();
  }

  function tickLevel() {
    if (!(state.phase === 'listening' || state.phase === 'interim')) return;
    const t = now();
    let db;
    if (speaking) {
      const s = (t - speakStartedAt) / 1000;
      const syllable = Math.sin(2 * Math.PI * 4.6 * s) * 5 + Math.sin(2 * Math.PI * 1.3 * s) * 3;
      const gap = (Math.sin(2 * Math.PI * 0.9 * s + 1.2) > 0.82) ? -14 : 0;
      db = -33 + syllable + gap + (Math.random() - 0.5) * 4;
    } else {
      db = -55 + (Math.random() - 0.5) * 3;
    }
    db = Math.max(-60, Math.min(-6, db));
    state.levelDb = db;
    state.levels.push(db);
    if (state.levels.length > LEVEL_HISTORY) state.levels.shift();
    state.sessionMs = sessionBaseMs + (t - state.pressedAt);
    if (!state.warning && state.sessionMs >= WARN_MS) state.warning = true;
  }

  function startLevels() {
    stopLevels();
    levelTimer = setInterval(tickLevel, 1000 / LEVEL_HZ);
  }
  function stopLevels() {
    if (levelTimer) clearInterval(levelTimer);
    levelTimer = null;
  }

  function beginSession(mode, baseMs) {
    runToken += 1;
    const token = runToken;
    Object.assign(state, freshState());
    state.mode = mode;
    state.pressedAt = now();
    sessionBaseMs = baseMs || 0;
    state.sessionMs = sessionBaseMs;
    setPhase('listening');
    startLevels();
    return token;
  }

  const MISHEARINGS = {
    Marta: 'Martha', Friday: 'fried A', Luis: 'Louise', mutex: 'mute ex', semaphore: 'sema for',
    Wayland: 'way land', overlay: 'over lay', daemon: 'demon', rasterizer: 'raster riser',
    fontdue: 'font do', latency: 'late and see', clipboard: 'clip board', Parakeet: 'parakeet',
  };

  /** Speak a sentence: emits interim bursts on chunk boundaries; resolves on the last chunk. */
  async function speak(token, sentence, opts = {}) {
    const words = sentence.split(/\s+/).filter(Boolean);
    const wps = opts.wps || WORDS_PER_SEC;
    const durationMs = (words.length / wps) * 1000;
    speaking = true;
    speakStartedAt = now();
    let elapsed = 0;
    let committedText = '';
    let prevSpokenCount = 0;
    while (elapsed < durationMs) {
      const step = Math.min(CHUNK_MS, durationMs - elapsed);
      await sleep(step);
      if (token !== runToken || !speaking) return false;
      elapsed += step;
      const spoken = Math.min(words.length, Math.floor((elapsed / 1000) * wps));
      if (spoken <= prevSpokenCount) continue;
      const stable = words.slice(0, prevSpokenCount).join(' ');
      committedText = stable;
      let tail = words.slice(prevSpokenCount, spoken);
      if (spoken < words.length && tail.length > 0) {
        const last = tail[tail.length - 1];
        const clean = last.replace(/[.,!?]/g, '');
        if (MISHEARINGS[clean] && Math.random() < 0.7) {
          tail = tail.slice(0, -1).concat([MISHEARINGS[clean]]);
        }
      }
      const text = (stable ? stable + ' ' : '') + tail.join(' ');
      setText(text, committedText.length);
      setPhase('interim');
      prevSpokenCount = spoken;
    }
    speaking = false;
    return true;
  }

  async function release(token, sentence, sealMs) {
    speaking = false;
    state.releasedAt = now();
    state.sessionMs = sessionBaseMs + (state.releasedAt - state.pressedAt);
    stopLevels();
    // stop_replay burst: the full sentence lands ~150 ms after release
    await sleep(150);
    if (token !== runToken) return false;
    if (sentence) {
      setText(sentence, sentence.length);
      setPhase('interim');
    }
    await sleep(40);
    if (token !== runToken) return false;
    setPhase('finalizing');
    const sealStart = now();
    await sleep(sealMs);
    if (token !== runToken) return false;
    state.sealMs = now() - sealStart;
    return true;
  }

  async function inject(token, success) {
    await sleep(120);
    if (token !== runToken) return false;
    state.success = success;
    setPhase('done');
    await sleep(AUTO_HIDE_MS);
    if (token !== runToken) return false;
    setPhase('hidden');
    return true;
  }

  async function streamAnswer(token, answer) {
    setPhase('answering');
    state.llmState = 'Generating answer...';
    state.text = '';
    state.committedLen = 0;
    await sleep(700);
    if (token !== runToken) return false;
    state.llmState = null;
    const tokens = answer.match(/\S+\s*/g) || [];
    let acc = '';
    for (const tok of tokens) {
      acc += tok;
      setText(acc, acc.length);
      await sleep(18 + Math.random() * 40);
      if (token !== runToken) return false;
    }
    return true;
  }

  const SENTENCES = [
    'send the invoice to Marta by Friday and copy Luis on the thread',
    'the overlay should feel like a quiet instrument that only speaks when you do',
    'push the branch, open a draft pull request, and ask Marta for a review before lunch',
    'remind me to check the Wayland layer shell docs for the click through region',
    'the seal path took about three hundred milliseconds which is fine for short sentences',
  ];
  const LONG_SENTENCE =
    'okay so the plan for this afternoon is to finish the overlay renderer refactor, ' +
    'move the waveform into its own module, add a test for the per character fade, ' +
    'and then write up the results in the pull request so Marta can review it before Friday, ' +
    'and if there is time left over I want to look at the clipboard latency again';
  const QUESTION = "what's the difference between a mutex and a semaphore";
  const ANSWER =
    'A mutex is a lock with a single owner: only the thread that acquired it may release it, ' +
    'so it protects a critical section. A semaphore is a counter that any thread may signal, ' +
    'so it coordinates access to N resources or hands off work between threads.';

  const scenarios = {
    short: async () => {
      const t = beginSession('stt');
      await sleep(350);
      if (t !== runToken) return;
      if (!(await speak(t, SENTENCES[0]))) return;
      if (!(await release(t, SENTENCES[0], 220))) return;
      await inject(t, true);
    },
    long: async () => {
      const t = beginSession('stt');
      await sleep(300);
      if (t !== runToken) return;
      if (!(await speak(t, LONG_SENTENCE))) return;
      if (!(await release(t, LONG_SENTENCE, 780))) return;
      await inject(t, true);
    },
    llm: async () => {
      const t = beginSession('llm');
      await sleep(400);
      if (t !== runToken) return;
      if (!(await speak(t, QUESTION))) return;
      if (!(await release(t, QUESTION, 260))) return;
      state.question = QUESTION;
      if (!(await streamAnswer(t, ANSWER))) return;
      await inject(t, true);
    },
    empty: async () => {
      const t = beginSession('stt');
      await sleep(1400);
      if (t !== runToken) return;
      if (!(await release(t, '', 150))) return;
      state.text = '';
      await inject(t, false);
    },
    error: async () => {
      const t = beginSession('stt');
      await sleep(300);
      if (t !== runToken) return;
      const partial = SENTENCES[3];
      speak(t, partial);
      await sleep(3200);
      if (t !== runToken) return;
      speaking = false;
      stopLevels();
      state.reason = 'daemon disconnected';
      setPhase('error');
      await sleep(1800);
      if (t !== runToken) return;
      setPhase('hidden');
    },
    cap: async () => {
      const t = beginSession('stt', WARN_MS - 4000);
      await sleep(300);
      if (t !== runToken) return;
      const s = SENTENCES[2] + ' ' + SENTENCES[3] + ' ' + SENTENCES[4];
      if (!(await speak(t, s))) return;
      if (!(await release(t, s, 600))) return;
      await inject(t, true);
    },
  };
  const scenarioOrder = ['short', 'long', 'llm', 'empty', 'error', 'cap'];

  async function playAll() {
    for (const key of scenarioOrder) {
      const before = runToken;
      await scenarios[key]();
      if (runToken !== before + 1) return; // interrupted
      await sleep(900);
    }
  }

  // free play: hold Space (Shift+Space for llm)
  let holdToken = null;
  let holdSentence = '';
  let holdMode = 'stt';
  function holdStart(mode) {
    holdMode = mode;
    holdSentence = mode === 'llm' ? QUESTION : SENTENCES[Math.floor(Math.random() * SENTENCES.length)];
    holdToken = beginSession(mode);
    const t = holdToken;
    (async () => {
      await sleep(400);
      if (t !== runToken) return;
      await speak(t, holdSentence + ' ' + SENTENCES[(Math.random() * SENTENCES.length) | 0], { wps: 2.4 });
    })();
  }
  async function holdStop() {
    const t = holdToken;
    if (t === null || t !== runToken) return;
    holdToken = null;
    const spokenWords = Math.max(1, Math.floor(((now() - speakStartedAt) / 1000) * 2.4));
    const sentence = state.text ? holdSentence.split(' ').slice(0, spokenWords).join(' ') || state.text : '';
    const sealMs = 160 + Math.min(900, spokenWords * 45);
    if (!(await release(t, sentence, sealMs))) return;
    if (holdMode === 'llm') {
      state.question = sentence;
      if (!(await streamAnswer(t, ANSWER))) return;
    }
    await inject(t, sentence.length > 0);
  }

  function cancel() {
    runToken += 1;
    speaking = false;
    stopLevels();
    setPhase('hidden');
  }

  // ---------------------------------------------------------------- backdrops
  const BACKDROPS = ['chat', 'terminal', 'docs'];
  function buildBackdrop(kind) {
    const el = document.getElementById('backdrop');
    el.className = 'backdrop backdrop-' + kind;
    if (kind === 'terminal') {
      el.innerHTML = `
        <div class="panel"><span>Sep 6, 12:04</span></div>
        <div class="term">
          <pre>$ just phase6-contract
&gt;&gt;&gt; running overlay contract (1 pass)
  overlay_events_emitted      412
  overlay_events_dropped      0
  stream_path_executed        true
  finalization_mode           offline_seal
  seal_ms                     284
PASS  overlay-contract  (12.4s)

$ git log --oneline -5
7122eb9 fix(seal): stop tail trimmer from cutting quiet last words
2348092 chore(skills): refresh project skill links
d8ff092 fix(llm): change managed server default port
c2ba66b Merge pull request #183 from SystemicVoid/issue/110-stream-seal-final-accuracy
72c2143 refactor(model): deduplicate streaming decode defaults

$ git commit -m "<span class="caret">▍</span></pre>
        </div>`;
    } else if (kind === 'docs') {
      el.innerHTML = `
        <div class="panel"><span>Sep 6, 12:04</span></div>
        <div class="doc">
          <h1>Overlay redesign: decision notes</h1>
          <p>The overlay is the only thing the user sees during a session. It has to answer one question in under a second: is it hearing me? Everything else is secondary.</p>
          <p>Interim text arrives in bursts every 2.4 seconds, not per character. The seal path replaces the tail on release. The design should make those mechanics feel deliberate rather than laggy.</p>
          <h2>Open questions</h2>
          <ul><li>Does a frameless treatment survive light backdrops?</li><li>How much of the runtime truth belongs on screen?</li><li>What does the LLM answer mode look like when the answer is long?</li></ul>
          <p class="field" contenteditable="false">Type your notes here<span class="caret">|</span></p>
        </div>`;
    } else {
      el.innerHTML = `
        <div class="panel"><span>Sep 6, 12:04</span></div>
        <div class="chat">
          <aside>
            <div class="brand">Threads</div>
            <div class="item active">Overlay redesign</div>
            <div class="item">Seal path accuracy</div>
            <div class="item">Clipboard latency</div>
            <div class="item">Eval dataset v3</div>
          </aside>
          <main>
            <div class="msg them">Can you avoid leaving json at root, and also place output in the same folder so it's all together?</div>
            <div class="msg me">Moved the output, JSON configuration, generator, tests, and instructions into <code>context/</code>. Links updated. All six tests and lint checks pass.</div>
            <div class="msg them">What did the final overlay contract run say?</div>
            <div class="msg me">412 events emitted, 0 dropped, seal path 284 ms. Green.</div>
            <div class="composer"><span class="placeholder">Message Marta</span><span class="caret">|</span></div>
          </main>
        </div>`;
    }
  }

  // ---------------------------------------------------------------- switcher bar
  function buildBar() {
    const bar = document.getElementById('proto-bar');
    bar.innerHTML = `
      <button data-act="prev" title="previous variant (←)">‹</button>
      <span id="proto-variant" class="label"></span>
      <button data-act="next" title="next variant (→)">›</button>
      <span class="sep"></span>
      ${scenarioOrder.map((k, i) => `<button data-scn="${k}" title="key ${i + 1}">${k}</button>`).join('')}
      <button data-act="all" title="play every scenario">all</button>
      <button data-act="cancel" title="Esc">stop</button>
      <span class="sep"></span>
      <button data-act="backdrop" title="cycle backdrop (b)">backdrop</button>
      <span class="hint">hold <kbd>Space</kbd> to talk · <kbd>Shift</kbd>+<kbd>Space</kbd> asks the LLM</span>
      <span id="proto-state" class="state"></span>`;
    bar.addEventListener('click', (e) => {
      const b = e.target.closest('button');
      if (!b) return;
      if (b.dataset.scn) scenarios[b.dataset.scn]();
      else if (b.dataset.act === 'prev') cycle(-1);
      else if (b.dataset.act === 'next') cycle(1);
      else if (b.dataset.act === 'all') playAll();
      else if (b.dataset.act === 'cancel') cancel();
      else if (b.dataset.act === 'backdrop') cycleBackdrop();
    });
  }

  function updateBarState() {
    const el = document.getElementById('proto-state');
    if (!el) return;
    el.textContent =
      `${state.phase} · ${state.mode} · ${(state.sessionMs / 1000).toFixed(1)}s · ` +
      `${state.levelDb.toFixed(0)} dB · ${state.text.length} ch (${state.committedLen} stable)` +
      (state.warning ? ' · WARNING' : '') + (state.reason ? ' · ' + state.reason : '');
  }

  let backdropKey = 'chat';
  function cycleBackdrop() {
    backdropKey = BACKDROPS[(BACKDROPS.indexOf(backdropKey) + 1) % BACKDROPS.length];
    buildBackdrop(backdropKey);
    syncUrl();
  }

  function syncUrl() {
    const url = new URL(location.href);
    url.searchParams.set('variant', currentKey);
    url.searchParams.set('backdrop', backdropKey);
    history.replaceState(null, '', url);
  }

  function select(key) {
    if (!registry.has(key)) key = order[0];
    if (currentKey === key) return;
    const root = document.getElementById('overlay-root');
    if (current && current.unmount) current.unmount();
    root.innerHTML = '';
    root.className = 'variant-' + key;
    current = registry.get(key);
    currentKey = key;
    current.mount(root);
    document.getElementById('proto-variant').textContent = `${key.toUpperCase()} · ${current.name}`;
    syncUrl();
  }

  function cycle(delta) {
    const i = order.indexOf(currentKey);
    select(order[(i + delta + order.length) % order.length]);
  }

  function frame() {
    if (current && current.render) current.render(state, now());
    updateBarState();
    requestAnimationFrame(frame);
  }

  let spaceHeld = false;
  function onKey(e) {
    const t = e.target;
    if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.isContentEditable)) return;
    if (e.type === 'keydown') {
      if (e.key === 'ArrowLeft') cycle(-1);
      else if (e.key === 'ArrowRight') cycle(1);
      else if (e.key === 'b') cycleBackdrop();
      else if (e.key === 'Escape') cancel();
      else if (e.key === ' ' ) {
        e.preventDefault();
        if (!spaceHeld) { spaceHeld = true; holdStart(e.shiftKey ? 'llm' : 'stt'); }
      } else if (/^[1-6]$/.test(e.key)) scenarios[scenarioOrder[Number(e.key) - 1]]();
    } else if (e.type === 'keyup' && e.key === ' ') {
      spaceHeld = false;
      holdStop();
    }
  }

  function init() {
    buildBar();
    const params = new URLSearchParams(location.search);
    backdropKey = BACKDROPS.includes(params.get('backdrop')) ? params.get('backdrop') : 'chat';
    buildBackdrop(backdropKey);
    select(params.get('variant') || order[0]);
    // ?auto=<scenario> runs a scenario on load (used by shoot.sh with headless Chrome virtual time).
    const auto = params.get('auto');
    if (auto && scenarios[auto]) setTimeout(() => scenarios[auto](), 50);
    window.addEventListener('keydown', onKey);
    window.addEventListener('keyup', onKey);
    requestAnimationFrame(frame);
  }

  window.OverlayProto = {
    register(key, def) {
      registry.set(key, def);
      if (!order.includes(key)) order.push(key);
      order.sort();
    },
    state,
    scenarios,
    STATE_DOC,
    select,
  };
  document.addEventListener('DOMContentLoaded', init);
})();
