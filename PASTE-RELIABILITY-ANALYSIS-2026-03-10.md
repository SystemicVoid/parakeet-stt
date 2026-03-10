# Paste Reliability Analysis: Grounded Evidence and Recommended Design (2026-03-10)

## 0. Executive Summary

- **Strongest read:** the main problem is the lifecycle of a freshly created `uinput` keyboard, not clipboard preparation, route selection, or missing seat keyboard capability.
- **Best-supported final design:** keep one `uinput` device alive while healthy, lazily create or recreate it when needed, and apply a bounded warm-up only after a fresh create or recovery.
- **Current state (`8564084`):** correct as a recovery stopgap, but likely a reliability regression because it recreates the risky "brand-new device" boundary on every paste job.
- **Do not ship as the fix:** blind chord retries, text-mutating priming, or per-job create/destroy with only a bigger delay.
- **Most important next proof:** instrument fresh-vs-reused device age and rerun the A/B matrix so the final design is justified by measured behavior, not only by plausible mechanism.

## 1. Facts

- **Current PTT path rebuilds the injector on every job.** `InProcessInjectorRunner::run()` calls `self.injector_builder(&self.config)` for each injection job, explicitly to recover from late `/dev/uinput` availability without restart. `[parakeet-ptt/src/main.rs:823-840]`

- **Each rebuild creates a brand-new uinput virtual keyboard.** `build_injector_with_shortcut_override()` calls `UinputChordSender::new(...)`, and `UinputChordSender::new()` calls `VirtualDeviceBuilder::new().build()` to create the device. `[parakeet-ptt/src/main.rs:1910-1956]` `[parakeet-ptt/src/injector.rs:425-445]`

- **The virtual keyboard is very minimal and short-lived in the per-job model.** It only advertises `KEY_LEFTCTRL`, `KEY_LEFTSHIFT`, and `KEY_V`, under the name `"Parakeet STT Virtual Keyboard"`. In the current model, that device exists for one job, emits the chord, then is dropped with the injector. `[parakeet-ptt/src/injector.rs:427-439]`

- **The chord is emitted as separate packets, not one atomic packet.** `send_shortcut()` emits modifier-down events one by one, then `V` down, sleeps for dwell, then `V` up, then modifier-up events in reverse order. `[parakeet-ptt/src/injector.rs:468-487]`

- **Per evdev 0.12.2, `emit()` appends `SYN_REPORT` automatically.** So Parakeet's sequence for `Ctrl+Shift+V` is effectively:
  1. Ctrl down + SYN
  2. Shift down + SYN
  3. V down + SYN
  4. sleep
  5. V up + SYN
  6. Shift up + SYN
  7. Ctrl up + SYN

  This means partial observation of the early packets can materially change semantics. `[evdev-0.12.2/src/uinput.rs:282-293]` `[parakeet-ptt/src/injector.rs:475-485]`

- **Linux uinput docs explicitly warn about a post-create discovery window.** After `UI_DEV_CREATE`, the kernel creates `/dev/input/eventX`, and the official example includes `sleep(1)` before the first emitted event so userspace has time to detect the new device. `[codemap-deepwiki-uinput.md:55-64,99-103]`

- **Smithay separates seat keyboard capability from backend device hotplug.** A seat only gets a `KeyboardHandle` when the compositor explicitly calls `Seat::add_keyboard(...)`; backend `DeviceAdded` does not create one automatically. In the reference Anvil compositor, `add_keyboard()` happens once at startup, while hotplug handling only updates LED state / device bookkeeping. `[Smithay src/wayland/seat/mod.rs:167-180,567-586]` `[Smithay anvil/src/state.rs:659-665]` `[Smithay anvil/src/udev.rs:309-320]`

- **Smithay's backend keyboard routing requires focus, not per-device keyboard-handle creation.** Backend key events flow through `keyboard.input(...)`, which updates XKB state, runs the compositor filter, goes through grab handling, and only then forwards to the focused client. If focus is `None`, final delivery returns early. `[Smithay src/input/keyboard/mod.rs:947-970,1013-1043,1264-1298,1321-1368]`

- **Smithay virtual-keyboard protocol is a different path than backend evdev/uinput devices.** The `zwp_virtual_keyboard_v1` handler reuses the existing seat keyboard focus, requires a client-provided keymap first, and then sends `wl_keyboard.key` directly to focused clients, bypassing the normal backend-device path, XKB state tracking, compositor filter, and grab stack. `[Smithay src/wayland/virtual_keyboard/mod.rs:157-173]` `[Smithay src/wayland/virtual_keyboard/virtual_keyboard_handle.rs:93-123,179-218]`

- **COSMIC compositor has a concrete race mechanism.** COSMIC processes `DeviceAdded` by inserting the device into the seat's device map; keyboard events look up the seat via `for_device(&event.device())`; if the device is not yet known to any seat, the keyboard event is silently dropped. `[cosmic-comp src/input/mod.rs:163-275]` `[cosmic-comp src/shell/seats.rs:82-127]`

- **COSMIC's always-present seat keyboard weakens the "missing keyboard capability" hypothesis.** COSMIC's `create_seat()` always calls `seat.add_keyboard(...)`, matching Smithay's recommended pattern of separating seat capability from later device hotplug. That means a fresh `uinput` failure is more likely to be "device not yet discovered/routable" than "seat lacked a keyboard object." `[cosmic-comp src/shell/seats.rs:184-236]` `[Smithay src/wayland/seat/mod.rs:567-586]`

- **The reliability split tracks device lifetime model, not backend choice.** Controlled experiments show roughly **95% success** with one persistent injector/device and roughly **20% success** with per-job device creation, with the same uinput backend, same compositor, and same Ghostty target.

- **A simple "wait after hotkey release" did not solve it.** The 250ms settle-delay experiment after talk-key release was tried and rejected. `[HANDOFF-raw-ptt-paste-gap-2026-03-08.md:391-406]`

- **Current telemetry already shows the failure is downstream of clipboard prep and route selection.** Logs showed successful daemon finalization, queued jobs, no worker failures, no explicit backend failures, yet operator-visible "no paste." `[HANDOFF-raw-ptt-paste-gap-2026-03-08.md:183-242]`

- **Child-side reports also showed good-looking clipboard and routing while visible paste still failed.** For inspected runs: `clipboard_ready=true`, `post_clipboard_matches=Some(true)`, backend `uinput:ok`, Ghostty routed as `Terminal` with `CtrlShiftV`, yet paste still failed visibly. `[HANDOFF-raw-ptt-paste-gap-2026-03-08.md:347-389,422-437]`

- **`8564084` fixed a real bug, but knowingly gave up warm-device reuse.** The handoff correctly states that startup-time injector caching froze `/dev/uinput` availability for the whole session, while per-job rebuild restored late recovery at the cost of losing whole-session device reuse. `[HANDOFF-raw-ptt-paste-gap-2026-03-08.md:57-100]`

## 2. Inferences

- **The evidence supports "fresh virtual device discovery/routing race" as the primary hypothesis.** Not proven, but it is the best-supported explanation because:
  - it has a direct mechanism in COSMIC/Smithay (`DeviceAdded` / device-map registration vs keyboard event ordering),
  - Linux uinput docs explicitly warn about this class of race,
  - and it cleanly explains the large 95% vs 20% split while the backend, compositor, and app stay constant.

- **The Smithay evidence weakens "seat keyboard capability missing" as a primary explanation.** Smithay expects compositors to create the logical seat keyboard up front with `add_keyboard()`, separate from backend hotplug. COSMIC follows that model. So the stronger boundary is "fresh backend device not yet active for routing," not "keyboard object does not exist yet."

- **Persistent warm uinput likely helps for more than setup overhead.** A persistent device is not just "faster to create"; it changes semantics because the compositor/libinput have already discovered, classified, and seated the device before the first real paste chord. That is a real behavioral difference, not mere micro-optimization.

- **Wayland virtual-keyboard protocol findings do not undercut the uinput hypothesis.** Smithay's virtual-keyboard path bypasses the backend-device discovery and routing path entirely, so it should not be treated as contrary evidence about `/dev/uinput` behavior. It is a different delivery mechanism with different failure modes.

- **`a90b9ef`'s benefit cannot be attributed purely to subprocess removal.** The system is already in-process today, yet reliability is poor again when it recreates the uinput device per job. That means "subprocess → in-process" alone is not sufficient. Current evidence points to device lifetime/warmth as a necessary part of the improvement, even if its exact share versus subprocess effects is not yet isolated.

- **`8564084` is both correct and a regression.** It is a correct review-safe stopgap because it fixes the real "startup poison" bug. It is also a likely reliability regression because it reintroduces the fresh-device timing boundary on every paste attempt.

- **The symptom pattern is consistent with partial chord delivery inside the broader fresh-device race.** Because Parakeet emits `Ctrl`, `Shift`, and `V` as separate `emit()` calls, if the compositor starts routing the device mid-chord or misses early packets from a just-created device, the app may see:
  - naked `V`,
  - `Shift+V`,
  - release-only cleanup,
  - or layout-dependent text instead of paste.

  That matches "first/last char truncation" and occasional Cyrillic/wrong-layout output better than a pure focus bug does, but this remains an inference rather than a directly captured packet trace.

- **A hotkey-release timing bug is not the best primary explanation anymore.** The failed 250ms settle-delay experiment weakens "just too early after release" as root cause. The more specific device-enumeration race is more compelling.

## 3. Best-Supported Design

### Persistent while healthy, lazy/retryable init on demand, plus a one-time post-create warm-up wait

This is effectively **C + D + E** from the candidate list, not A/B/F.

#### Design

1. **Keep one in-process worker** as today.
2. Replace "build injector per job" with a small **persistent sender manager**:
   - `None` initially
   - lazily create `UinputChordSender` on first paste attempt
   - keep it for subsequent jobs while healthy
3. **If creation fails**, do not poison the session forever:
   - remember the error
   - retry on later jobs with bounded backoff
4. **If a sender was freshly created**, wait a bounded **device warm-up delay** before the first real chord.
   - Start with something like **150–250ms**
   - make it internal/tunable for experiments
   - treat this as a readiness hedge for newly created devices, not as a general per-paste delay
5. **Do not auto-resend the real paste chord** on silent failures.
   - There is no reliable app-level ack
   - a blind retry risks double-paste
6. **On explicit send I/O error**, mark the sender unhealthy and recreate on the next job.
   - Do not resend the same job automatically unless you can prove zero events were emitted
7. **If stronger readiness evidence becomes available later**, prefer it over a pure time delay.
   - Example: if deeper libinput/Smithay research reveals a better observable than elapsed time, replace or tighten the warm-up heuristic

#### Why this is the best fit

- It preserves the main reliability property from `a90b9ef`: a **warm discovered device**
- It preserves the recovery property from `8564084`: **late `/dev/uinput` recovery**
- It directly addresses the only mechanism that explains the large persistent-vs-fresh reliability gap
- It matches Smithay/COSMIC's model: the seat keyboard already exists, so the missing piece to stabilize is backend-device discovery/readiness, not seat capability setup
- It is a relatively small code change, not a new backend or architectural rewrite
- It keeps the reliability-critical state at the right layer: persist the device handle, not a startup decision that can never recover

#### Minimal state machine

```rust
enum UinputState {
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

Send path:

- if `Healthy` → send immediately
- if `Uninitialized` or retry window elapsed → create
- after create → sleep until `created_at + warmup_ms` before first send
- on explicit send error → drop sender, transition to `CreateFailed`/`Uninitialized`

## 4. Why Not the Alternatives

- **A. Rebuild per injection/job (current): no.**
  Maximizes exposure to the fresh-device race. This is the exact model associated with the ~20% success path. It solves recovery but fights reliability every time. `[main.rs:835-839]`

- **B. Build once at startup, keep forever: no.**
  Reliability may be good, but startup-time `/dev/uinput` unavailability poisons the whole session. Review concern was real, so this is not an acceptable final design. `[HANDOFF:57-66]`

- **D alone. Lazy-create once, then keep: almost, but incomplete by itself.**
  Close to the recommended design, but without retry/re-init after failure, you still reintroduce session poisoning. Without a post-create warm-up, you still leave the first-use race untreated.

- **Wayland virtual-keyboard protocol as a quick replacement: no, not on current evidence.**
  Smithay's virtual-keyboard path is a materially different delivery model that bypasses the backend-device path, compositor filter, and grab handling. That makes it a separate product/compatibility decision, not a drop-in fix for the current `uinput` regression.

- **E alone. Explicit warm-up wait after every fresh create: useful, but not sufficient alone.**
  Recreating the device every job still pays avoidable create/destroy churn and still depends on a timing guess every time. Better to pay that cost only on first creation or recovery.

- **F. Priming before real paste: not defensible as shipped behavior.**
  Space/backspace is not semantically neutral. It can corrupt the target buffer if only half lands. It may mask the bug rather than fix it. Keep this as diagnostic-only if you must test it. `[HANDOFF:102-112]`

- **Sysfs readiness check as the main fix: no.**
  Seeing `/dev/input/eventX` or a sysfs node only proves kernel-side creation. It does not prove libinput/COSMIC has fully discovered and associated the device for routing. Smithay/COSMIC make that association in later backend/compositor layers. Good for telemetry, weak as a correctness gate.

## 5. Unknowns / Residual Risk

- **No true target-side ack exists yet.** Current logs can prove clipboard correctness and backend "success," but not that Ghostty actually consumed the shortcut. `[HANDOFF:224-242,347-389]`

- **Some residual failures may still be app/compositor semantic acceptance issues.** Even with a warm persistent device, the remaining ~5% gap suggests there may be additional edge cases in Ghostty/COSMIC/libinput interaction.

- **The current 95% vs 20% result is highly directional but should be tightened into a repeatable gate.** It is already strong enough to guide design, but the final merge argument should rely on a repeatable harnessed matrix, not memory of ad hoc runs.

- **Smithay/libinput discovery details are still not fully grounded.** The Smithay result clarifies seat and routing semantics, but it does not by itself prove exactly when libinput/COSMIC consider a newly created `uinput` keyboard fully active for first-event delivery.

- **The ideal warm-up duration is unknown.** Kernel docs use `sleep(1)` as a conservative example, but that is too expensive for UX. The right product number should be measured, not guessed. `[codemap-deepwiki-uinput.md:60-68]`

- **There may still be secondary keymap/modifier-state issues.** The Cyrillic/truncation symptoms fit partial early-packet loss, but could also involve layout state in the compositor/app.

- **If creation succeeds but first-use still silently fails, production cannot safely "retry the chord" without risking duplicate paste.** That is why observability matters before adding any automatic duplicate-send behavior.

## 6. Recommended Validation Plan

### A. Add telemetry before locking the design

Add these fields to every paste attempt:

- `uinput_sender_generation`
- `uinput_fresh_device: bool`
- `uinput_device_age_ms_at_first_key`
- `uinput_use_count`
- `uinput_created_this_job: bool`
- `uinput_create_elapsed_ms`
- `uinput_last_create_error`
- `uinput_reused_after_failure: bool`
- target app / route / shortcut (already partly present)

This is the key missing discriminator: **fresh vs reused device at chord time**.

### B. Run a focused A/B matrix

On COSMIC + Ghostty, and then Brave:

1. **Current** per-job rebuild
2. **Per-job rebuild + explicit post-create warm-up**
3. **Persistent + retryable re-init**
4. **Persistent + retryable re-init + one-time post-create warm-up** ← expected winner

Run at least **50–100 injections per cell** with unique tokens and record:

- visible paste success
- duplicate paste
- truncation
- wrong-layout/Cyrillic artifacts

### C. Validate late `/dev/uinput` recovery explicitly

Test the scenario that motivated `8564084`:

1. Start Parakeet with `/dev/uinput` unavailable
2. Attempt paste (expect error/copy-only fallback)
3. Restore `/dev/uinput` access without restarting
4. Next injection should create the sender and succeed

If this passes, you keep the review fix while restoring warm-device reliability.

### D. Improve the target-side harness

The existing Ghostty sink artifact was not authoritative. `[HANDOFF:336-341]`

Before declaring victory, use a harness that reliably captures what Ghostty actually receives.

### E. Decide shipping criteria

Do not call this fixed until:

- Ghostty success is in the **high 90s over repeated runs**
- Brave also passes
- no duplicate pastes appear
- late `/dev/uinput` recovery works without restart
- raw dictation and LLM-answer paths both pass

### F. Escalation path (only if recommended design still underperforms)

Then escalate to compositor/app-level investigation:

- confirm whether COSMIC/libinput delivers first events from newly created uinput keyboards consistently
- inspect Ghostty handling of synthetic modifier chords
- use a diagnostic-only priming experiment to test the "first event is lost" theory, but do not ship it by default

## 7. Final Recommendation

Implement the persistent-but-recoverable `uinput` sender next.

Concretely:

1. Keep the in-process worker.
2. Stop rebuilding `UinputChordSender` for every job.
3. Store sender state across jobs.
4. Recreate only on first use, after explicit failure, or after prior create failure once retry/backoff allows it.
5. Apply a bounded warm-up only when a device was freshly created.
6. Add the fresh-vs-reused telemetry before calling the design done.

The reason is simple: the evidence points to persisting the wrong layer as the core mistake. `8564084` fixed startup poisoning by rebuilding everything each time, but the reliability-sensitive thing is the discovered `uinput` device, not the abstract injector configuration. The best-supported correction is therefore not "cache everything forever" and not "recreate everything every time," but "keep the device alive while it is healthy, and be willing to recreate it when reality changes."
