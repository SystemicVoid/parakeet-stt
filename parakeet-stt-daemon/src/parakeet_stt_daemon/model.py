"""Parakeet model loader helpers.

The import is deferred so environments without `nemo_toolkit[asr]` can still
start the daemon for protocol testing.
"""

from __future__ import annotations

import tempfile
import wave
from collections.abc import Iterable, Sequence
from dataclasses import dataclass
from importlib import import_module
from pathlib import Path
from typing import TYPE_CHECKING, Any, cast

import numpy as np
from loguru import logger

DEFAULT_MODEL_NAME = "nvidia/parakeet-tdt-0.6b-v3"

if TYPE_CHECKING:  # pragma: no cover
    import nemo.collections.asr as nemo_asr
    import torch
    from nemo.collections.asr.models import ASRModel
else:
    nemo_asr = None
    ASRModel = Any
    try:
        import torch  # noqa: F401
    except ImportError:  # pragma: no cover - inference extra not installed
        torch = None


def _resolve_device(requested: str) -> str:
    if requested == "cuda":
        if torch is None:  # pragma: no cover - inference extra not installed
            logger.warning("CUDA requested but torch is not available; falling back to CPU")
            return "cpu"
        if torch.cuda.is_available():
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


def _is_tdt_model(model: ASRModel) -> bool:
    loss = getattr(model, "loss", None)
    loss_impl = getattr(loss, "_loss", None)
    for candidate in (loss_impl, loss):
        if candidate is None:
            continue
        name = type(candidate).__name__.lower()
        if "tdt" in name:
            return True
    cfg = getattr(model, "_cfg", None) or getattr(model, "cfg", None)
    decoder = getattr(cfg, "decoder", None)
    target = getattr(decoder, "_target_", None)
    if target and "tdt" in str(target).lower():
        return True
    return False


def _get_cfg_value(cfg: Any, key: str) -> Any:
    if cfg is None:
        return None
    if isinstance(cfg, dict):
        return cfg.get(key)
    try:
        return cfg[key]
    except (AttributeError, IndexError, KeyError, TypeError):
        return getattr(cfg, key, None)


def _set_cfg_value(cfg: Any, key: str, value: Any) -> bool:
    if cfg is None:
        return False
    try:
        if isinstance(cfg, dict):
            cfg[key] = value
            return True
        if hasattr(cfg, "__setitem__"):
            cfg[key] = value
            return True
        if hasattr(cfg, key):
            setattr(cfg, key, value)
            return True
    except (AttributeError, IndexError, KeyError, TypeError, ValueError):
        return False
    return False


def _write_audio_file(path: Path, samples: np.ndarray, sample_rate: int) -> None:
    sf: Any | None
    try:
        import soundfile as sf_mod
    except ImportError:
        sf = None  # pragma: no cover - fallback for minimal environments
    else:
        sf = sf_mod

    if sf is not None:
        try:
            sf.write(path, samples, sample_rate)
            return
        except (OSError, RuntimeError, TypeError, ValueError):
            pass  # pragma: no cover - fallback for minimal environments

    # Minimal fallback path for environments without soundfile or where writes fail.
    pcm = (np.clip(samples, -1.0, 1.0) * 32767).astype("<i2")
    with wave.open(str(path), "wb") as wf:
        wf.setnchannels(1)
        wf.setsampwidth(2)
        wf.setframerate(sample_rate)
        wf.writeframes(pcm.tobytes())


def load_parakeet_model(model_name: str = DEFAULT_MODEL_NAME, device: str = "cuda") -> ASRModel:
    """Load the Parakeet model with a minimal amount of glue."""
    try:
        nemo_asr = import_module("nemo.collections.asr")
    except ImportError as exc:  # pragma: no cover - runtime guard
        raise RuntimeError(
            "nemo_toolkit[asr] is not installed; install with `uv sync --extra inference`"
        ) from exc

    resolved_device = _resolve_device(device)
    map_location = torch.device("cpu") if torch is not None else None
    model: ASRModel = nemo_asr.models.ASRModel.from_pretrained(
        model_name=model_name, map_location=map_location
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
        except (AttributeError, RuntimeError, TypeError, ValueError) as exc:  # pragma: no cover
            logger.warning("Failed to adjust attention window: {}", exc)

    object.__setattr__(model, "_parakeet_effective_device", resolved_device)
    return model


@dataclass
class ParakeetTranscriber:
    model: ASRModel

    def warmup(self) -> None:
        """Run a trivial forward pass to pay the first-use cost."""
        try:
            cfg = getattr(self.model, "_cfg", None)
            sample_rate = getattr(cfg, "sample_rate", 16_000)
            silence = np.zeros((sample_rate,), dtype=np.float32)
            _ = self.transcribe_samples(silence, sample_rate=sample_rate)
        except (AttributeError, RuntimeError, TypeError, ValueError) as exc:  # pragma: no cover
            logger.debug("Warmup skipped: {}", exc)

    def transcribe_files(self, paths: Sequence[str], *, timestamps: bool = False) -> list[str]:
        if not paths:
            return []
        logger.info("Transcribing {} file(s) with Parakeet", len(paths))
        # NeMo's type stubs are incomplete; transcribe() returns list of transcription results
        outputs = self.model.transcribe(list(paths), timestamps=timestamps)  # type: ignore[operator]
        return [_extract_text(item) for item in outputs]

    def transcribe_iter(self, paths: Iterable[str], *, timestamps: bool = False) -> list[str]:
        return self.transcribe_files(list(paths), timestamps=timestamps)

    def transcribe_wav(self, path: str) -> str:
        outputs = self.model.transcribe([path], batch_size=1)  # type: ignore[operator]
        if not outputs:
            return ""
        return _extract_text(outputs[0])

    def transcribe_samples(self, samples: np.ndarray, *, sample_rate: int = 16_000) -> str:
        """Transcribe in-memory audio and fall back to a temp wav on API mismatch."""
        audio = np.asarray(samples, dtype=np.float32).reshape(-1)
        if audio.size == 0:
            logger.debug("Skipping transcription for empty audio buffer")
            return ""
        try:
            outputs = self.model.transcribe([audio], batch_size=1, verbose=False)  # type: ignore[operator]
            if not outputs:
                return ""
            return _extract_text(outputs[0])
        except Exception as exc:  # noqa: BLE001 - compatibility fallback across NeMo variants
            logger.warning(
                "In-memory transcription failed ({}); falling back to temp wav",
                exc.__class__.__name__,
            )
            with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as tmp:
                path = Path(tmp.name)
            try:
                _write_audio_file(path, audio, sample_rate)
                return self.transcribe_wav(str(path))
            finally:
                path.unlink(missing_ok=True)


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
        self.fallback_reason: str | None = None
        self._helper_class_name: str | None = None

        self.offline = ParakeetTranscriber(model)
        self._init_helper()

    @property
    def helper_active(self) -> bool:
        return self.chunk_helper is not None

    def _init_helper(self) -> None:
        try:
            from nemo.collections.asr.parts.utils.streaming_utils import FrameBatchChunkedRNNT
        except ImportError as exc:  # pragma: no cover - environment dependent
            logger.warning("NeMo streaming utilities unavailable; using offline fallback: {}", exc)
            self.chunk_helper = None
            self.fallback_reason = f"import_failed:{exc.__class__.__name__}"
            return

        try:
            from nemo.collections.asr.parts.utils.streaming_utils import BatchedFrameASRTDT
        except ImportError:  # pragma: no cover - optional helper
            BatchedFrameASRTDT = None  # type: ignore[assignment]

        try:
            from omegaconf import open_dict
        except ImportError:  # pragma: no cover - optional dependency
            open_dict = None  # type: ignore[assignment]

        total_buffer_secs = self.chunk_secs + self.right_context_secs
        model_is_tdt = _is_tdt_model(self.model)
        max_steps_per_timestep = 5
        cfg = getattr(self.model, "_cfg", None) or getattr(self.model, "cfg", None)
        decoding_cfg = getattr(cfg, "decoding", None)
        greedy_cfg = getattr(decoding_cfg, "greedy", None)
        configured = _get_cfg_value(greedy_cfg, "max_symbols_per_step")
        if configured is not None:
            max_steps_per_timestep = int(configured)
        try:
            if model_is_tdt and BatchedFrameASRTDT is None:
                logger.warning(
                    "TDT model detected but BatchedFrameASRTDT is unavailable; "
                    "falling back to RNNT helper"
                )
            if model_is_tdt and BatchedFrameASRTDT is not None:
                try:
                    change_decoding = getattr(self.model, "change_decoding_strategy", None)
                    if callable(change_decoding) and decoding_cfg is not None:
                        try:
                            if open_dict is not None:
                                with open_dict(decoding_cfg):
                                    _set_cfg_value(decoding_cfg, "strategy", "greedy")
                                    _set_cfg_value(decoding_cfg, "preserve_alignments", True)
                                    _set_cfg_value(decoding_cfg, "fused_batch_size", -1)
                                    beam_cfg = _get_cfg_value(decoding_cfg, "beam")
                                    _set_cfg_value(beam_cfg, "return_best_hypothesis", True)
                                    _set_cfg_value(
                                        greedy_cfg,
                                        "max_symbols_per_step",
                                        int(max_steps_per_timestep),
                                    )
                            else:
                                _set_cfg_value(decoding_cfg, "strategy", "greedy")
                                _set_cfg_value(decoding_cfg, "preserve_alignments", True)
                                _set_cfg_value(decoding_cfg, "fused_batch_size", -1)
                                beam_cfg = _get_cfg_value(decoding_cfg, "beam")
                                _set_cfg_value(beam_cfg, "return_best_hypothesis", True)
                                _set_cfg_value(
                                    greedy_cfg, "max_symbols_per_step", int(max_steps_per_timestep)
                                )
                            change_decoding(decoding_cfg)
                        except Exception as exc:  # noqa: BLE001 - NeMo config shape varies by build
                            logger.warning(
                                "Failed to adjust decoding strategy for TDT streaming: {}", exc
                            )
                    tdt_batch_size = 1
                    if self.batch_size != tdt_batch_size:
                        logger.info(
                            "TDT streaming helper forces batch_size=1 (config requested {})",
                            self.batch_size,
                        )
                    self.chunk_helper = BatchedFrameASRTDT(
                        asr_model=self.model,
                        frame_len=self.chunk_secs,
                        total_buffer=total_buffer_secs,
                        batch_size=tdt_batch_size,
                        max_steps_per_timestep=max_steps_per_timestep,
                        stateful_decoding=False,
                    )
                    # BatchedFrameASRTDT doesn't pass stateful_decoding/max_steps to the base class.
                    self.chunk_helper.stateful_decoding = False
                    self.chunk_helper.max_steps_per_timestep = int(max_steps_per_timestep)
                    self._helper_class_name = "BatchedFrameASRTDT"
                    logger.info(
                        "Streaming helper initialised via {} (frame_len={}, total_buffer={}, "
                        "batch_size={})",
                        self._helper_class_name,
                        self.chunk_secs,
                        total_buffer_secs,
                        tdt_batch_size,
                    )
                    self.fallback_reason = None
                    return
                except Exception as exc:  # noqa: BLE001 - helper init is best-effort
                    logger.warning(
                        "TDT streaming helper init failed; falling back to RNNT helper: {}",
                        exc,
                    )
            self.chunk_helper = FrameBatchChunkedRNNT(
                asr_model=self.model,
                frame_len=cast(Any, self.chunk_secs),
                total_buffer=cast(Any, total_buffer_secs),
                batch_size=self.batch_size,
            )
            self._helper_class_name = "FrameBatchChunkedRNNT"
            logger.info(
                "Streaming helper initialised via {} "
                "(frame_len={}, total_buffer={}, batch_size={})",
                self._helper_class_name,
                self.chunk_secs,
                total_buffer_secs,
                self.batch_size,
            )
            self.fallback_reason = None
        except Exception as exc:  # noqa: BLE001 - streaming helper must fail open to offline mode
            logger.warning("Streaming helper init failed; using offline fallback: {}", exc)
            self.chunk_helper = None
            self.fallback_reason = f"init_failed:{exc.__class__.__name__}"

    def start_session(self, sample_rate: int) -> ParakeetStreamingSession:
        if self.chunk_helper is not None:
            try:
                self.chunk_helper.reset()
            except Exception as exc:  # noqa: BLE001 - reset failures should not break sessions
                logger.debug("Streaming helper reset failed, falling back to offline: {}", exc)
                self.chunk_helper = None
                self.fallback_reason = f"reset_failed:{exc.__class__.__name__}"
        return ParakeetStreamingSession(self, sample_rate)

    def _transcribe_offline(self, samples: np.ndarray, sample_rate: int) -> str:
        return self.offline.transcribe_samples(samples, sample_rate=sample_rate)


__all__ = [
    "load_parakeet_model",
    "ParakeetTranscriber",
    "ParakeetStreamingTranscriber",
    "ParakeetStreamingSession",
    "DEFAULT_MODEL_NAME",
]
