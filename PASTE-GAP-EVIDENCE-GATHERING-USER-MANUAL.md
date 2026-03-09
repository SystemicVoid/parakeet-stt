# Paste-Gap Evidence Gathering User Manual

This is the operator playbook for collecting reproducible evidence for the intermittent auto-paste bug.

Use this manual exactly as written for consistency across runs.

## Goal

Build clean evidence that separates:

1. synthetic key backend behavior (`uinput` vs `ydotool` vs `auto`), and
2. full push-to-talk lifecycle behavior (raw final result path under normal `stt start`).

## Scope

This guide covers both evidence modes:

1. inject-only backend isolation (`just paste-gap-inject-only`), and
2. full raw PTT path matrix (`just paste-gap-start` / `just paste-gap-stop`).

## Requirements

1. Run commands from repo root: `/home/hugo/Documents/Engineering/parakeet-stt`.
2. Keep one dedicated Ghostty window as the paste target sink.
3. Keep one separate terminal for control commands.
4. Avoid changing focus during active attempts unless a step says to.

## Focus And Mouse Discipline

This bug is focus-sensitive. Treat focus like lab contamination control.

1. If your desktop uses focus-follows-mouse, temporarily disable it before starting the session.
2. If you cannot disable it, park the mouse pointer inside the Ghostty sink window and do not move it during attempts.
3. Do not hover over other windows while attempts run.
4. Do not click notifications, docks, or panel widgets mid-run.
5. If you think focus drifted, stop and restart that run instead of trusting partial data.

## Terminal Layout

Use two terminals:

1. Terminal A (control): run `just` / helper commands from repo root.
2. Ghostty window (target sink): this is where paste effects must land.

## Step 1: Prepare The Ghostty Sink Window

Run this inside the Ghostty target window:

```bash
: > /tmp/parakeet-ghostty-sink.txt
cat | tee -a /tmp/parakeet-ghostty-sink.txt
```

Notes:

1. Keep this command running for the entire evidence pass.
2. If pasted text appears but does not hit the sink file yet, press `Enter` once to flush the line.

## Step 2: Quick Sanity Check

Run from Terminal A (repo root):

```bash
scripts/paste-gap-matrix.sh help
just --list | rg paste-gap
```

Expected: you see `paste-gap-start`, `paste-gap-stop`, `paste-gap-summary`, `paste-gap-current`, and `paste-gap-inject-only`.

## Step 3: Inject-Only Backend Isolation (Do This First)

This mode removes ASR/hotkey/session confounders and tests backend paste semantics directly.

### 3A) `uinput` Forced `CtrlShiftV` (Primary)

Run from Terminal A:

```bash
( sleep 5; just paste-gap-inject-only backend=uinput shortcut=ctrl-shift-v label=ghostty-inject-only attempts=20 prefix=PG interval_ms=150 )
```

Immediately after pressing Enter:

1. focus Ghostty sink window,
2. keep mouse parked in that window,
3. do not switch windows until run completes.

Why the `sleep 5`: gives you time to return focus to Ghostty before first synthetic chord.

### 3B) Capture Run Directory And Review

After completion, in Terminal A:

```bash
run_dir="$(just paste-gap-current)"
echo "$run_dir"
cat "$run_dir/summary.txt"
```

### 3C) Record Operator Observations

Edit the template:

```bash
nano "$run_dir/operator-observations.tsv"
```

For each attempt row:

1. set `visible_paste` to `yes` or `no`,
2. set `sink_captured` to `yes` or `no`,
3. add anything odd in `notes` (for example `244442`, duplicate paste, wrong text).

Rebuild summary after edits:

```bash
just paste-gap-summary run_dir="$run_dir"
cat "$run_dir/summary.txt"
```

### 3D) Repeat For `ydotool` And `auto`

Run one backend at a time, same focus discipline:

```bash
( sleep 5; just paste-gap-inject-only backend=ydotool shortcut=ctrl-shift-v label=ghostty-inject-only attempts=20 prefix=PG interval_ms=150 )
( sleep 5; just paste-gap-inject-only backend=auto shortcut=ctrl-shift-v label=ghostty-inject-only attempts=20 prefix=PG interval_ms=150 )
```

After each run, repeat Step 3B and 3C.

## Step 4: Full Raw PTT Path Matrix

This mode tests real behavior with daemon + hotkey lifecycle.

### 4A) Start One Backend Run

From Terminal A:

```bash
just paste-gap-start backend=uinput label=ghostty attempts=10
```

Important: this may attach to tmux. If it does, detach with `Ctrl+b` then `d` to return to your shell.

### 4B) Perform 10 Raw Dictation Attempts

With Ghostty sink focused:

1. hold talk hotkey,
2. speak one phrase,
3. release talk hotkey,
4. wait for expected auto-paste effect,
5. press `Enter` in sink only if needed to flush captured line.

Use the same 10 phrase set every run for consistency:

1. `raw one alpha`
2. `raw two bravo`
3. `raw three charlie`
4. `raw four delta`
5. `raw five echo`
6. `raw six foxtrot`
7. `raw seven golf`
8. `raw eight hotel`
9. `raw nine india`
10. `raw ten juliet`

### 4C) Stop And Archive

From Terminal A:

```bash
just paste-gap-stop
run_dir="$(just paste-gap-current)"
echo "$run_dir"
```

### 4D) Fill Observations And Rebuild Summary

```bash
nano "$run_dir/operator-observations.tsv"
just paste-gap-summary run_dir="$run_dir"
cat "$run_dir/summary.txt"
```

### 4E) Repeat Backend Runs

Repeat Steps 4A-4D for:

```bash
just paste-gap-start backend=ydotool label=ghostty attempts=10
just paste-gap-start backend=auto label=ghostty attempts=10
```

## Step 5: Optional Direct Diagnostic Control

If you need a quick non-archived spot check:

```bash
source scripts/stt-helper.sh
stt diag-injector --backend uinput --attempts 10 --shortcut ctrl-shift-v --prefix QUICK --interval-ms 150
```

Run this only from repo root.

## Step 6: Artifact Checklist Per Run

Each run directory should include:

1. `run-meta.env`
2. `operator-observations.tsv`
3. `summary.txt`
4. `injector-subprocess-report.tsv`
5. `artifacts/parakeet-ptt.log`
6. `artifacts/parakeet-daemon.log`
7. `artifacts/parakeet-ghostty-sink.txt`
8. `artifacts/diag-injector.log` (for inject-only runs)

## Step 7: Quick Triage Commands

From Terminal A:

```bash
run_dir="$(just paste-gap-current)"
cat "$run_dir/summary.txt"
rg -n "backend_warning_tags|backend_attempts|route_primary|route_class" "$run_dir/summary.txt"
```

To inspect parsed rows:

```bash
column -ts $'\t' "$run_dir/injector-subprocess-report.tsv" | less -S
```

## Failure Handling

If a run is contaminated (focus drift, accidental clicks, wrong target window):

1. do not reuse it as primary evidence,
2. keep artifacts for audit,
3. rerun the same backend with the same attempt count and phrase set.

## End-Of-Session Cleanup

From Terminal A:

```bash
just stop
```

In Ghostty sink window, stop sink capture with `Ctrl+C`.
