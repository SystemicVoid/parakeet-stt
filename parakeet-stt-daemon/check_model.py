"""Quick inference/streaming probe for Parakeet.

Generates a short sine wave, runs it through the Parakeet model, and reports
whether streaming helpers initialise successfully.
"""

from __future__ import annotations

import argparse
import tempfile
import wave
from pathlib import Path

import numpy as np

from parakeet_stt_daemon.model import (
    DEFAULT_MODEL_NAME,
    ParakeetStreamingTranscriber,
    ParakeetTranscriber,
    load_parakeet_model,
)

SAMPLE_RATE = 16_000


def generate_sine(duration_secs: float, freq_hz: float, amplitude: float) -> np.ndarray:
    t = np.linspace(0, duration_secs, int(SAMPLE_RATE * duration_secs), endpoint=False)
    return (amplitude * np.sin(2 * np.pi * freq_hz * t)).astype(np.float32)


def write_wav(path: Path, samples: np.ndarray) -> None:
    try:
        import soundfile as sf

        sf.write(path, samples, SAMPLE_RATE)
    except Exception:  # pragma: no cover - minimal fallback
        pcm = (np.clip(samples, -1.0, 1.0) * 32767).astype("<i2")
        with wave.open(str(path), "wb") as wf:
            wf.setnchannels(1)
            wf.setsampwidth(2)
            wf.setframerate(SAMPLE_RATE)
            wf.writeframes(pcm.tobytes())


def split_chunks(samples: np.ndarray, chunk_size: int) -> list[np.ndarray]:
    return [samples[idx : idx + chunk_size] for idx in range(0, samples.size, chunk_size)]


def run_streaming_probe(model, samples: np.ndarray) -> str:
    try:
        streamer = ParakeetStreamingTranscriber(
            model,
            chunk_secs=1.6,
            right_context_secs=2.4,
            left_context_secs=2.0,
            batch_size=4,
        )
    except Exception as exc:
        return f"streaming helper init failed: {exc}"

    helper_available = streamer.chunk_helper is not None
    try:
        session = streamer.start_session(SAMPLE_RATE)
        chunk_size = max(1, int(SAMPLE_RATE * streamer.chunk_secs))
        for chunk in split_chunks(samples, chunk_size):
            session.feed(chunk)
        result = session.finalize()
        mode = "streaming helper" if helper_available else "offline fallback"
        return f"{mode} succeeded (transcript='{result}')"
    except Exception as exc:  # pragma: no cover - runtime probe
        return f"streaming helper blew up during run: {exc}"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Verify Parakeet inference locally.")
    parser.add_argument(
        "--device",
        choices=["cuda", "cpu"],
        default="cuda",
        help="Target device for model inference",
    )
    parser.add_argument(
        "--model",
        default=DEFAULT_MODEL_NAME,
        help="Model name or path (defaults to the TDT 0.6B checkpoint)",
    )
    parser.add_argument(
        "--duration",
        type=float,
        default=2.0,
        help="Length of generated sine wave (seconds)",
    )
    parser.add_argument(
        "--freq",
        type=float,
        default=440.0,
        help="Sine frequency in Hz",
    )
    parser.add_argument(
        "--amplitude",
        type=float,
        default=0.2,
        help="Amplitude for generated sine wave (0.0-1.0)",
    )
    parser.add_argument(
        "--skip-streaming",
        action="store_true",
        help="Do not attempt streaming helper initialisation",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    samples = generate_sine(args.duration, args.freq, args.amplitude)
    with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as tmp:
        wav_path = Path(tmp.name)

    try:
        write_wav(wav_path, samples)
        model = load_parakeet_model(args.model, device=args.device)
        transcriber = ParakeetTranscriber(model)

        offline_text = transcriber.transcribe_wav(str(wav_path))
        print(f"Offline transcription: '{offline_text}'")
        if args.skip_streaming:
            print("Streaming probe skipped by flag")
            return

        streaming_status = run_streaming_probe(model, samples)
        print(streaming_status)
    finally:
        wav_path.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
