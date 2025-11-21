"""Parakeet model loader helpers.

The import is deferred so environments without `nemo_toolkit[asr]` can still
start the daemon for protocol testing.
"""

from __future__ import annotations

import tempfile
from collections.abc import Iterable, Sequence
from dataclasses import dataclass
from importlib import import_module
from pathlib import Path
from typing import TYPE_CHECKING, Any

import numpy as np
from loguru import logger

DEFAULT_MODEL_NAME = "nvidia/parakeet-tdt-0.6b-v3"

if TYPE_CHECKING:  # pragma: no cover
    import nemo.collections.asr as nemo_asr
    import torch
    from nemo.collections.asr.models import ASRModel
else:
    nemo_asr = None  # type: ignore
    ASRModel = Any  # type: ignore
    try:
        import torch  # type: ignore  # noqa: F401
    except ImportError:  # pragma: no cover - inference extra not installed
        torch = None  # type: ignore


def _resolve_device(requested: str) -> str:
    if requested == "cuda":
        if torch is None:  # pragma: no cover - inference extra not installed
            logger.warning("CUDA requested but torch is not available; falling back to CPU")
            return "cpu"
        if torch.cuda.is_available():  # type: ignore[union-attr]
            return "cuda"
        logger.warning("CUDA requested but not available; using CPU instead")
        return "cpu"
    return requested


def _extract_text(output: Any) -> str:
    if isinstance(output, str):
        return output.strip()
    text = getattr(output, "text", None)
    if text is not None:
        return str(text).strip()
    return str(output).strip()


def load_parakeet_model(model_name: str = DEFAULT_MODEL_NAME, device: str = "cuda") -> ASRModel:
    """Load the Parakeet model with a minimal amount of glue."""
    try:
        nemo_asr = import_module("nemo.collections.asr")
    except ImportError as exc:  # pragma: no cover - runtime guard
        raise RuntimeError(
            "nemo_toolkit[asr] is not installed; install with `uv sync --extra inference`"
        ) from exc

    resolved_device = _resolve_device(device)
    model: ASRModel = nemo_asr.models.ASRModel.from_pretrained(
        model_name=model_name, map_location="cpu"
    )
    try:
        model.to(resolved_device)
    except Exception as exc:  # pragma: no cover - runtime/device dependent
        if resolved_device == "cuda":
            logger.warning(
                "Failed to place Parakeet on CUDA ({}); retrying on CPU", exc.__class__.__name__
            )
            model.to("cpu")
            resolved_device = "cpu"
        else:
            raise
    logger.info("Loaded Parakeet model '{}' on device {}", model_name, resolved_device)

    # Optional attention tweak is aligned with the HF card guidance.
    change_attention = getattr(model, "change_attention_model", None)
    if callable(change_attention):
        try:
            change_attention(self_attention_model="rel_pos_local_attn", att_context_size=[256, 256])
        except Exception as exc:  # pragma: no cover - best-effort
            logger.warning("Failed to adjust attention window: {}", exc)

    return model


@dataclass
class ParakeetTranscriber:
    model: ASRModel

    def warmup(self) -> None:
        """Run a trivial forward pass to pay the first-use cost."""
        tmp_path: Path | None = None
        try:
            with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as tmp:
                tmp_path = Path(tmp.name)
            cfg = getattr(self.model, "_cfg", None)
            sample_rate = getattr(cfg, "sample_rate", 16_000)
            silence = np.zeros((sample_rate,), dtype=np.float32)
            import soundfile as sf

            sf.write(tmp_path, silence, sample_rate)
            _ = self.transcribe_wav(str(tmp_path))
        except Exception as exc:  # pragma: no cover - warmup is optional
            logger.debug("Warmup skipped: {}", exc)
        finally:
            if tmp_path:
                tmp_path.unlink(missing_ok=True)

    def transcribe_files(self, paths: Sequence[str], *, timestamps: bool = False) -> list[str]:
        if not paths:
            return []
        logger.info("Transcribing {} file(s) with Parakeet", len(paths))
        outputs = self.model.transcribe(list(paths), timestamps=timestamps)
        return [_extract_text(item) for item in outputs]

    def transcribe_iter(self, paths: Iterable[str], *, timestamps: bool = False) -> list[str]:
        return self.transcribe_files(list(paths), timestamps=timestamps)

    def transcribe_wav(self, path: str) -> str:
        outputs = self.model.transcribe([path], batch_size=1)
        if not outputs:
            return ""
        return _extract_text(outputs[0])

class ParakeetStreamingSession:
    """Accumulate audio chunks for a single streaming session."""

    def __init__(self, parent: ParakeetStreamingTranscriber, sample_rate: int) -> None:
        self._parent = parent
        self.sample_rate = sample_rate
        self._chunks: list[np.ndarray] = []

    def feed(self, chunk: np.ndarray) -> None:
        self._chunks.append(np.array(chunk, dtype=np.float32, copy=True))

    def finalize(self) -> str:
        if not self._chunks:
            return ""
        combined = np.concatenate(self._chunks)
        if self._parent.chunk_helper is not None:
            try:
                return self._parent.chunk_helper.transcribe([combined])[0]  # type: ignore[attr-defined,index]  # noqa: E501
            except Exception as exc:  # pragma: no cover - fallback path
                logger.warning("Chunk helper failed, falling back to offline: {}", exc)
        return self._parent._transcribe_offline(combined, self.sample_rate)


class ParakeetStreamingTranscriber:
    """Streaming-friendly wrapper around Parakeet with offline fallback."""

    def __init__(
        self,
        model: ASRModel,
        *,
        chunk_secs: float = 2.0,
        right_context_secs: float = 2.0,
        left_context_secs: float = 10.0,
        batch_size: int = 32,
    ) -> None:
        self.model = model
        self.chunk_secs = float(chunk_secs)
        self.right_context_secs = float(right_context_secs)
        self.left_context_secs = float(left_context_secs)
        self.batch_size = int(batch_size)
        self.chunk_helper: Any | None = None

        self.offline = ParakeetTranscriber(model)
        self._init_helper()

    def _init_helper(self) -> None:
        try:
            from nemo.collections.asr.parts.utils.streaming_utils import (  # type: ignore
                ChunkedRNNTInfer,
            )

            cfg = getattr(self.model, "cfg", getattr(self.model, "_cfg", None))
            sample_rate = getattr(cfg, "sample_rate", 16_000)
            decoder_delay_ms = getattr(cfg, "decoder_delay_in_ms", 0)
            self.chunk_helper = ChunkedRNNTInfer(
                model=self.model,
                decoder_delay_in_ms=decoder_delay_ms,
                chunk_len_in_secs=self.chunk_secs,
                chunk_batch_size=self.batch_size,
                right_context_len_in_secs=self.right_context_secs,
                left_context_len_in_secs=self.left_context_secs,
                audio_sample_rate=sample_rate,
            )
            logger.info(
                "Streaming helper initialised (chunk_secs={}, right_context_secs={}, left_context_secs={}, batch_size={}, sample_rate={})",  # noqa: E501
                self.chunk_secs,
                self.right_context_secs,
                self.left_context_secs,
                self.batch_size,
                sample_rate,
            )
        except Exception as exc:  # pragma: no cover - environment dependent
            logger.warning("Chunked streaming helper unavailable; using offline fallback: {}", exc)
            self.chunk_helper = None

    def start_session(self, sample_rate: int) -> ParakeetStreamingSession:
        if self.chunk_helper is not None:
            try:
                self.chunk_helper.reset()  # type: ignore[attr-defined]
            except Exception as exc:  # pragma: no cover
                logger.debug("Streaming helper reset failed, falling back to offline: {}", exc)
                self.chunk_helper = None
        return ParakeetStreamingSession(self, sample_rate)

    def _transcribe_offline(self, samples: np.ndarray, sample_rate: int) -> str:
        with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as tmp:
            path = Path(tmp.name)
        try:
            import soundfile as sf

            sf.write(path, samples, sample_rate)
            return self.offline.transcribe_wav(str(path))
        finally:
            path.unlink(missing_ok=True)


__all__ = [
    "load_parakeet_model",
    "ParakeetTranscriber",
    "ParakeetStreamingTranscriber",
    "ParakeetStreamingSession",
    "DEFAULT_MODEL_NAME",
]
