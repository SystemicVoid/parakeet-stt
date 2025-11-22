Short version: yes, you should absolutely “fork the idea” of whisper-overlay, but switch the engine to Parakeet + NeMo and keep all the UX bits in Rust.

I’ll lay this out as if I’m giving your coding agent a spec + task list.

---

## 1. High-level architecture

Target UX:

* Push-to-talk hotkey
* Live transcription overlay on Wayland (COSMIC)
* On release / pause → final, more accurate transcription
* Text is injected into **whatever input field is focused**, just like nerd-dictation / whisper-overlay.

Given Parakeet is a NeMo model (not a Whisper-style ONNX/ggml thing) and is officially meant to run via NeMo 2.4 + PyTorch + CUDA([Hugging Face][1]), the cleanest architecture is:

**Components**

1. **`parakeet-stt-daemon` (Python, NeMo, GPU)**

   * Long-lived process.
   * Owns the microphone, VAD, buffering, and Parakeet model.
   * Exposes a very small API over WebSocket or gRPC (localhost only):

     * `start_session` (push-to-talk started)
     * `audio_stream` (optional, if we stream audio from client; or daemon records itself)
     * `stop_session` (push-to-talk ended)
     * Emits:

       * `partial` transcriptions (for live overlay)
       * `final` transcription (for typing into window).

2. **`parakeet-overlay` (Rust, Wayland)**

   * Wayland overlay window (layer-shell + virtual-keyboard-v1) similar to whisper-overlay([GitHub][2]).
   * Listens for a global hotkey via `evdev`.
   * On key down → tells daemon to start.
   * Shows streaming text from daemon in a floating overlay.
   * On key up → stops session, waits for `final` text, then injects it as keystrokes into currently focused window.

3. **`parakeet-cli` (Rust or Python)**

   * Simple CLI to transcribe **audio files** using the same daemon.
   * `parakeet-cli transcribe path/to/file.wav` → prints text; optional `--json`, `--timestamps`.

This keeps Python + NeMo isolated to one process, while all OS integration and UX is in Rust, like you want.

---

## 2. Tech stack selection

### 2.1 STT engine

* **Model**: `nvidia/parakeet-tdt-0.6b-v3` on HuggingFace.([Hugging Face][1])
* **Runtime**: NVIDIA NeMo 2.4 + PyTorch CUDA.

  * Install via:

    ```bash
    pip install -U "nemo_toolkit[asr]"
    ```

    and a CUDA-enabled PyTorch (cu121 on Pop!_OS 24.04 with modern drivers).([GitHub][3])
* **Streaming**:

  * For v1, we can do “pseudo-streaming” by chunking mic audio and calling `asr_model.transcribe()` on a sliding window.
  * NeMo *does* provide a streaming RNNT script for Parakeet([Hugging Face][1]), but integrating it programmatically is more work; we can get a very usable system with chunked inference first, then refactor to true streaming later.

### 2.2 Daemon implementation

Language: **Python 3.11**
Main libs:

* `nemo_toolkit[asr]` – loads Parakeet & performs transcription([Hugging Face][1])
* `sounddevice` or `pyaudio` – microphone capture
* `webrtcvad` or `silero-vad` – VAD; RealtimeSTT uses WebRTC + Silero and that combination is battle-tested([GitHub][3])
* `fastapi` + `uvicorn` **or** `websockets` – for a small WebSocket API

### 2.3 Overlay client (Rust)

Language: **Rust 1.89+**

Crates (rough cut):

* Wayland & overlay:

  * `smithay-client-toolkit` – Wayland client & layer-shell
  * `wayland-protocols` – `wlr-layer-shell`, `virtual-keyboard-v1` if needed
* UI:

  * `gtk4` or `relm4` (whisper-overlay uses GTK4 for overlay)([GitHub][2])
* Hotkey & input:

  * `evdev` – global key press detection (like whisper-overlay)([GitHub][2])
  * `virtual-keyboard` protocol or `uinput` crate – to inject keystrokes
* Networking:

  * `tokio` + `tokio-tungstenite` (WebSocket)
  * `serde`, `serde_json` – message encoding
* CLI:

  * `clap` – subcommands: `overlay`, `waybar-status`, `health`, etc.

### 2.4 File transcription CLI

Option A (simpler): Python‐only:

* Provide a `parakeet-stt` Python CLI command:

  ```bash
  parakeet-stt transcribe foo.wav
  ```

Option B (nicer UX): Rust client hitting the daemon:

* `parakeet-cli transcribe foo.wav`
* Sends `TranscribeFile` request (file path) to daemon; daemon runs model on GPU and returns text.

I’d pick **Option B** so everything routes through one model instance.

---

## 3. API between overlay & daemon

Keep it tiny and explicit. This is what I’d have your coding agent implement.

### 3.1 Transport

* WebSocket on `ws://127.0.0.1:7007/ws`
* JSON messages, UTF-8.

### 3.2 Message types

**Client → Server**

* `StartSession`

  ```json
  { "type": "start_session", "session_id": "uuid-1", "mode": "mic", "language": null }
  ```

  * `mode`: `"mic"` or `"file"`
  * `language`: optional BCP-47 code (e.g. `"es"`, `"en"`) – otherwise Parakeet auto-detects.([Hugging Face][1])

* `StopSession`

  ```json
  { "type": "stop_session", "session_id": "uuid-1" }
  ```

* `TranscribeFile`

  ```json
  {
    "type": "transcribe_file",
    "session_id": "uuid-2",
    "path": "/path/to/audio/foo.wav"
  }
  ```

*(Variant if you want to support uploaded raw audio later: `audio_b64` field.)*

**Server → Client**

* `SessionStarted`

  ```json
  { "type": "session_started", "session_id": "uuid-1" }
  ```

* `Partial`

  ```json
  {
    "type": "partial",
    "session_id": "uuid-1",
    "text": "this is a partial",
    "is_final": false
  }
  ```

* `Final`

  ```json
  {
    "type": "final",
    "session_id": "uuid-1",
    "text": "This is the final sentence.",
    "is_final": true
  }
  ```

* `FileResult`

  ```json
  {
    "type": "file_result",
    "session_id": "uuid-2",
    "text": "Transcript of file…",
    "timestamps": null
  }
  ```

* `Error`

  ```json
  {
    "type": "error",
    "session_id": "uuid-1",
    "message": "GPU out of memory"
  }
  ```

Semantics:

* Overlay sends `start_session` on hotkey down → overlay starts showing partials.
* When user stops talking or hotkey up → overlay sends `stop_session`, daemon emits `final`.
* Overlay takes `final.text`, injects it as keyboard events, then hides overlay.

---

## 4. Implementation details per component

### 4.1 `parakeet-stt-daemon` – tasks for your coding agent

**Repo structure**

```text
parakeet-stt-daemon/
  pyproject.toml
  src/
    parakeet_daemon/
      __init__.py
      config.py
      model.py
      audio.py
      vad.py
      server.py
      main.py
```

#### Step 1 – Environment & CUDA

1. Install Python toolchain (you’re already on `uv` – perfect):

   ```bash
   uv init parakeet-stt-daemon
   cd parakeet-stt-daemon
   ```

2. Add dependencies to `pyproject.toml`:

   * `torch` (CUDA build for your driver, e.g. cu121)
   * `torchaudio`
   * `nemo_toolkit[asr]`
   * `sounddevice` or `pyaudio`
   * `webrtcvad` or `silero-vad`
   * `fastapi`
   * `uvicorn[standard]`
   * `websockets` (if you don’t want FastAPI websockets)
   * `pydantic`

3. Make sure CUDA is working:

   ```python
   import torch
   print(torch.cuda.is_available(), torch.cuda.get_device_name(0))
   ```

#### Step 2 – Load Parakeet model once

`model.py`:

* Use NeMo’s ASRModel loader:([Hugging Face][1])

  ```python
  import nemo.collections.asr as nemo_asr

  class ParakeetModel:
      def __init__(self, device="cuda"):
          self.asr_model = nemo_asr.models.ASRModel.from_pretrained(
              model_name="nvidia/parakeet-tdt-0.6b-v3"
          )
          self.asr_model.to(device)
          self.device = device

      def transcribe_wav(self, wav_path: str, timestamps: bool = False):
          out = self.asr_model.transcribe([wav_path], timestamps=timestamps)
          return out[0]
  ```

* Optional: set attention model for longer context if you want:([Hugging Face][1])

  ```python
  self.asr_model.change_attention_model(
      self_attention_model="rel_pos_local_attn",
      att_context_size=[256, 256],
  )
  ```

#### Step 3 – Microphone + VAD

`audio.py`:

* Use `sounddevice` to record from default input at 16 kHz mono.
* Implement a background thread or async generator that yields PCM frames (e.g. 20 ms or 30 ms frames).

`vad.py`:

* Wrap `webrtcvad`:

  * Use 16 kHz, 20 ms frames.
  * Maintain a ring buffer to smooth decisions.
  * Expose:

    * `on_voice_start` callback
    * `on_voice_end` callback
* Design so the daemon knows:

  * “User started speaking” → start accumulating frames for partial transcription
  * “User stopped for N ms” → finalize segment.

For push-to-talk mode, you can simplify: as long as the key is held, you treat everything as “active”, but VAD still helps to know when to run final re-decode.

#### Step 4 – Chunked “streaming”

Simplest version for your agent:

* When a session starts:

  * Begin writing mic PCM into a temporary `.wav` file in `/tmp/parakeet-session-<uuid>.wav`.
  * Every `chunk_interval_ms` (e.g. 700–1000 ms), schedule a background task:

    * Copy **last N seconds** of audio to a small temp file (e.g. 6–8 seconds sliding window).
    * Run `parakeet_model.transcribe_wav(temp)` → get text.
    * Emit `Partial` with the **full** text; overlay replaces its display.

* When VAD says “stopped” OR overlay sends `stop_session`:

  * Stop writing audio.
  * Run one final `transcribe_wav` on the entire captured `.wav` file.
  * Emit `Final` and delete temp files.

On your GPU (5060 Ti 16GB), a few seconds of audio with a 0.6B model should be comfortable in <200–400 ms per call, especially at 16 kHz.

#### Step 5 – WebSocket server

`server.py`:

* Use `fastapi` with `WebSocket` or plain `websockets` lib.
* Maintain:

  * `sessions: Dict[str, SessionState]`
  * `SessionState` includes:

    * `wav_writer`
    * `partial_task_handle`
    * `is_running`
* Handle messages from 3.2 API.

Pseudo-flow:

```python
async def handle_ws(ws):
    model = global_model
    while True:
        msg = await ws.receive_json()
        if msg["type"] == "start_session":
            session = create_session(msg["session_id"])
            start_mic_recording(session)
            await ws.send_json({"type": "session_started", ...})

        elif msg["type"] == "stop_session":
            session = sessions[msg["session_id"]]
            stop_mic_recording(session)
            text = model.transcribe_wav(session.full_wav_path)
            await ws.send_json({"type": "final", "text": text, ...})

        elif msg["type"] == "transcribe_file":
            text = model.transcribe_wav(msg["path"])
            await ws.send_json({"type": "file_result", "text": text, ...})
```

`main.py`:

* CLI entrypoint:

  ```bash
  uv run parakeet_daemon.main --port 7007
  ```
* Adds health endpoint or just logs “ready” when model loaded.

---

### 4.2 `parakeet-overlay` – tasks for your coding agent

Because GitHub’s UI isn’t giving us the Rust source of whisper-overlay, I’d treat it as conceptual inspiration and rebuild the pieces.

**Repo layout**

```text
parakeet-overlay/
  Cargo.toml
  src/
    main.rs
    overlay.rs
    hotkey.rs
    client.rs
    input.rs
    waybar.rs
```

#### Step 1 – CLI interface

Use `clap`:

* Subcommands:

  * `overlay` – run overlay + hotkey
  * `waybar-status` – print JSON status (disconnected, connected, active)
  * `health` – ping daemon

Example:

```bash
parakeet-overlay overlay --hotkey KEY_RIGHTCTRL --address 127.0.0.1:7007
```

#### Step 2 – Hotkey via `evdev`

* Enumerate input devices, pick those with keyboard capabilities.
* Use `evdev` to listen for `KEY_RIGHTCTRL` (or whichever is configured).
* When key is pressed:

  * If not already in session → send `start_session` to daemon.
  * Mark state as `Recording`.
* When key released:

  * Send `stop_session`.

Same pattern as whisper-overlay’s global hotkey approach.([GitHub][2])

#### Step 3 – WebSocket client

`client.rs`:

* Use `tokio-tungstenite` to connect to `ws://127.0.0.1:7007/ws`.
* Spawn a task that:

  * Sends `StartSession` / `StopSession` JSON messages.
  * Receives `partial` / `final` messages and forwards them to UI layer via a channel (`tokio::sync::mpsc`).

State machine:

* `Idle` → `Connecting` → `Ready`
* `Ready + key_down` → `Recording`
* On `partial` → update overlay text
* On `final` → call `input::type_text(final.text)` and return to `Idle` (overlay hidden).

#### Step 4 – Wayland overlay UI

You want something like whisper-overlay:

* Always-on-top overlay anchored at top or center, dark/translucent background, big white text.

Use:

* `gtk4` for UI (text label in a window with transparent background).
* `smithay-client-toolkit` or `gtk-layer-shell` (Rust bindings) to mark window as a layer-shell surface (e.g. `top-layer`) so it appears above all apps.

Essential behaviors:

* Initial state: hidden.
* When session starts:

  * Show overlay window.
  * Set label text to empty or “Listening…”
* On each partial:

  * Replace label text with `msg.text`.
* On final:

  * Hide overlay window.

You can keep styling minimal – your agent can copy the CSS approach from whisper-overlay’s README (it mentions external stylesheet support).([GitHub][2])

#### Step 5 – Virtual keyboard / input injection

For Wayland, whisper-overlay uses `virtual-keyboard-v1`.([GitHub][2])
Your agent should:

* Negotiate `virtual-keyboard` with the compositor via `smithay-client-toolkit` (or use any crate that already wraps this protocol).
* Implement `type_text(text: &str)`:

  * For each Unicode scalar:

    * Map to key events (rough version: use `uinput` + keymap; or send Unicode if compositor supports it).
  * Send key down / key up sequences with small delay.

If COSMIC’s compositor doesn’t expose virtual-keyboard yet, fallback options:

* `ydotool` / `wtype` call as subprocess. Crude, but gets you unblocked until cosmic catches up.

For a first version, I’d **just shell out to `wtype`**:

```rust
Command::new("wtype")
    .arg(&text)
    .spawn()
    .await?;
```

Then later replace that with proper virtual-keyboard once you confirm protocol support.

#### Step 6 – Waybar integration (optional nice-to-have)

Replicate whisper-overlay’s tiny status JSON:([GitHub][2])

* `parakeet-overlay waybar-status` prints:

```json
{ "text": "", "alt": "connected-active", "tooltip": "Parakeet STT active" }
```

depending on state:

* `disconnected`
* `connected`
* `connected-active` (while recording)

---

## 5. File transcription UX

Add a small Rust binary `parakeet-cli` (or a subcommand of `parakeet-overlay`) that uses the same WebSocket API:

```bash
parakeet-overlay transcribe --file foo.wav
# prints transcript to stdout
```

Flow:

* Connect to daemon.
* Generate new `session_id`.
* Send `transcribe_file` with absolute path.
* Wait for `file_result`.
* Print `text`.

You get audio-file transcription “for free” courtesy of the same Parakeet instance.

---

## 6. Should you fork whisper-overlay?

My take:

* **Conceptually**: yes, it’s the right design:

  * Wayland overlay + hotkey + server-client split, status in waybar, Wayland-only focus.([GitHub][2])
* **Practically**: with current GitHub UI limitations and the fact that whisper-overlay is tightly coupled to RealtimeSTT + Whisper([GitHub][2]), it’s cleaner to:

  * Use it as a **reference**, not a hard fork.
  * Re-implement client logic with your own, simpler protocol.

If you really want a direct fork:

* You’d:

  * Keep the Rust crate (client).
  * Replace `realtime-stt-server.py` with a new `parakeet-stt-server.py` that **implements the same line-protocol / WebSocket messages** RealtimeSTT expects (you’d need to inspect that repo locally).
  * Swap out RealtimeSTT’s Whisper backend with NeMo + Parakeet.
* That’s doable but slightly more annoying than rolling your own small protocol, and gives you features (e.g. wake words, multi-client handling) you probably don’t need for personal use.

Given your goal (“fast, simple, production-grade, Rust-heavy”), I’d:

> **Build `parakeet-stt-daemon` + `parakeet-overlay` as new repos with a clean JSON/WebSocket protocol, inspired by whisper-overlay, not shackled to it.**

---

## 7. Nice future add-ons (for “v2”)

Once v1 is stable:

1. **Post-processing with a local LLM via LM Studio / Ollama**

   * Send raw Parakeet output to a local Qwen / Llama for punctuation/style cleanup.
   * You already have LM Studio exposure from the blog post; its local HTTP API is perfect for this.([florianbrand.com][4])

2. **Language-aware macros**

   * “If current window is VSCode, wrap final text in `\"` and `\";`” etc.
   * Later integrate with your broader agent toolchain.

3. **True RNNT streaming**

   * Replace chunked `transcribe()` calls with direct usage of NeMo’s streaming infer script internals.([Hugging Face][1])

---

If you want, next step I can do is:

* Draft concrete `pyproject.toml` and `Cargo.toml`.
* Provide minimal skeleton code for the daemon & overlay (enough for your coding agent to fill in).

[1]: https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3 "nvidia/parakeet-tdt-0.6b-v3 · Hugging Face"
[2]: https://github.com/oddlama/whisper-overlay "GitHub - oddlama/whisper-overlay: A wayland overlay providing speech-to-text functionality for any application via a global push-to-talk hotkey"
[3]: https://github.com/KoljaB/RealtimeSTT "GitHub - KoljaB/RealtimeSTT: A robust, efficient, low-latency speech-to-text library with advanced voice activity detection, wake word activation and instant transcription."
[4]: https://florianbrand.com/posts/local-llms "Florian Brand"
