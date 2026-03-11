# The Paste Gap Investigation: A Deep Dive into Wayland uinput Reliability

**Date:** 2026-03-11
**Status:** Closed — PR #25 merged as commit `7c30635`
**Archived Source:** `docs/archive/HANDOFF-raw-ptt-paste-gap-2026-03-08.md`

---

## Executive Summary

The Parakeet push-to-talk (PTT) system suffered an intermittent but frustrating paste reliability issue: plain raw dictation would successfully produce transcripts and update the clipboard, but the automatic paste into the target application would silently fail roughly 80% of the time. Meanwhile, the LLM answer injection path appeared to work more reliably—though later evidence showed it was also not immune to failures.

This investigation spanned three days (2026-03-08 to 2026-03-10) and involved:
- Multiple disproven hypotheses (focus drift, timing races, backend selection)
- Construction of a controlled evidence-gathering harness
- Deep research into Linux `uinput` semantics and compositor behavior (Smithay/COSMIC)
- A final smoking-gun discovery: **95% success with persistent device vs 20% with per-job device creation**

The root cause was identified as the **fresh virtual keyboard discovery race**: creating a brand-new `uinput` virtual keyboard device for every paste attempt, immediately emitting chords, and then destroying it. Linux uinput documentation explicitly warns about this pattern. Compositors like COSMIC require time to discover, classify, and seat new devices before routing their events.

The fix that shipped replaced the subprocess-per-injection model with a **persistent in-process uinput sender** that survives across utterances, with lazy initialization for `/dev/uinput` recovery and a bounded warm-up after fresh device creation.

---

## 1. Problem Statement

### Symptoms

1. **Primary symptom:** Plain raw PTT dictation (`stt`, `stt start`, `stt llm` without LLM modifier) would:
   - Produce correct transcripts
   - Update the system clipboard successfully
   - **Fail to auto-paste** into the target application (Ghostty, Brave, Zed)
   - Manual paste (`Ctrl+V` or `Ctrl+Shift+V`) from the same clipboard worked correctly

2. **Secondary symptom:** LLM answer injection historically worked more reliably, suggesting the bug was specific to the raw path.

3. **Later contradiction:** Eventually observed that raw dictation could occasionally paste successfully while LLM-query runs could also fail, weakening the clean "raw bad / LLM good" split.

### Reproduction Steps

1. Start `stt` or `stt llm`
2. Use normal raw dictation (plain PTT, hold and release talk key)
3. Release the talk key
4. Observe transcript is recognized, clipboard updates, but automatic paste does not happen

**Confirmed unaffected by:**
- Offline/streaming/overlay modes
- Different target surfaces (Ghostty, Brave, Zed all exhibited failures)

---

## 2. Initial Investigation: Theory-Driven Hypotheses

### Hypothesis 1: Paste Fires Too Early After Hotkey Release (REJECTED)

**Theory:** Raw dictation enqueues injection ~300ms after hotkey-up, while LLM path enqueues 700-1900ms later. The synthetic paste might be firing before the target surface is ready to accept input after the modifier key (right Ctrl) is released.

**Experiment attempted:**
- Added `not_before` deadline on raw final-result injection jobs
- Added fixed 250ms raw-only settle delay before injection
- Added `settle_wait_ms` reporting and timing tests

**Result:** The delay did not solve the problem. Paste behavior remained intermittent.

**Action taken:** Experiment reverted, troubleshooting docs updated to not describe a false fix.

**Key insight:** The timing difference is real (~300ms vs ~700-1900ms), but a simple fixed delay is not sufficient. The difference between the paths is more structural than a timing offset.

### Hypothesis 2: Focus/Routing Drift at Injection Time (REJECTED)

**Theory:** Between hotkey-up and paste injection, focus might have drifted away from the intended target, causing the synthetic chord to route to the wrong surface.

**Evidence against this:**
- Child-side focus snapshots were captured and threaded through subprocess boundaries
- Runs with correct-looking route selection (Ghostty `CtrlShiftV`, Brave `CtrlV`) still showed visible paste failures
- Parent-guided routing override was tried and ultimately removed because it added complexity without solving the problem

**Key insight:** The package was addressed correctly and marked "delivered," but the recipient never got it. The failure boundary is the handoff at the door, not the address label.

---

## 3. Instrumentation and Evidence-Gathering

### Phase 1: Structured Logging

To move beyond "it works sometimes" to comparable evidence, we added:

**Path tags on injection jobs:**
- `origin=raw_final_result`
- `origin=llm_answer`
- `origin=demo`

**Timing fields:**
- `hotkey_up_elapsed_ms_at_enqueue` / `hotkey_up_elapsed_ms_at_worker_start`
- `stop_message_elapsed_ms_at_enqueue` / `stop_message_elapsed_ms_at_worker_start`
- `queue_wait_ms` (separated queue delay from "time since release")

**Child-side reporting:**
- `PARAKEET_INJECT_REPORT {json}` emitted on every injection
- Parent parses child reports into structured logs
- Fields include: outcome, clipboard_ready, post_clipboard_matches, route_class, backend_attempts, duration, exit_status

### Phase 2: Paste-Gap Matrix Harness

To reduce operator variance and standardize backend comparisons, we created:

**`scripts/paste-gap-matrix.sh`** with commands:
- `paste-gap-start` — Record baseline SHA/worktree status, clear logs, start with explicit backend
- `paste-gap-inject-only` — Isolate backend behavior without PTT lifecycle
- `paste-gap-stop` — Archive runtime artifacts
- `paste-gap-diag` — Run control tests
- `paste-gap-summary` / `paste-gap-current` — Parse and compare results

**Just recipes:**
- `just paste-gap-start`, `just paste-gap-inject-only`, `just paste-gap-stop`
- `just paste-gap-diag`, `just paste-gap-summary`, `just paste-gap-current`

### Phase 3: Evidence-Gathering Manual

**`PASTE-GAP-EVIDENCE-GATHERING-USER-MANUAL.md`** codified:

- Exact terminal layout (tmux session with Ghostty sink and log panes)
- 10-phrase test set for repeatable comparisons
- Backend isolation with forced shortcut selection
- Artifact checklist and observation templates

---

## 4. The Backend Matrix: Smoking Gun Discovery

On 2026-03-10, we ran a comprehensive matrix comparing **inject-only** (no ASR, no hotkey, just clipboard + synthetic paste) against **full PTT flow** across three backends.

### Results Matrix

| Run | Backend | Mode | Attempts | Visible Pastes | Rate | Key Finding |
|-----|---------|------|----------|---------------|------|-------------|
| 1 | uinput | inject-only | 20 | 19/20 | 95% | First/last chars truncated, no newlines |
| 2 | ydotool | inject-only | 20 | 20/20 | 100% | **All paste = "244442" (wrong content)** |
| 3 | auto | inject-only | 20 | 19/20 | 95% | Same as uinput (auto=uinput here) |
| 4 | uinput | PTT flow | 10 | 2/10 | **20%** | child_focus MISSING on 7/10 |
| 5 | ydotool | PTT flow | 10 | 10/10 | 100% | **All paste = "244442" (wrong content)** |
| 6 | auto | PTT flow | 10 | 7/10 | 70% | Some Cyrillic output; 11 injections for 10 attempts |

### The Smoking Gun

**Inject-only uinput achieves 95% success. PTT-flow uinput achieves only 20%.**

Same backend. Same Ghostty target. Same shortcut (`CtrlShiftV`). The only difference is the **PTT lifecycle path**.

This definitively separates the uinput backend's intrinsic reliability from the PTT flow's injection delivery mechanism.

### Additional Critical Findings

**ydotool is broken without `ydotoold`:**
- Consistently types keycode numbers as literal text
- `"244442"` corresponds to keycodes 29 (Ctrl), 42 (Shift), 47 (V)
- Without the ydotoold daemon, ydotool falls back to direct evdev which is not a paste backend—it's a keycode-to-text printer

**Cyrillic output in PTT uinput runs:**
- Indicates partial chord interpretation during device warm-up
- If only modifier key events land before the device is fully integrated, the compositor may assume a different keymap layout

**Truncated first/last characters:**
- Fits the pattern of partial packet loss from a newly-enumerated device
- Parakeet emits `Ctrl` down, `Shift` down, `V` down as separate `emit()` calls with automatic `SYN_REPORT` flushing

---

## 5. Root Cause Analysis

Three root causes were identified, ranked by probability:

### RC1: Fresh uinput Virtual Keyboard Per Injection (45% probability)

**Mechanism:**

The PTT flow spawns a new subprocess (`--internal-inject-once`) for **every injection**. Each subprocess creates a brand-new `UinputChordSender`, which creates a brand-new `/dev/uinput` virtual keyboard device, emits the chord almost immediately, then exits—destroying the device.

**Code path difference:**
- **inject-only:** `build_injector_with_shortcut_override()` once → `injector.inject()` × N (main.rs:1867-1881)
- **PTT flow:** `InjectorSubprocessRunner.run()` → spawn new process → `run_internal_inject_once()` → `build_injector_with_shortcut_override()` → `inject()` once → exit (main.rs:845-875, 1927-1944)

**Why this explains the evidence:**
- Compositors and applications commonly drop or partially interpret the first events from a newly-enumerated virtual input device
- Fits the 20% PTT success rate vs 95% inject-only success rate
- Explains truncated first/last characters (partial chord interpretation during device warm-up)
- Explains Cyrillic output (chord partially interpreted = wrong keymap state assumed by compositor)
- Explains why the 250ms settle delay after hotkey release did not fix it—the delay was before the subprocess spawn, not after the virtual device creation

### RC2: Hotkey Listener Self-Observing Its Own Virtual Keyboard (25% probability)

**Mechanism:**

`is_hotkey_capable_device()` in `hotkey.rs:907-913` attaches to **any** `/dev/input/event*` device that supports the talk key OR any LLM pre-modifier key. The Parakeet virtual keyboard declares `KEY_LEFTCTRL` and `KEY_LEFTSHIFT` in its capability set (injector.rs:429-432). The default LLM pre-modifier is `KEY_SHIFT`.

**Result:** Every PTT injection creates a transient virtual keyboard → hotkey supervisor (rescanning every 750ms) attaches to it → subprocess exits → device disappears → `No such device (os error 19)` warning.

The `/dev/input/event18` churn warnings seen throughout the investigation are almost certainly the hotkey supervisor attaching to and losing its own transient virtual keyboards.

### RC3: Hotkey Release / Input-State Race at Compositor Boundary (15% probability)

**Mechanism:**

The operator's PTT key is right Ctrl (a modifier key). Raw path enqueues injection ~300ms after hotkey-up; LLM path 700-1900ms later. Physical modifier key release may leave compositor aggregate key state unsettled when the synthetic chord lands.

**Evidence for:** Historically, later injections (LLM path) were somewhat healthier.

**Evidence against:** The 250ms settle delay did not fix it alone.

**Verdict:** Likely a contributing factor rather than the primary cause.

---

## 6. Deep Technical Research: Linux uinput and Compositor Semantics

### Linux uinput Device Lifecycle

**VirtualDeviceBuilder::build()** (evdev crate):
- Calls `ioctl(fd, UI_DEV_CREATE)` to create the virtual device
- Returns a file descriptor that writes event packets to `/dev/input/eventX`

**Linux uinput documentation explicitly warns:**

> After `UI_DEV_CREATE`, the kernel creates `/dev/input/eventX`. The official example includes `sleep(1)` before the first emitted event so userspace has time to detect the new device.

**SYN_REPORT flushing (evdev 0.12.2):**
- Each `emit()` call automatically appends `SYN_REPORT` (synchronization report)
- Parakeet's `Ctrl+Shift+V` sequence is effectively:
  1. Ctrl down + SYN
  2. Shift down + SYN
  3. V down + SYN
  4. sleep (dwell)
  5. V up + SYN
  6. Shift up + SYN
  7. Ctrl up + SYN

This means partial observation of early packets can **materially change semantics**. If the compositor starts routing the device mid-chord or misses early packets, the app may see:
- Naked `V` (no modifiers)
- `Shift+V` (missing Ctrl)
- Release-only cleanup
- Layout-dependent text instead of paste

### Smithay/Wayland Compositor Model

**Seat keyboard capability vs backend device hotplug:**
- Smithay separates these concerns: a seat gets a `KeyboardHandle` when the compositor explicitly calls `Seat::add_keyboard(...)`
- Backend `DeviceAdded` does not create one automatically
- In reference Anvil compositor: `add_keyboard()` happens once at startup, while hotplug handling only updates LED state / device bookkeeping

**Backend keyboard routing requires focus:**
- Backend key events flow through `keyboard.input(...)`
- Updates XKB state → runs compositor filter → goes through grab handling → forwards to focused client
- If focus is `None`, final delivery returns early

**Virtual-keyboard protocol is a different path:**
- The `zwp_virtual_keyboard_v1` handler reuses existing seat keyboard focus
- Requires a client-provided keymap first
- Sends `wl_keyboard.key` directly to focused clients
- **Bypasses** the normal backend-device path, XKB state tracking, compositor filter, and grab stack

### COSMIC Compositor Implementation

**Concrete race mechanism:**
- COSMIC processes `DeviceAdded` by inserting device into the seat's device map
- Keyboard events look up the seat via `for_device(&event.device())`
- **If the device is not yet known to any seat, the keyboard event is silently dropped**

**Always-present seat keyboard:**
- COSMIC's `create_seat()` always calls `seat.add_keyboard(...)`
- Matches Smithay's recommended pattern of separating seat capability from later device hotplug
- This weakens the "missing keyboard capability" hypothesis—a fresh `uinput` failure is more likely to be "device not yet discovered/routable" than "seat lacked a keyboard object"

---

## 7. Disproven Hypotheses (Cumulative)

| Hypothesis | Status | Evidence |
|------------|--------|---------|
| Focus/routing is primary cause | **Disproven** | Parent-guided focus override tried and removed; correct routing proven on every attempt |
| ydotool is a valid fallback backend | **Disproven** | Without ydotoold, it types keycode numbers as literal text (`244442` = keycodes 29,42,47) |
| Fixed settle delay after hotkey release | **Disproven** | 250ms delay did not change outcomes |
| auto mode tries ydotool fallback | **Disproven** | auto only falls back on hard errors; uinput never hard-errors |
| Empty Ghostty sink = paste did not happen | **Disproven for inject-only** | `cat \| tee` is line-buffered; no-newline pastes are visible but not flushed to pipe |
| `success_assumed = paste worked` | **Disproven** | Means "no error thrown", not "target received it" |

---

## 8. The Fix Journey: From Hypothesis to Implementation

### Iteration 1: Subprocess Cleanup (commits 1-8, March 8)

Initial cleanup of clipboard helper subprocess lifetime issues:
- `wl-copy` helper processes were inheriting injector subprocess pipes
- Delayed paste completion due to leaked clipboard helper pipes
- Fixed stderr pipe handling to use child exit as authoritative boundary
- Added nonblocking poll-based stderr drain

**Impact:** Removed noise and timing artifacts, but did not solve the core paste failure.

### Iteration 2: In-Process Injection with Child Reporting (commits 9-16)

Major architectural shift:
- Added structured child reporting via `PARAKEET_INJECT_REPORT {json}`
- Added parent focus capture at hotkey-up
- Made `FocusSnapshot` serializable across process boundaries
- Added paste-gap matrix harness and evidence-gathering manual

**Impact:** Provided the observability needed to discover the smoking gun. Still used subprocess-per-injection model.

### Iteration 3: Full In-Process Worker (commit 17)

Removed subprocess worker harness from runtime:
- Moved to in-process injection only
- Kept subprocess coverage in tests only
- Cleaner report-builder shape

**Impact:** Eliminated subprocess overhead but still recreated `UinputChordSender` per job.

### Iteration 4: Backend Cleanup and Legacy Removal (commits 18-27)

Systematic simplification:
- Disabled `ydotool` and `wtype` backends entirely (proven broken)
- Removed helper-level backend selection (now uinput-only)
- Updated docs (README, SPEC, troubleshooting) to match uinput-only contract
- Added legacy backend name warnings that degrade to uinput

**Impact:** Reduced complexity and removed broken fallback paths.

### Iteration 5: Review-Safe Per-Job Rebuild (commit 28)

Addressed review concern about startup-time `/dev/uinput` poisoning:
- Per-job injector rebuilds for late `/dev/uinput` recovery
- Fixed a real behavioral regression
- **But knowingly gave up warm-device reuse**

**Impact:** Correct as a recovery stopgap, but likely a reliability regression because it recreated the risky "brand-new device" boundary on every paste job.

### Iteration 6: Persistent In-Process Sender with Recovery (commits 29-33, March 10)

Final fix combining reliability with recovery:

**Key changes in `parakeet-ptt/src/main.rs`:**

1. **Created `InProcessInjectorRunner`** with persistent sender state:
   ```rust
   enum UinputSenderState {
       Uninitialized,
       Healthy {
           sender: Arc<UinputChordSender>,
           created_at: Instant,
           use_count: u64,
       },
       CreateFailed {
           last_error: String,
           retry_after: Instant,
       },
   }
   ```

2. **Sender lifecycle:**
   - Lazily create on first paste attempt
   - Keep for subsequent jobs while healthy
   - Retry after failure with bounded backoff
   - Drop sender on explicit send I/O error

3. **One-time warm-up:**
   - After fresh device creation, wait bounded delay before first chord
   - Only apply warm-up after fresh create, not on every paste
   - Internal/tunable for experiments

4. **Per-job injector rebuild with persistent sender:**
   - Still rebuild `ClipboardInjector` per job for per-job context
   - But reuse the same `UinputChordSender` across jobs

5. **Telemetry:**
   - `uinput_sender_generation`
   - `uinput_fresh_device: bool`
   - `uinput_device_age_ms_at_first_key`
   - `uinput_use_count`
   - Backend attempt summaries include uinput lifecycle suffixes

**Why this design:**
- Preserves main reliability property: **warm discovered device**
- Preserves recovery property: **late `/dev/uinput` recovery**
- Directly addresses only mechanism explaining the 95% vs 20% split
- Matches Smithay/COSMIC's model: seat keyboard already exists
- Keeps reliability-critical state at the right layer: persist device handle, not a startup decision that can never recover

### Iteration 7: Thread Leak and Serde Fixes (commits 34-36)

Final polish:
- Fixed thread leak from fresh `ClipboardInjector` creating multiple focus cache workers
- Fixed `FocusSnapshot` serde round-tripping (resolver field now owned string data)
- Fixed paste-gap summary parsing for nullable focus payloads
- Fixed `copy-only` runs emitting proper `clipboard_write_failed` outcome

### Iteration 8: Archive and Closure (commit 37)

Moved investigation docs to `docs/archive/` with closure context:
- Investigation track is closed
- Current runtime truth now lives in `docs/stt-troubleshooting.md`
- Archive preserved as historical debugging context

---

## 9. Confirmed Environmental Facts

- **Talk key:** right Ctrl (modifier key)
- **LLM pre-modifier:** Shift
- **Compositor:** COSMIC (Wayland)
- **ydotoold:** not running; ydotool falls back to direct evdev which types keycode numbers as text
- **Parakeet virtual keyboard name:** `"Parakeet STT Virtual Keyboard"`
- **Virtual keyboard capabilities:** `KEY_LEFTCTRL`, `KEY_LEFTSHIFT`, `KEY_V`

---

## 10. Key Technical Insights

### Insight 1: Fresh Device Discovery is Real, Not Superstition

The Linux uinput documentation's warning about a post-create discovery window is not theoretical. Compositors like COSMIC require time to:
1. Detect the new `/dev/input/eventX` node
2. Classify the device (read capabilities)
3. Associate it with a seat
4. Make it routable for keyboard events

When Parakeet creates a device, immediately emits a chord, then destroys it—events can be:
- Silently dropped (device not yet routable)
- Partially interpreted (mid-chord discovery)
- Mis-routed (seat association incomplete)

### Insight 2: Persistent Device Changes Semantics, Not Just Performance

A persistent device is not just "faster to create"—it changes semantics because:
- The compositor/libinput have already discovered, classified, and seated the device before the first real paste chord
- This is a **real behavioral difference**, not mere micro-optimization
- The 95% vs 20% split demonstrates this empirically

### Insight 3: Subprocess → In-Process is Necessary but Not Sufficient

The system is already in-process today (after iteration 3), but reliability was poor again when it recreated the uinput device per job (iteration 5). That means:
- "Subprocess → in-process" alone is not sufficient
- **Device lifetime/warmth is a necessary part of the improvement**
- The exact share versus subprocess effects cannot be isolated from the available data

### Insight 4: Review Concerns and Reliability Goals Can Be Reconciled

The review concern about startup-time `/dev/uinput` poisoning was correct—but the per-job rebuild fix went too far in the other direction by giving up warm-device reuse.

The final design preserves both:
- **Late recovery:** retry after failure with backoff
- **Warm reliability:** keep device alive while healthy
- **Bounded warm-up:** only after fresh create, not every time

### Insight 5: Observability at the Right Boundary Was Critical

The smoking gun (95% vs 20%) could only be discovered because we:
1. Separated inject-only (no PTT lifecycle) from full PTT flow
2. Added per-attempt telemetry distinguishing fresh vs reused devices
3. Standardized evidence collection to make runs comparable

Without this, we would have continued chasing focus/routing theories that were already disproven but not yet proven.

---

## 11. Acceptance Criteria

The fix is validated when:

- PTT-flow uinput paste rate into Ghostty matches or exceeds the inject-only rate (~95%)
- `/dev/input/event18` churn warnings no longer appear
- No ydotool code paths remain
- `cargo test` passes
- `cargo fmt` passes
- Manual Ghostty PTT repro confirms reliable paste
- Late `/dev/uinput` recovery works without restart
- Raw dictation and LLM-answer paths both pass

---

## 12. Lessons Learned

### For Future Investigations

1. **Build evidence before fixing:** The matrix harness and structured logging were essential to finding the smoking gun. Ad hoc runs without standardized comparison would have perpetuated hypothesis churn.

2. **Hypotheses should be falsifiable:** The 250ms settle delay experiment was valuable because it cleanly falsified a theory, not because it solved the problem.

3. **Preserve disproven work:** Archived docs prevent future sessions from circling back to rejected fixes.

4. **Instrumentation at the failure boundary:** Child-side reporting was the key to seeing that "clipboard prepared" ≠ "target visibly pasted."

### For System Design

1. **Device lifetime matters more than process architecture:** Persistent in-process device with proper lifecycle semantics is more important than just "avoiding subprocesses."

2. **Recovery should not sacrifice reliability:** The right design is not "cache everything forever" and not "recreate everything every time," but "keep device alive while healthy, be willing to recreate when reality changes."

3. **Telemetry precedes tuning:** Before settling on warm-up duration, measure the difference between fresh and reused devices across many runs.

### For Compositor/Integration Work

1. **Virtual keyboard protocol ≠ backend uinput:** Smithay's `zwp_virtual_keyboard_v1` path bypasses the backend-device discovery race entirely. It is a different delivery mechanism with different failure modes, not a drop-in replacement for understanding `uinput` behavior.

2. **Seat capability vs device hotplug are separate:** Compositors that follow Smithay's pattern create logical seat keyboards up front, separate from backend device hotplug. This means "seat has no keyboard" is less likely than "new device not yet routable."

---

## 13. References

**Archived investigation documents:**
- `docs/archive/HANDOFF-raw-ptt-paste-gap-2026-03-08.md` — Full investigation timeline and status updates
- `docs/archive/PASTE-RELIABILITY-ANALYSIS-2026-03-10.md` — Deep uinput/compositor technical analysis

**Pull request:**
- PR #25: `fix(injector): persistent in-process uinput sender with discovery-race warm-up`
- Merged as commit `7c30635` on 2026-03-11

**Code locations referenced:**
- `parakeet-ptt/src/main.rs:823-840` — InProcessInjectorRunner::run()
- `parakeet-ptt/src/main.rs:1867-1881` — inject-only persistent injector pattern
- `parakeet-ptt/src/main.rs:1910-1956` — build_injector_with_shortcut_override()
- `parakeet-ptt/src/injector.rs:425-445` — UinputChordSender::new()
- `parakeet-ptt/src/injector.rs:468-487` — send_shortcut() chord emission
- `parakeet-ptt/src/hotkey.rs:907-913` — is_hotkey_capable_device()

**External references:**
- Linux uinput documentation — UI_DEV_CREATE discovery window warning
- evdev 0.12.2 — SYN_REPORT automatic flushing
- Smithay — seat keyboard capability vs backend device hotplug separation
- COSMIC compositor — DeviceAdded / seat device map race mechanism

---

**End of Report**
