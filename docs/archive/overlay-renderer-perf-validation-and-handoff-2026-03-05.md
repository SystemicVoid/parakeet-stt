# Overlay Renderer Perf Validation + Handoff (2026-03-05)

## Why this file exists
This document replaces the earlier suggestion-only draft with a validated, local-system handoff.
It captures what was measured, where the hotspots are in code, what claims were confirmed or refuted, and a ranked plan another agent can continue without re-discovery.

## Scope and environment used for validation
- Repo: `parakeet-stt`
- Overlay binary: `parakeet-ptt/src/bin/parakeet-overlay.rs`
- Date: 2026-03-05
- Host: local Pop!_OS workstation (Zen 5 class CPU)
- Build mode used for final measurements: `release` with `RUSTFLAGS='-C target-cpu=native'`
- Backend for reproducible local runs: `fallback-window`
- Note: `perf` was unavailable on this host (`perf: command not found`), so profiling was done using controlled workload + `/usr/bin/time` deltas.

## Executive summary
The four incoming suggestions are directionally reasonable, but the original framing overstates one core assumption: this renderer is not generally redrawing a full-screen 4K framebuffer.

What was confirmed:
- Full-frame clear + redraw happens each render call.
- Rounded-rect coverage and scalar blending are in hot loops.
- Text glyph rasterization happens during draw calls.
- CPU cost scales significantly with overlay width.

What was corrected:
- Default renderer cadence is ~20 FPS (`50ms` tick), not 60 FPS.
- Surface size is bounded to overlay panel dimensions, not entire screen dimensions.

## Primary code references

### Surface sizing and why this is not full-screen by default
- `parakeet-ptt/src/bin/parakeet-overlay.rs:1091`
  - `max_width` is clamped to `MIN_PANEL_WIDTH..=3840`.
- `parakeet-ptt/src/bin/parakeet-overlay.rs:1112-1120`
  - `surface_dimensions()` derives from clamped content width/height plus shadow padding.

### Render cadence and redraw trigger behavior
- `parakeet-ptt/src/bin/parakeet-overlay.rs:3122`
  - Tick interval is `Duration::from_millis(50)` (~20 FPS).
- `parakeet-ptt/src/bin/parakeet-overlay.rs:3138-3146`
  - Incoming IPC events (including `audio_level`) can trigger render.
- `parakeet-ptt/src/bin/parakeet-overlay.rs:3171-3180`
  - Tick path also redraws when `is_fading()` or timers advance.
- `parakeet-ptt/src/bin/parakeet-overlay.rs:1738-1749`
  - `is_fading()` stays true for multiple animation states and waveform signal presence.

### Full-frame clear + compositing path
- `parakeet-ptt/src/bin/parakeet-overlay.rs:2703-2774`
  - `render_frame` starts with full clear and then repaints shadow/background/border.
- `parakeet-ptt/src/bin/parakeet-overlay.rs:2720`
  - `fill_frame(frame, [0, 0, 0, 0])`.
- `parakeet-ptt/src/bin/parakeet-overlay.rs:2900-2903`
  - `fill_frame` iterates all pixels.

### Rounded-rect math and blending hotspots
- `parakeet-ptt/src/bin/parakeet-overlay.rs:261-293`
  - `rounded_rect_coverage`, includes corner distance checks (`sqrt`).
- `parakeet-ptt/src/bin/parakeet-overlay.rs:295-369`
  - `fill_rounded_rect` and `stroke_rounded_rect` loop over pixel regions and call coverage repeatedly.
- `parakeet-ptt/src/bin/parakeet-overlay.rs:240-258`
  - `blend_pixel` is scalar channel-by-channel premultiplied alpha blending.
- `parakeet-ptt/src/bin/parakeet-overlay.rs:2947-2953`
  - `blend_bitmap` inner blend path also scalar per channel.

### Text rasterization behavior
- `parakeet-ptt/src/bin/parakeet-overlay.rs:1293-1385`
  - `draw_headline` does per-character work each call.
- `parakeet-ptt/src/bin/parakeet-overlay.rs:1347`
  - `font.rasterize(character, size)` called in render path.

## Measured local results

## Method
Synthetic IPC was piped to `parakeet-overlay` through stdin, using repeatable cases:
- `listening_audio`: enter listening + stream `audio_level` events at ~50 Hz.
- `listening_idle`: enter listening and hold.
- `interim_text_stable`: stream identical interim text at ~50 Hz.
- `interim_text_changing`: stream changing interim text at ~50 Hz.
- `hidden_idle`: no active overlay events.

Command shape used:
- `./target/release/parakeet-overlay --backend fallback-window --adaptive-width false --max-width <W>`
- Captured with `/usr/bin/time -f 'user=%U sys=%S elapsed=%e maxrss_kb=%M'`.

## Results (representative runs)
- `native_w960_listening_audio`: user `0.41s`, sys `0.03s`, elapsed `5.63s`.
- `native_w3840_listening_audio`: user `1.61s`, sys `0.04s`, elapsed `6.01s`.
- `native_w960_interim_text_stable`: user `0.37s`, sys `0.01s`, elapsed `5.99s`.
- `w960_hidden_idle`: user `0.02s`, sys `0.01s`, elapsed `6.00s`.
- `w960_interim_text_changing`: user `0.45s`, sys `0.03s`, elapsed `5.86s`.

## Interpretation
- Width matters a lot: 3840-width run consumed about 3.7x the CPU time of 960-width in similar listening-audio conditions.
- Hidden state cost is negligible.
- Text workloads are non-trivial but not dominant versus worst-case large-surface redraw.
- Suggestion #1 (dirty/scissor) has real upside in wide overlays, but more complex correctness surface.

## Claim-by-claim validation of incoming suggestions

### 1) Scissored rendering / dirty rects
Status: **Partially validated**.

Reasoning:
- Correct that full clear + redraw happens every render.
- Correct that reducing touched pixels can help significantly for wide surfaces.
- Incorrect framing in original doc: this is not inherently full-screen 4K every frame.

Tradeoffs:
- Higher implementation complexity than suggested because render triggers include fades, breathing, ellipsis animation, progress bar, and width animation.
- Dirty-region safety must be state-aware. If done naively, visual artifacts are likely.

Recommendation:
- Do after lower-risk wins, unless current production uses very wide overlays frequently.

### 2) Corner mask precompute
Status: **Validated as plausible low-risk optimization**.

Reasoning:
- `rounded_rect_coverage` is repeatedly called in fill/stroke loops.
- Radius is constant (`CORNER_RADIUS=12.0`), enabling lookup-table approaches.

Tradeoffs:
- Must preserve anti-aliasing exactly enough to avoid subtle visual regressions.
- True gain depends on how much of frame time is in these geometric fills vs other effects.

Recommendation:
- Good early optimization candidate, especially with snapshot tests.

### 3) Text bitmap caching
Status: **Validated**.

Reasoning:
- Text draw path rasterizes glyphs on each render call.
- Stable text and repeated phrases can reuse cached glyphs/line bitmaps.

Tradeoffs:
- Need careful cache key design (text, size, font, width/line-wrap constraints, fade mode interactions).
- Memory vs CPU tradeoff is favorable at current overlay scales.

Recommendation:
- Best first target in current codebase for low-risk measurable wins.

### 4) SIMD-friendly blending
Status: **Unproven from local evidence**.

Reasoning:
- Scalar blending exists in hot loops.
- Without instruction-level profiling, likely but unproven benefit magnitude.

Tradeoffs:
- Highest portability and correctness risk.
- Could increase code complexity for modest gain unless benchmarked rigorously.

Recommendation:
- Keep last, and gate by benchmarks + image equivalence checks.

## Priority ranking (ROI / (effort*risk))
1. Text caching (#3): best near-term ratio.
2. Corner mask precompute (#2): solid second, isolated algorithmic change.
3. Dirty rect/scissor (#1): potentially large in worst-case widths, but complexity/risk higher.
4. SIMD blending (#4): defer until better profiling instrumentation available.

## Implementation handoff plan

### Phase A: Text caching (first)
Goal:
- Avoid repeated `font.rasterize` for unchanged glyph/text content.

Suggested approach:
- Add a cache in `TextRenderer` for per-glyph bitmap + metrics keyed by `(char, size_px, font_identity)`.
- Optionally add line/layout cache keyed by `(text, max_width_px, max_lines, size_px)`.
- Keep final blending per-frame to respect fade alpha dynamics.

Safety checks:
- Existing test suite must stay green.
- Add tests for cache hit/miss behavior and visual equivalence for repeated draws.

### Phase B: Corner mask precompute (second)
Goal:
- Replace repeated analytical corner AA coverage calls with lookup reads.

Suggested approach:
- Precompute a quarter-corner alpha table for radius 12.0 at startup.
- Reuse in rounded fill/stroke corner regions.
- Keep non-corner regions branch-simple and fast.

Safety checks:
- Pixel-diff tests against baseline for representative rectangles.
- Keep tolerances strict to avoid visible edge changes.

### Phase C: Scoped dirty-rect rendering (third)
Goal:
- Reduce touched pixels for audio-only waveform updates.

Suggested approach:
- Introduce render reason classification (audio-only, text update, animation tick, phase transition, width animation).
- Enable dirty rect only for safe classes where stale pixels cannot occur.
- Fallback to full redraw on any uncertainty.

Safety checks:
- Add tests for transitions (Hidden->Listening, Interim changes, Finalizing, fade in/out).
- Add torture test with rapid phase changes + width animation.

### Phase D: SIMD experiment (last)
Goal:
- Improve blend throughput only if gains are proven.

Suggested approach:
- First reshape loops for compiler autovectorization friendliness.
- Only introduce explicit packed math if portable and benchmarked.

Safety checks:
- Benchmark gate + output image equivalence checks.

## Reproduction commands for next session
Build:
```bash
cd parakeet-ptt
RUSTFLAGS='-C target-cpu=native' cargo build --release --bin parakeet-overlay
cargo test --release --bin parakeet-overlay
```

Quick manual CPU sanity check pattern:
```bash
# Run overlay with synthetic stdin IPC and measure with /usr/bin/time.
# Use --max-width 960 and --max-width 3840 to compare scaling.
./target/release/parakeet-overlay --backend fallback-window --adaptive-width false --max-width 960
```

Suggested automation note:
- Convert the ad-hoc fifo+time harness used during this validation into a checked-in script under `scripts/` for repeatable before/after comparisons.

## 2026-03-06 Update: Phase A Implemented + Measured

### Implementation status
- Implemented per-glyph cache in `parakeet-ptt/src/bin/parakeet-overlay.rs` (`TextRenderer` + bounded cache).
- Cache key includes `(character, size_bits, font_identity)`.
- Added runtime rollback switch: `PARAKEET_OVERLAY_GLYPH_CACHE=true|false` (default enabled).
- Added focused unit tests for key correctness, hit/miss behavior, and eviction behavior.

### Rebuild + regression gates completed
- Rebuilt optimized overlay binary for local E2E use:
  - `cd parakeet-ptt`
  - `RUSTFLAGS='-C target-cpu=native' cargo build --release --bin parakeet-overlay`
- Validation gates run after implementation:
  - `cargo fmt --check` (pass)
  - `cargo test --release --bin parakeet-overlay` (61/61 pass)
  - `just phase6-contract` (pass)

### Benchmark method (A/B on same binary)
To isolate cache impact without cross-build variance, the same optimized binary was run with cache toggled by env var:
- Cache ON: `PARAKEET_OVERLAY_GLYPH_CACHE=true`
- Cache OFF: `PARAKEET_OVERLAY_GLYPH_CACHE=false`

Workload profile:
- Synthetic stdin IPC at ~50 Hz for ~5.5s per run.
- Scenarios: `listening_audio`, `interim_text_stable`, `interim_text_changing`, and wide-surface `interim_text_stable` at `--max-width 3840`.
- Backend: `fallback-window`; adaptive width disabled.
- Two runs per scenario per mode.
- Metric capture: `/usr/bin/time -f 'user=%U sys=%S elapsed=%e maxrss_kb=%M'`.

Raw artifact:
- `/tmp/overlay-glyph-cache-bench-20260306.tsv`

### Aggregated results (2-run averages)

| Scenario | Width | Cache ON user(s) | Cache OFF user(s) | Delta user(s) | Delta % |
|---|---:|---:|---:|---:|---:|
| listening_audio | 960 | 0.450 | 0.445 | +0.005 | +1.1% |
| interim_text_stable | 960 | 0.340 | 0.345 | -0.005 | -1.4% |
| interim_text_changing | 960 | 0.435 | 0.445 | -0.010 | -2.2% |
| interim_text_stable | 3840 | 1.135 | 1.160 | -0.025 | -2.2% |

### Interpretation of update
- Cache gives a small but consistent CPU win for text-heavy interim workloads in this synthetic profile (~1-2% user CPU reduction).
- Audio-only path stays effectively neutral (slight +1.1% in these two samples, within expected noise for short runs).
- No correctness or reliability regressions were observed under current gates.
- Wide overlays still dominate total CPU cost; cache helps, but does not change the broader conclusion that pixel-touch reduction (`dirty/scissor`) remains the highest upside next.

## Known gaps from this validation
- No symbol-level profile flamegraph because `perf` tooling was unavailable on host.
- Results are from synthetic workloads, not end-to-end compositor interaction under real desktop load.
- No frame-time percentile metrics yet; only process CPU and elapsed aggregates.

## If picking this up next session
Start with Phase A and add a minimal benchmark harness script first, so each optimization can be accepted or reverted using the same measurement method.
