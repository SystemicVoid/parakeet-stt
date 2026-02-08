# System Specs and Full-Stack References (2026-02-08)

## Purpose

This document is a deep-research handoff companion to `docs/HANDOFF-clipboard-injector-2026-02-08.md`.
It captures exact runtime/system context plus authoritative external references so another AI/engineer can run targeted research without re-discovering environment facts.

## Snapshot Metadata

- Snapshot time (UTC): `2026-02-08T18:41:42+00:00`
- Hostname: `excurio`
- Repo: `/home/hugo/Documents/Engineering/parakeet-stt`
- Git branch state: `main...origin/main [ahead 16]`
- Latest relevant repo commits:
  - `f50b33d` `docs(stt): document strategy-based injector workflow`
  - `d0ff202` `fix(helper): forward advanced paste controls`
  - `c60290c` `fix(ptt): add strategy-driven clipboard injection`

## Hardware and OS

- OS: Pop!_OS 24.04 LTS (`/etc/os-release`)
- Kernel: `6.18.7-76061807-generic`
- CPU: AMD Ryzen 9 9900X (12C/24T)
- RAM: 62 GiB total
- GPU: NVIDIA GeForce RTX 5060 Ti, 16311 MiB, driver `580.119.02`
- Architecture: `x86_64`

## Desktop, Session, Display, Compositor

- Desktop: `COSMIC` (`XDG_CURRENT_DESKTOP=COSMIC`)
- Wayland display socket: `WAYLAND_DISPLAY=wayland-1`
- X display bridge: `DISPLAY=:0`
- DBus session bus: `unix:path=/run/user/1000/bus`
- Client process runtime env (from `/proc/<parakeet-ptt>/environ`):
  - `XDG_SESSION_TYPE=tty`
  - `XDG_RUNTIME_DIR=/run/user/1000`
- Relevant packages:
  - `cosmic-comp 0.1~1770304347~24.04~0e97ddb`
  - `xdg-desktop-portal 1.18.4-1ubuntu2.24.04.1`
  - `xdg-desktop-portal-cosmic 0.1.0pop1~1769825428~24.04~ff0633f`
  - `xwayland 24.1.2-1pop1~...`

## Toolchain and Runtime Versions (Observed)

- `nu 0.110.0` (`/home/hugo/.cargo/bin/nu`)
- `ghostty 1.2.3`
- `cosmic-term 1.0.5`
- `tmux 3.4`
- `wtype 0.4-3` (dpkg package version)
- `wl-clipboard 2.2.1` (`wl-copy`, `wl-paste`)
- `ydotool 0.1.8-3build1`
- `rustc 1.93.0`, `cargo 1.93.0`
- `uv 0.10.0`
- `python 3.12.3`
- `node v22.17.0`
- `pnpm 10.29.1`

### Declared vs observed drift

`AGENTS.md` declares older baselines (for example Rust 1.89.0 and uv 0.9.26), but current machine state is newer (Rust 1.93.0, uv 0.10.0, pnpm 10.29.1).
Research recommendations should prefer observed versions above.

## Stack Roles and Tool Usage in This System

- `parakeet-stt-daemon` (Python/FastAPI): local ASR server over WebSocket.
- `parakeet-ptt` (Rust/tokio): hotkey client, session control, transcript injection.
- `evdev`: hotkey source (`KEY_RIGHTCTRL`).
- `wl-copy` / `wl-paste`: clipboard ownership and readback.
- `wtype` (default key backend): sends synthetic paste chords via Wayland virtual keyboard protocol.
- `ydotool` (optional key backend): uinput-based synthetic key events via `ydotoold` daemon.
- `tmux` (`parakeet-stt` session): orchestrates long-running daemon/client processes.
- `nu` wrapper: delegates `stt` command to `bash` helper script.

## Relevant Local Code and Behavior Surfaces

- Injector implementation: `parakeet-ptt/src/injector.rs`
  - `ClipboardInjector`, strategy engine, post-chord hold, restore policy, key backends.
- CLI and injector wiring: `parakeet-ptt/src/main.rs`
  - flags: `--paste-*`, `--copy-only`, `--paste-key-backend`, `--paste-seat`.
- Config types: `parakeet-ptt/src/config.rs`
- Runtime launcher/orchestration: `scripts/stt-helper.sh`
- Nu bridge function: `~/.config/forge/nushell/.config/nushell/config.nu`
  - `def stt [...args: string] { ... bash -lc ... }`

## Nu Wrapper and Process Topology

Current nu wrapper implementation:

```nu
# ~/.config/forge/nushell/.config/nushell/config.nu
def stt [...args: string] {
  let cmd = if ($args | is-empty) { "stt start" } else { $"stt ($args | str join ' ')" }
  bash -lc $"source ~/Documents/Engineering/parakeet-stt/scripts/stt-helper.sh && ($cmd)"
}
```

Runtime chain:

1. User runs `stt ...` in nu.
2. nu builds a single shell command string and invokes `bash -lc`.
3. `scripts/stt-helper.sh` starts tmux session `parakeet-stt`.
4. tmux pane launches daemon (`uv run parakeet-stt-daemon`) and client (`parakeet-ptt`).
5. Rust client receives `final_result` and executes clipboard + paste choreography.

## Active STT Defaults in Helper (Current)

From `scripts/stt-helper.sh` defaults:

- `PARAKEET_INJECTION_MODE=type` (unless overridden)
- `PARAKEET_PASTE_SHORTCUT=ctrl-shift-v`
- `PARAKEET_PASTE_SHORTCUT_FALLBACK=none`
- `PARAKEET_PASTE_STRATEGY=always-chain`
- `PARAKEET_PASTE_CHAIN_DELAY_MS=45`
- `PARAKEET_PASTE_RESTORE_POLICY=never`
- `PARAKEET_PASTE_RESTORE_DELAY_MS=250`
- `PARAKEET_PASTE_POST_CHORD_HOLD_MS=700`
- `PARAKEET_PASTE_COPY_FOREGROUND=true`
- `PARAKEET_PASTE_MIME_TYPE=text/plain;charset=utf-8`
- `PARAKEET_PASTE_KEY_BACKEND=wtype`
- `PARAKEET_PASTE_SEAT=<empty>`
- `PARAKEET_PASTE_WRITE_PRIMARY=false`

## Runtime Evidence Summary (Latest Focused Window)

Window analyzed: `2026-02-08 18:19-18:30 UTC`

- Daemon side (`/tmp/parakeet-daemon.log`): healthy start/stop/final_result cycles.
- Client side (`/tmp/parakeet-ptt.log`):
  - `final result received` for each utterance.
  - `starting clipboard injection` for each final result.
  - Chords executed according to strategy (`CtrlShiftV`, optional `ShiftInsert`, `CtrlV`).
  - Injector finishes with `outcome="success_assumed"` for paste mode.
  - `copy-only` runs finish with `outcome="copy_only"`.
  - No WARN/ERROR in this latest window.

Current instrumentation status:

- Confirms process-level success.
- Does not confirm app-level insertion success.

## Known Behavioral Matrix (Latest User Repro)

- Ghostty terminal:
  - STT final result produced.
  - Paste chord fires.
  - Transcript often not inserted automatically.
- COSMIC Terminal:
  - Similar to Ghostty in repro report.
- Brave input fields:
  - Paste trigger can fire, but focus side effects observed (input may need re-click).
- COSMIC Text Editor:
  - Focus side effect less evident compared to web fields.
- `copy-only` mode:
  - Reliable clipboard update path; no synthetic key chord.

## Timeline and Package-Change Context (Confounders)

- `ghostty 1.2.3` installed on `2026-01-25` (apt history).
- COSMIC compositor/session packages upgraded repeatedly, including on `2026-02-07`.
- `wtype` installed on `2025-11-21`.
- `ydotool` installed on `2025-11-17`.

Implication: the observed breakage may correlate with nu migration, but co-timed terminal/compositor updates are plausible contributors.

## Deep-Research Priority Questions

1. Which Wayland input path is most reliable on COSMIC + Ghostty for synthetic paste (virtual keyboard vs uinput vs portal/libei)?
2. Does chained shortcut injection (`always-chain`) commonly cause focus churn or semantic no-op in Chromium-based web fields?
3. What are known Ghostty and COSMIC caveats specifically for synthetic `Ctrl+Shift+V`, `Shift+Insert`, and `Ctrl+V` chords?
4. Is there a robust Rust-native clipboard + input stack that avoids shelling out to `wl-copy/wtype` while preserving Wayland correctness?
5. Is portal/libei practical for non-sandboxed local agents as a stable cross-compositor input backend?

## External References (Authoritative, by Subsystem)

### Ghostty

- Ghostty discussion on broken `wtype` virtual keyboard text behavior:
  - <https://github.com/ghostty-org/ghostty/discussions/10558>
- Ghostty config reference:
  - <https://ghostty.org/docs/config/reference>
- Ghostty keybind docs:
  - <https://ghostty.org/docs/config/keybind>

### wtype and virtual keyboard protocol

- `wtype` man page:
  - <https://manpages.ubuntu.com/manpages/jammy/man1/wtype.1.html>
- `wtype` issue showing compositor support dependency for `virtual-keyboard-unstable-v1`:
  - <https://github.com/atx/wtype/issues/45>
- Wayland virtual keyboard protocol reference:
  - <https://wayland.app/protocols/virtual-keyboard-unstable-v1>
- Official protocol XML (wayland-protocols):
  - <https://gitlab.freedesktop.org/wayland/wayland-protocols/-/blob/main/unstable/virtual-keyboard/virtual-keyboard-unstable-v1.xml>

### Clipboard semantics and tooling

- `wl-clipboard` repository/docs:
  - <https://github.com/bugaevc/wl-clipboard>
- `wl-copy` man page:
  - <https://manpages.ubuntu.com/manpages/jammy/man1/wl-copy.1.html>
- Known browser caveat for one-shot clipboard ownership:
  - <https://github.com/bugaevc/wl-clipboard/issues/107>
- Wayland data-control protocol (`wlr-data-control`):
  - <https://wayland.app/protocols/wlr-data-control-unstable-v1>
- Wayland data-control protocol (`ext-data-control`):
  - <https://wayland.app/protocols/ext-data-control-v1>

### Rust alternatives for clipboard handling

- `wl-clipboard-rs` crate (Wayland clipboard operations in Rust):
  - <https://docs.rs/wl-clipboard-rs/latest/wl_clipboard_rs/>
- `arboard` crate docs (clipboard ownership caveats):
  - <https://docs.rs/arboard/latest/arboard/>

### Alternative key backend

- `ydotool` repository and daemon model (`ydotoold` + `/dev/uinput`):
  - <https://github.com/ReimuNotMoe/ydotool>

### COSMIC compositor and portal context

- COSMIC compositor project:
  - <https://github.com/pop-os/cosmic-comp>
- xdg-desktop-portal RemoteDesktop interface:
  - <https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html>
- `libei` project (input emulation channel):
  - <https://libinput.pages.freedesktop.org/libei/>

### Nushell and tmux behavior

- Nushell string interpolation:
  - <https://www.nushell.sh/book/operators.html#string-interpolation>
- Nushell `run-external` docs:
  - <https://www.nushell.sh/commands/docs/run-external.html>
- tmux environment/session model (man page):
  - <https://manpages.ubuntu.com/manpages/focal/man1/tmux.1.html>

### STT model/server stack references

- NVIDIA Parakeet model card used by daemon:
  - <https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3>
- FastAPI WebSocket docs:
  - <https://fastapi.tiangolo.com/advanced/websockets/>

## Companion Documents

- Issue handoff with attempts, failures, and hypotheses:
  - `docs/HANDOFF-clipboard-injector-2026-02-08.md`
- Repository operation notes for agents:
  - `CLAUDE.md`
