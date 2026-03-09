# Handoff: Raw PTT Paste Gap Under `stt` / `stt llm` (2026-03-08)

This file is the canonical running log for this bug investigation.

Use it to record:

- hypotheses tested
- code changes made
- repro results observed
- options ruled out

Goal:

- prevent repeating dead-end experiments
- keep cross-session debugging grounded in the same evidence trail

## Status

This issue is **not solved**.

The latest attempted fix was based on the wrong root-cause hypothesis and has been reverted. The codebase should be treated as back to the pre-experiment runtime behavior.

## Status Update (2026-03-08, later debugging pass)

The investigation has now moved from theory-driven fixes to **side-by-side path instrumentation**.

What was added in the current local worktree:

- path tags on injection jobs and reports:
  - `raw_final_result`
  - `llm_answer`
  - `demo`
- enqueue-time timing fields for both paths:
  - elapsed since the most recent hotkey-up
  - elapsed since the matching `stop_session` send
- worker-start timing fields for the same job, so queue delay and "time since release" are separated
- enqueue-side logs for:
  - raw final-result jobs
  - LLM-answer jobs
- worker-start logs for all injection jobs
- report logs now include the same origin/timing fields on success or failure

Result of this pass:

- No runtime behavior was intentionally changed.
- `cargo fmt` passed.
- `cargo test` passed in `parakeet-ptt`.
- The repo now has a better "flight recorder" for comparing raw-fail versus LLM-success on the same surface.

What this did **not** prove yet:

- It does not yet identify the root cause by itself.
- It does not yet show whether Ghostty receives the wrong shortcut, the wrong focus target, or a syntactically correct shortcut at the wrong input-state boundary.
- A live Ghostty repro capture is still required.

## Status Update (2026-03-08, operator repro after instrumentation)

The operator ran `stt llm` and reported this sequence:

1. first raw dictation: no visible paste
2. first LLM query: no visible paste
3. second raw dictation: paste succeeded
4. second LLM query: no visible paste and operator reported clipboard did not update

### What the logs confirm for that run

The inspected sessions on the current instrumented build were:

- `964c1646-2442-4e95-9434-da69f4c411a4` raw
- `6826a897-2862-4a4a-955b-50ba762bc311` LLM query
- `337ad445-c883-4c02-bc62-96d6f77510cd` raw
- `230f6aee-ccfa-43a8-a753-8d20e5d7f508` LLM query
- later additional pair:
  - `c98524cf-3471-48e9-a4c2-6eda7f79f1b5` raw
  - `30fe27e5-d731-4ecb-b687-433ef12e7262` LLM query

For the four-session sequence at `18:53` to `18:54` UTC, the logs show:

- both LLM utterances were correctly classified as `intent=LlmQuery`
- all four sessions reached daemon finalization successfully
- all four injection jobs were queued
- all four injection jobs started immediately (`queue_wait_ms=0`)
- all four injection jobs completed successfully from the parent worker’s point of view
- there were no enqueue timeouts
- there were no worker failures
- there were no explicit backend failures

Observed enqueue timing split in those sessions:

- raw jobs were enqueued about `305-307ms` after hotkey-up
- LLM jobs were enqueued about `725-860ms` after hotkey-up

This preserves the earlier structural contrast:

- raw path injects sooner while state is `waiting_result`
- LLM path injects later while state is already `idle` and session-end handling was deferred

### What the daemon confirms

The daemon finalized every inspected session successfully and produced transcript text in all of them.

Examples from the inspected window:

- raw session `964c1646...`: `text_len=69`
- LLM-query transcript session `6826a897...`: `text_len=49`
- raw session `337ad445...`: `text_len=57`
- LLM-query transcript session `230f6aee...`: `text_len=65`
- later LLM-query transcript session `30fe27e5...`: `text_len=85`

So for the runs inspected so far:

- ASR did succeed
- daemon finalization did succeed
- LLM query intent capture did succeed

### Contradiction now established

The strongest new fact is this:

- the operator observed "no visible paste" and in one case "clipboard did not update"
- but the current parent-side runtime logs still show successful completion for the corresponding injection jobs

That means the present observability is still missing the exact failure boundary.

The system currently proves:

- daemon produced text
- parent client queued injection
- parent worker ran the internal injector subprocess to a successful exit

The system does **not** yet prove:

- what the internal injector subprocess actually routed to on successful runs
- whether `wl-copy` successfully wrote the expected text on those runs
- whether the target app accepted the synthetic chord
- whether Ghostty received paste but dropped it semantically

### Important observability gap discovered

The new parent-side instrumentation in `main.rs` is useful, but it exposed a second gap:

- the actual clipboard/focus/routing logic runs in the internal injector subprocess
- successful subprocess runs do not currently stream their detailed `injector.rs` info logs back into the parent log

Practical consequence:

- we can now compare raw-vs-LLM enqueue timing and parent worker behavior
- but we still cannot see per-run successful child details such as:
  - resolved focus snapshot
  - chosen route shortcut
  - clipboard readiness outcome
  - final injector outcome classification

This is now one of the highest-value next instrumentation targets.

### Additional signals seen during the repro window

Repeated hotkey listener device churn warnings happened near these utterances:

- `/dev/input/event18` produced `No such device (os error 19)` and `No such file or directory (os error 2)` warnings

Important nuance:

- those warnings happened around both raw and LLM runs
- they did **not** prevent LLM intent capture in the inspected sessions
- they may still be relevant as an input-state disturbance, but current evidence does not show them cleanly explaining why one utterance visibly pastes and another does not

### Ghostty sink artifact status

The staged Ghostty sink file `/tmp/parakeet-ghostty-sink.txt` did not provide a confirmed capture for the inspected failure sequence.

That means the current run gives us:

- strong client/daemon-side facts
- but not a definitive target-side artifact for what actually landed in Ghostty during the observed failures

## Status Update (2026-03-08, later operator observation)

The operator reported a new contradictory result after the earlier repro:

- for the first time since the bug appeared, a plain direct raw transcript did auto-paste successfully
- the LLM-query runs in that same period still failed to paste

Why this matters:

- it weakens the earlier working assumption that the bug is cleanly explained by "raw path bad, LLM-answer path good"
- it means the divergence may still be related to raw-versus-LLM path differences, but current evidence no longer allows that to be treated as the primary proven split
- at minimum, the failure now looks less like a deterministic route distinction and more like an intermittent condition that can affect both paths

Practical interpretation:

- do **not** assume the LLM path is a stable known-good control anymore
- use per-run evidence before drawing conclusions from path labels alone
- keep comparing raw and LLM behavior, but treat "LLM usually works" as a historical observation, not a guaranteed invariant

## Status Update (2026-03-08, post-reset Ghostty repro with tmux harness)

After a full reset (`stt llm stop`, tmux cleanup, log truncation, fresh `stt llm start`), the operator reported this result for a 6-utterance alternating run:

1. raw #1 pasted
2. llm #1 did not paste
3. raw #2 pasted
4. llm #2 did not paste
5. raw #3 did not paste
6. llm #3 pasted

The corresponding sessions in `/tmp/parakeet-ptt.log` were:

- `033d9a69-2cd4-41d5-9d0c-38fd54627e95` raw
- `deb844f2-0f5d-43e4-9f20-f9311fe16ad9` llm query
- `6774787d-ce5f-4e82-aa67-8f9ef1784bf0` raw
- `c8f1e972-51a7-4ae6-bcf3-88009c35c6db` llm query
- `40f834b2-002e-48ce-9df2-db57f9f1a947` raw
- `b7c782da-9eea-4a44-8f95-da8593765af3` llm query

What logs confirm for this run:

- all six sessions started and stopped normally
- daemon finalization succeeded for all six sessions with non-empty transcript lengths
- all six injection jobs were queued
- all six injection jobs started immediately (`queue_wait_ms=0`)
- all six injection jobs were reported as successful completion by the parent worker

Observed enqueue timing split in this run:

- raw jobs: about `303-306ms` after hotkey-up
- llm jobs: about `521ms`, `817ms`, and `1934ms` after hotkey-up

So the structural timing difference (raw sooner, llm later) still holds, but success/failure remains mixed on both paths.

Ghostty sink artifact status in this run:

- `/tmp/parakeet-ghostty-sink.txt` captured only one visible line:
  - `Raw one alpha transcription in ghosty sing.`

This is weaker than the operator-observed sequence, and indicates that at least some visible pastes did not land in the sink capture artifact itself (focus/routing drift during repro remains possible).

Additional signals in the same window:

- repeated hotkey listener churn warnings continued around utterances:
  - `/dev/input/event18`: `No such device (os error 19)`
  - `/dev/input/event18`: `No such file or directory (os error 2)`

New observability note from this run:

- the parent log did **not** contain `injector subprocess log line` entries in this capture window
- so child-side per-success focus/route/clipboard lines are still not present in this specific artifact set
- this keeps the key boundary unresolved for now: successful parent worker reports still do not prove what exact child-side target routing happened on each attempt

## Problem Statement

Plain push-to-talk raw dictation does **not** auto-paste reliably after release.

That failure reproduces in the "raw transcript should be pasted directly" path:

- `stt`
- `stt start`
- `stt llm` when used as plain dictation without invoking the LLM answer path

Historically, the **LLM answer injection path** has often pasted correctly:

- `stt llm` with the LLM query modifier held
- transcript goes to the model
- final model answer is injected successfully

However, later operator observations showed at least one period where a plain raw transcript pasted successfully while LLM-query runs still failed to paste.

So the issue should now be framed more carefully as an **intermittent auto-paste failure affecting final injection behavior**, with raw plain PTT still the most commonly reported failing path, but not the only one that can misbehave.

## What Is Confirmed

These facts are the current working baseline:

- Raw transcription itself works.
- The raw transcript is produced.
- The clipboard is updated with the raw transcript.
- Manual paste of that clipboard content works.
- LLM answer injection has succeeded in prior reproductions, but is no longer safe to assume as a consistently working control path.
- Therefore the failure boundary is between "raw final result is ready" and "target surface receives the synthetic paste as intended".

In other words: the pipeline is getting the text to the clipboard, but the raw path is not completing the last step at the UI boundary.

## What Was Tried And Rejected

### Rejected hypothesis: raw path fires paste too early after hotkey release

We tested the theory that plain raw dictation pastes before the target surface is ready again after hotkey-up, and that LLM mode only works because model latency accidentally acts like a debounce buffer.

That experiment added:

- per-session hotkey-up timestamps
- a `not_before` deadline on raw final-result injection jobs
- a fixed 250ms raw-only settle delay before injection
- `settle_wait_ms` reporting
- timing tests around that artificial delay

Result:

- It **did not change or solve the real problem**.
- Therefore the "focus-settle delay" theory is not the root cause, or at minimum not a sufficient explanation.

Action taken:

- The runtime change was reverted.
- The troubleshooting doc was reverted so it does not describe a false fix.
- The timing tests for that hypothesis were also removed.

## Why The Rejected Fix Still Taught Us Something

The failed experiment still narrowed the search space:

- The gap is not simply "add a small wait after release and raw paste starts working."
- The difference between raw dictation and LLM-answer injection is more structural than a fixed 250ms delay.
- The next investigation should compare the **full raw injection path** against the **LLM-answer injection path**, not only their wall-clock timing.

## Most Likely Remaining Problem Area

The useful contrast is now:

- Raw dictation path: transcript reaches finalization and often fails to auto-paste.
- LLM answer path: has succeeded in prior runs, but newer operator evidence shows it can also fail to paste.

That suggests the next engineer should inspect **what is materially different between those two injection triggers**, for example:

- exact route into `injector_worker.enqueue(...)`
- state transitions around session end/reset
- hotkey/input state at the moment synthetic paste is sent
- whether the raw path and LLM path observe different focused-surface or routing conditions
- whether the raw path is colliding with talk-key lifecycle in a way the later LLM path avoids

The key point: investigate **behavioral divergence between injection attempts and their surrounding desktop/input state**, not ASR quality and not only clipboard write success.

Refined current hypothesis after reading the code:

- Raw dictation and LLM answer injection both converge on the same injector worker and the same clipboard/paste backend.
- The main structural difference is **when and under what session/input-state conditions** they enqueue work.
- Raw dictation enqueues directly on daemon `FinalResult`.
- LLM mode resets state earlier, defers daemon session-end handling, waits for model completion, and only then enqueues the final answer.
- Therefore the bug is still more likely to involve **desktop/input-state divergence at injection time** than a generic ASR failure, but it may not map cleanly onto a simple raw-path-versus-LLM-path split.

Practical interpretation:

- Earlier evidence suggested that if the same injector succeeded later in LLM mode, the injector itself was not the first thing to blame.
- Newer evidence weakens that inference, because LLM-query injections can also fail visibly.
- The more promising search area is now the state around any enqueue/injection attempt, with special attention to:
  - session lifecycle
  - hotkey release lifecycle
  - focus snapshot/routing decision
  - target-surface readiness as actually observed by the app, not as inferred by a fixed delay

## Reproduction Summary

### Failing path

1. Start `stt` or `stt llm` or `stt off`
2. Use normal raw dictation (plain PTT, no LLM query modifier). 
3. Release the talk key.
4. Observe:
   - transcript is recognized
   - clipboard updates
   - automatic paste into the target surface does not happen correctly

note : user tested with pure offline, streaming and overlay off and the same bug happens (clipboard updates, auto-pasting fails, manual pasting return correct transcript)

### Working path

1. Start `stt llm`.
2. Use the LLM query path.
3. Wait for the model answer.
4. Observe:
   - final answer is injected successfully
   - paste behavior works in the same general environment

## What Not To Spend Time On First

Based on current evidence, these are poor first bets:

- blaming transcription quality
- blaming daemon finalization
- blaming clipboard write failure
- re-trying the same fixed-delay-after-release idea without new evidence

## Recommended Next Debugging Work

1. Completed locally: add side-by-side logging for raw-final-result enqueue versus LLM-answer enqueue, with the goal of proving exactly where the two paths diverge.
2. Next: capture live Ghostty traces using the new origin/timing fields and compare one raw-fail run against one LLM-success run.
3. Capture route/focus/input-state evidence at the moment the paste backend is actually invoked for each path.
4. Verify whether the raw path reaches the same paste backend and shortcut plan as the LLM answer path.
5. Check whether the talk-key lifecycle or modifier state is still relevant when raw paste fires, even though a fixed 250ms delay did not fix it.
6. After Ghostty is understood, repeat the same comparison in Brave to determine whether the bug is shared across surfaces or only looks shared from the operator side.

## Ghostty Repro Harness

A detached tmux repro harness has been staged locally to make Ghostty-focused debugging repeatable:

- session: `parakeet-debug`
- window `ghostty-sink`:
  - intended as the focused Ghostty paste target
  - runs a sink that mirrors received pasted text into `/tmp/parakeet-ghostty-sink.txt`
- window `ptt-logs`:
  - tails `/tmp/parakeet-ptt.log`
  - tails `/tmp/parakeet-daemon.log`

Purpose:

- compare the user-visible result in Ghostty with the new raw-vs-LLM log fields
- preserve a simple text artifact of what actually landed in the target sink

Current status of the harness:

- prepared, but not yet used to capture a confirmed raw-fail versus LLM-success pair in this pass

## New Evidence To Look For In Logs

During live repro, the most useful new fields are:

- `origin=raw_final_result` versus `origin=llm_answer`
- `state_at_enqueue`
- `hotkey_up_elapsed_ms_at_enqueue`
- `stop_message_elapsed_ms_at_enqueue`
- `hotkey_up_elapsed_ms_at_worker_start`
- `stop_message_elapsed_ms_at_worker_start`
- `injector worker starting job`

The immediate debugging question is:

- do raw-fail and LLM-success already diverge **before** the injector backend runs, or do they look equivalent until backend execution?

If they diverge before backend execution, fix the upstream state/routing logic.

If they do not diverge before backend execution, the next suspect becomes backend/input semantics under the recent talk-key teardown boundary.

## Acceptance Criteria For The Real Fix

The issue is only solved when all of the following are true:

- Plain raw PTT auto-pastes correctly in Ghostty.
- Plain raw PTT auto-pastes correctly in Brave.
- `stt llm` raw dictation path also auto-pastes correctly when not invoking the model answer path.
- Shift/LLM-query mode still pastes the LLM answer correctly.
- Clipboard contents remain correct in both raw and LLM paths.

## Repo State After This Handoff

- No new runtime behavior from the rejected 250ms-settle experiment remains.
- No new CLI or env surface was introduced for that experiment.
- Current local debugging worktree now also contains:
  - non-behavioral raw-vs-LLM injection instrumentation in `parakeet-ptt/src/main.rs`
  - this updated handoff document
- The repo should still be treated as carrying the original unresolved bug.

## Status Update (2026-03-08, fresh restart after child report + guarded parent-focus routing)

This section supersedes the earlier "non-behavioral instrumentation only" note above.

During this pass, a real runtime change was landed in the local worktree, not just logging.

### What changed in code

The following files now contain the current first-fix experiment:

- `parakeet-ptt/src/main.rs`
- `parakeet-ptt/src/injector.rs`
- `parakeet-ptt/src/surface_focus.rs`

What this patch added:

- parent captures a Wayland focus snapshot at hotkey-up and threads it into each `InjectionJob`
- child injector emits one explicit machine-readable stderr line per job:
  - `PARAKEET_INJECT_REPORT {json}`
- parent parses that child report and re-logs it as a structured `injector subprocess report`
- child routing now prefers the parent-captured focus snapshot when it is fresh and the child-local snapshot is unavailable / low-confidence / route-disagreeing
- `FocusSnapshot` was made serializable so the snapshot can cross the parent -> child boundary

Practical effect:

- we now have a flight recorder for each paste attempt
- the system can finally distinguish "clipboard prepared" from "target visibly pasted"
- the new route-selection guard is active during repros and can be confirmed from logs

### Fresh repro setup

The operator restarted the stack and reported:

- all visible paste attempts failed on every tested surface
- except the first raw transcript, which visibly pasted once

Live artifacts inspected for this pass:

- `/tmp/parakeet-ptt.log`
- tmux session `parakeet-debug`, window `ghostty-sink`
- `/tmp/parakeet-ghostty-sink.txt`

Important artifact note:

- the `ghostty-sink` pane did not capture any pasted transcript text after restart
- `/tmp/parakeet-ghostty-sink.txt` remained size `0`

So for this run, Ghostty did not provide a target-side confirmation artifact even though the client reported multiple successful injection jobs.

### What the fresh logs prove

The new child reports are now present and working.

Across the inspected post-restart sessions, every injection job still reported:

- `outcome="success_assumed"`
- `clipboard_ready=true`
- `post_clipboard_matches=Some(true)`
- `backend_attempts="primary:uinput:ok"`

The new focus/routing evidence by surface:

- Ghostty-focused runs reported:
  - `parent_focus_app="com.mitchellh.ghostty"`
  - route class `Terminal`
  - primary shortcut `CtrlShiftV`
- Brave-focused run reported:
  - `parent_focus_app="brave-browser"`
  - route class `General`
  - primary shortcut `CtrlV`
- Zed-focused runs reported:
  - `parent_focus_app="dev.zed.Zed"`
  - route class `Unknown`
  - primary shortcut `CtrlShiftV`

The new guarded parent-focus routing was actually exercised in most runs:

- `route_focus_source="parent_focus_fresh_child_unavailable"`
- child-local cache usually reported:
  - `child_focus_source_selected="wayland_unavailable"`
  - `child_focus_wayland_fallback_reason="wayland_no_activated"`
  - cache age about `20-30ms`

One useful exception also appeared:

- session `4aad0fba-1dac-4847-b92f-fa4c9da15d86` (`llm_answer`, Ghostty)
- child-local focus was available:
  - `child_focus_before_app="com.mitchellh.ghostty"`
  - `child_focus_source_selected="wayland_cache"`
- but the chosen route was still the same Ghostty terminal route:
  - `CtrlShiftV`

So the new instrumentation shows two things at once:

- the parent-focus fallback is active and behaving as designed
- even when child-local focus is available, the chosen route does not materially change for Ghostty

### Sessions inspected in this post-restart run

Representative sessions from `/tmp/parakeet-ptt.log`:

- `e4807b99-1e76-4dc5-9f3c-b8f3a2d345fd` raw, Ghostty
- `ec20ddbe-ba24-4743-af46-1d38345039f7` raw, Ghostty
- `5c98b944-9010-4534-b6b7-6af6b5d350a9` raw, Ghostty
- `61849542-efe2-43ba-9cd1-48a5b1f449e3` raw, Ghostty
- `3eb0ac55-5cd5-4035-8506-9d28dbe53dc9` raw, Ghostty
- `9c7ed04d-3188-4236-9d07-a0aac06e48c3` raw, Brave
- `4aad0fba-1dac-4847-b92f-fa4c9da15d86` llm answer, Ghostty
- `8de08dec-93cc-490b-b412-9bf58b36411e` llm answer, Ghostty
- `2b48e086-eb28-4698-991a-651d4d4d4826` raw, Ghostty
- `5b57b4ec-3590-4abb-a4c8-06e5a10faf04` llm answer, Ghostty
- `5c2f01b4-7687-47d6-8e80-947c9aec3807` raw, Zed
- `28e6c0e5-0740-4a60-885a-8b64e6d9efba` llm answer, Zed
- `7f4c8db9-4ab3-42ed-9be8-3107e5f47e79` raw, Ghostty

Observed enqueue timing pattern still holds:

- raw jobs enqueue about `296-309ms` after hotkey-up
- llm answer jobs enqueue about `764-827ms` after hotkey-up

But the visible failure is now mixed across surfaces while the child report still looks syntactically healthy.

### What this result means

This fresh repro is important because it weakens the earlier "wrong focus snapshot" theory as the primary explanation.

Why:

- Ghostty runs now explicitly report the expected Ghostty route (`CtrlShiftV`)
- Brave run explicitly reports the expected browser/editor route (`CtrlV`)
- clipboard preparation succeeds
- the synthetic key backend reports success
- but visible paste still fails according to the operator

That is like proving the package was addressed correctly, loaded onto the truck, and marked "delivered" while the recipient still never got it. The remaining suspicious boundary is the handoff at the door, not the address label.

Refined interpretation after this run:

- the new parent-focus routing fix did not solve the bug
- simple late-focus drift is no longer the best single explanation for Ghostty / Brave failures
- the leading remaining hypothesis is now a semantic backend failure:
  - `uinput` emits a syntactically successful chord
  - Wayland / compositor / target app intermittently ignores or drops it

One narrower secondary issue is still visible:

- Zed is classified as `Unknown` and therefore gets `CtrlShiftV`
- that may be wrong for Zed specifically
- but it cannot explain the Ghostty and Brave failures seen in the same run

### Current best next move

Keep the new report plumbing.

Do not revert the parent-focus capture yet; it is now valuable evidence even if it was not a sufficient fix.

The next minimally risky runtime experiment should target backend semantics rather than focus routing:

- in `paste_key_backend=auto`, if the first backend reports a successful command exit but target-side effect is still suspected missing, retry the same shortcut once via the alternate backend under tight guards

Reason:

- this run now shows correctly prepared clipboard + plausible route + `uinput` command success can still correspond to "nothing visibly pasted"
- that makes backend semantic acceptance the next best boundary to test

## Status Update (2026-03-08, cleanup after proving parent-guided routing was not the fix)

After the fresh repro above, the local worktree was cleaned up to separate durable observability from disproven runtime behavior.

### Kept in the tree

These changes remain and should be considered part of the current debugging baseline:

- explicit child injector report channel:
  - `PARAKEET_INJECT_REPORT {json}`
- parent parsing and structured logging of child reports
- enqueue / worker timing fields on injection jobs
- parent focus capture at hotkey-up
- parent focus transport into the child context
- `FocusSnapshot` serde support so focus snapshots can cross the process boundary

Why these stay:

- they are diagnostic infrastructure, not a theory disguised as a fix
- they already paid for themselves by proving the current failure boundary more precisely

### Removed from the tree

The guarded runtime routing override was removed again.

What was removed:

- child route selection no longer prefers the parent-captured focus snapshot
- parent focus is now evidence only, not a live routing input

Why it was removed:

- the fresh repro showed correct-looking route selection on Ghostty and Brave even while visible paste still failed
- that means the override added complexity without solving the problem

Practical interpretation:

- keep the black box flight recorder
- remove the autopilot tweak that did not fix the crash

### Current repo state after cleanup

The current local tree should now be understood as:

- richer instrumentation than before
- no parent-guided route override
- bug still unresolved
- leading next hypothesis: semantic backend failure after apparently successful synthetic chord emission

### Canonical next option to test

If continuing from this state, the next minimally risky runtime experiment is:

- in `paste_key_backend=auto`, retry the same shortcut once through the alternate backend when the first backend exits successfully but target-side effect is still suspected missing

This is now the best next option because:

- focus/routing evidence is materially better than before
- the remaining contradiction is between "command says it sent the chord" and "target visibly did nothing"

## Status Update (2026-03-09, matrix harness added)

To reduce operator error in the next backend-semantics pass, the repo now contains a dedicated local harness:

- `scripts/paste-gap-matrix.sh`
- `just paste-gap-start`
- `just paste-gap-stop`
- `just paste-gap-diag`
- `just paste-gap-summary`
- `just paste-gap-current`

What it does:

- records baseline commit SHA and worktree status before each backend run
- clears `/tmp/parakeet-ptt.log`, `/tmp/parakeet-daemon.log`, and `/tmp/parakeet-ghostty-sink.txt`
- starts `stt` with one explicit backend and `--paste-backend-failure-policy error`
- archives the runtime artifacts after the manual Ghostty run
- mechanically extracts `injector subprocess report` rows into TSV files
- seeds operator-observation templates so visible-paste results can be recorded next to the parsed reports

What it does **not** do:

- it does not change injector routing or backend behavior
- it does not remove the need for a live Ghostty operator repro
- it does not yet answer the backend question by itself

Practical effect:

- the next Ghostty matrix run can now be repeated with less manual bookkeeping
- evidence capture is standardized before any behavioral change is attempted

## Status Update (2026-03-09, Ghostty backend matrix completed)

The Ghostty raw-path backend matrix has now been run with the local harness.

Run directories:

- `uinput`: `/tmp/parakeet-paste-gap/20260309T195817Z-uinput-ghostty`
- `ydotool`: `/tmp/parakeet-paste-gap/20260309T200916Z-ydotool-ghostty`
- `auto`: `/tmp/parakeet-paste-gap/20260309T201528Z-auto-ghostty`

### What was held constant

- target surface: Ghostty sink
- utterance set: 10 fixed raw dictation phrases
- path under test: plain raw PTT only
- route chosen in all inspected runs: Ghostty paste route `CtrlShiftV`
- runtime policy: explicit backend selection for `uinput` and `ydotool`, helper default `auto` for the third run

### `uinput` run

What the archived summary proves:

- 10 raw injections reached `injector subprocess report`
- all 10 reported:
  - `origin=raw_final_result`
  - `backend_attempts=primary:uinput:ok`
  - `clipboard_ready=true`
  - `post_clipboard_matches=Some(true)`
  - `route_primary=CtrlShiftV`
  - `route_class=Unknown`
- Ghostty sink artifact captured only 3 non-empty lines

Operator-visible result recorded for this run:

- attempts reported visibly pasted: `1`, `2`, `8`
- Ghostty sink artifact contained:
  - `Gravel Raw Three Charlie.`
  - `Raw for Delta.`
  - `Raw hate hotel.`

Important contradiction preserved:

- parent + child reports still look syntactically healthy on every attempt
- target-side visible/sink results are still intermittent and do not line up cleanly with the "all good" child reports

### `ydotool` run

What the archived summary proves:

- 10 raw injections reached `injector subprocess report`
- all 10 reported:
  - `origin=raw_final_result`
  - `backend_attempts=primary:ydotool:ok`
  - `clipboard_ready=true`
  - `post_clipboard_matches=Some(true)`
  - `route_primary=CtrlShiftV`
  - `route_class=Unknown`
- Ghostty sink artifact captured 10 non-empty lines

What the target actually received:

- every captured Ghostty sink line was the same value:
  - `244442`

Additional child-side signal present on every inspected `ydotool` injection:

- child stderr emitted:
  - `ydotool: notice: ydotoold backend unavailable (may have latency+delay issues)`

Practical interpretation from this run alone:

- `ydotool` does cause a consistent target-side effect in this environment
- but the effect captured here is not the dictated transcript text

### `auto` run

What the archived summary proves:

- 10 raw injections reached `injector subprocess report`
- all 10 reported:
  - `clipboard_ready=true`
  - `post_clipboard_matches=Some(true)`
  - `route_primary=CtrlShiftV`
- route classification was:
  - `Unknown` on 9 attempts
  - `Terminal` on 1 attempt

Most important backend fact from this run:

- all 10 `auto` attempts still reported:
  - `backend_attempts=primary:uinput:ok`

So for the archived `auto` run:

- no successful fallback to `ydotool` is evidenced
- `auto` behaved like `uinput-only` from the child report’s point of view

Ghostty sink artifact for `auto` contained 5 non-empty lines:

- `Raw to Bravo.Raw through a Charlie.`
- `Raw four delta`
- `Ролсикс фор фокт.`
- `Ро Эйт Хотел.`
- `Row ten Juliet`

This means:

- the `auto` run produced intermittent target-side transcript-like output
- but current evidence does **not** show the alternate backend being used on those successful-looking sink captures

### `diag-injector` limitation in this matrix

The control run is not clean evidence in this capture set.

Why:

- the operator reported focus-follow-mouse constraints during `stt diag-injector`
- Ghostty could not stay reliably focused while launching the control from another terminal
- the resulting `Parakeet Test...` target artifact cannot be attributed with confidence to a specific backend case inside the combined `diag-injector` sequence

Therefore:

- do **not** use the current `diag-injector` results as a per-backend proof point from this pass

### Strongest new facts established by the matrix

1. Backend choice materially changes target-side behavior in Ghostty.
2. `uinput` remains intermittent even though child reports look healthy on every attempt.
3. `ydotool` produces a consistent target-side effect in this environment, but the captured effect in this run was the repeated value `244442`, not the dictated transcript text.
4. `auto` did not show evidence of falling back away from `uinput`; every archived child report still said `primary:uinput:ok`.

### What this does and does not prove

What it proves:

- a simple "if `uinput` looks successful, retry with `ydotool`" change is not yet justified as a safe fix
- because the current `ydotool` path is not demonstrated as transcript-correct in this Ghostty environment

What it does not prove:

- it does not prove whether `ydotool` has **ever** been correct historically in prior environments or earlier builds
- it does not prove the exact origin of the repeated `244442` value

### Operator note to preserve, but not elevate to proven fact

The operator stated that `ydotool` may never have been a meaningfully used or needed fallback in normal practice and may only be adding complexity now.

Treat that as a useful product/maintenance concern, not yet as a proven historical fact from repo artifacts.

### Current best interpretation after this matrix

The matrix weakens the previous "just add alternate-backend retry in `auto`" idea.

Why:

- `auto` currently never showed evidence of leaving `uinput`
- the only directly observed `ydotool` behavior in Ghostty from this pass was consistent but wrong target-side output

So the next debugging question is no longer just:

- "should `auto` retry another backend after an apparent `uinput` success?"

It is now first:

- "is `ydotool` a valid transcript-correct backend at all in the current Ghostty/COSMIC environment, especially when `ydotoold` is unavailable?"
