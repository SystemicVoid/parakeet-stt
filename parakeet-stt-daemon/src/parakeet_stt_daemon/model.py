"""Parakeet model loader helpers.

The import is deferred so environments without `nemo_toolkit[asr]` can still
start the daemon for protocol testing.
"""

from __future__ import annotations

import math
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


def _coerce_rnnt_texts(result: Any) -> list[str]:
    """Normalize rnnt_decoder_predictions_tensor output across NeMo versions."""
    if isinstance(result, tuple) and result:
        result = result[0]
    if not isinstance(result, list):
        return [_extract_text(result)]
    if result and isinstance(result[0], list):
        texts: list[str] = []
        for group in result:
            if not group:
                texts.append("")
                continue
            texts.append(_coerce_rnnt_texts(group)[0])
        return texts
    return [_extract_text(item) for item in result]


def _normalize_streaming_text(result: Any) -> str:
    if isinstance(result, tuple) and result:
        return _normalize_streaming_text(result[0])
    if isinstance(result, list):
        if not result:
            return ""
        return _extract_text(result[0])
    return _extract_text(result)


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


def _compute_streaming_tokens(
    model: ASRModel, *, chunk_secs: float, total_buffer_secs: float
) -> tuple[int, int, float]:
    cfg = getattr(model, "_cfg", None) or getattr(model, "cfg", None)
    preprocessor = getattr(cfg, "preprocessor", None)
    window_stride = getattr(preprocessor, "window_stride", None)
    subsampling = getattr(getattr(model, "encoder", None), "subsampling_factor", None)
    if window_stride is None or subsampling is None:
        raise ValueError("missing model stride metadata")
    model_stride = float(window_stride) * float(subsampling)
    if model_stride <= 0:
        raise ValueError("invalid model stride")
    tokens_per_chunk = max(1, int(math.ceil(chunk_secs / model_stride)))
    delay = max(
        0,
        int(
            math.ceil(
                (chunk_secs + (float(total_buffer_secs) - float(chunk_secs)) / 2) / model_stride
            )
        ),
    )
    return tokens_per_chunk, delay, model_stride


def _get_cfg_value(cfg: Any, key: str) -> Any:
    if cfg is None:
        return None
    if isinstance(cfg, dict):
        return cfg.get(key)
    try:
        return cfg[key]
    except Exception:
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
    except Exception:
        return False
    return False


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
        except Exception as exc:  # pragma: no cover - best-effort
            logger.warning("Failed to adjust attention window: {}", exc)

    object.__setattr__(model, "_parakeet_effective_device", resolved_device)
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
        helper = self._parent.chunk_helper
        iter_cls = self._parent._audio_feature_iter_cls
        tokens_per_chunk = self._parent._helper_tokens_per_chunk
        delay = self._parent._helper_delay
        model_stride_secs = self._parent._helper_model_stride_secs
        if helper is not None and iter_cls is not None:
            try:
                if (
                    tokens_per_chunk is not None
                    and delay is not None
                    and model_stride_secs is not None
                    and delay > 0
                ):
                    pad_samples = int(delay * model_stride_secs * self.sample_rate)
                    if pad_samples > 0:
                        combined = np.pad(combined, (0, pad_samples))
                pad_to_frame_len = tokens_per_chunk is not None and delay is not None
                frame_reader = iter_cls(
                    combined,
                    helper.frame_len,
                    helper.raw_preprocessor,
                    helper.asr_model.device,
                    pad_to_frame_len=pad_to_frame_len,
                )
                try:
                    helper.set_frame_reader(frame_reader, 0)
                except TypeError:
                    helper.set_frame_reader(frame_reader)
                if tokens_per_chunk is not None and delay is not None:
                    result = helper.transcribe(tokens_per_chunk, delay)
                else:
                    result = helper.transcribe()
                return _normalize_streaming_text(result)
            except Exception as exc:  # pragma: no cover - fallback path
                logger.warning("Streaming helper failed during finalization: {}", exc)
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
        self._audio_feature_iter_cls: type | None = None
        self._helper_class_name: str | None = None
        self._helper_tokens_per_chunk: int | None = None
        self._helper_delay: int | None = None
        self._helper_model_stride_secs: float | None = None

        self.offline = ParakeetTranscriber(model)
        self._init_helper()

    @property
    def helper_active(self) -> bool:
        return self.chunk_helper is not None

    def _init_helper(self) -> None:
        try:
            from nemo.collections.asr.parts.utils.streaming_utils import (
                AudioFeatureIterator,
                FrameBatchChunkedRNNT,
            )
        except ImportError as exc:  # pragma: no cover - environment dependent
            logger.warning("NeMo streaming utilities unavailable; using offline fallback: {}", exc)
            self.chunk_helper = None
            self.fallback_reason = f"import_failed:{exc.__class__.__name__}"
            return

        try:
            from nemo.collections.asr.parts.utils.streaming_utils import BatchedFrameASRTDT
        except Exception:  # pragma: no cover - optional helper
            BatchedFrameASRTDT = None  # type: ignore[assignment]

        try:
            from omegaconf import open_dict
        except Exception:  # pragma: no cover - optional dependency
            open_dict = None  # type: ignore[assignment]

        class _PatchedFrameBatchChunkedRNNT(FrameBatchChunkedRNNT):
            @torch.no_grad()
            def _get_batch_preds(self, keep_logits: bool = False) -> None:  # type: ignore[override]
                device = self.asr_model.device
                for batch in iter(self.data_loader):
                    feat_signal, feat_signal_len = batch
                    feat_signal, feat_signal_len = (
                        feat_signal.to(device),
                        feat_signal_len.to(device),
                    )
                    encoded, encoded_len = self.asr_model(
                        processed_signal=feat_signal, processed_signal_length=feat_signal_len
                    )
                    decoded = self.asr_model.decoding.rnnt_decoder_predictions_tensor(
                        encoder_output=encoded, encoded_lengths=encoded_len, return_hypotheses=False
                    )
                    self.all_preds.extend(_coerce_rnnt_texts(decoded))
                    del encoded
                    del encoded_len

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
                    tokens_per_chunk, delay, model_stride_secs = _compute_streaming_tokens(
                        self.model,
                        chunk_secs=self.chunk_secs,
                        total_buffer_secs=total_buffer_secs,
                    )
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
                        except Exception as exc:  # pragma: no cover - best effort
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
                        stateful_decoding=True,
                    )
                    # BatchedFrameASRTDT doesn't pass stateful_decoding/max_steps to the base class.
                    self.chunk_helper.stateful_decoding = True
                    self.chunk_helper.max_steps_per_timestep = int(max_steps_per_timestep)
                    self._audio_feature_iter_cls = AudioFeatureIterator
                    self._helper_class_name = "BatchedFrameASRTDT"
                    self._helper_tokens_per_chunk = tokens_per_chunk
                    self._helper_delay = delay
                    self._helper_model_stride_secs = model_stride_secs
                    logger.info(
                        "Streaming helper initialised via {} "
                        "(frame_len={}, total_buffer={}, batch_size={}, "
                        "tokens_per_chunk={}, delay={})",
                        self._helper_class_name,
                        self.chunk_secs,
                        total_buffer_secs,
                        tdt_batch_size,
                        tokens_per_chunk,
                        delay,
                    )
                    self.fallback_reason = None
                    return
                except Exception as exc:  # pragma: no cover - optional helper
                    logger.warning(
                        "TDT streaming helper init failed; falling back to RNNT helper: {}",
                        exc,
                    )
            self.chunk_helper = _PatchedFrameBatchChunkedRNNT(
                asr_model=self.model,
                frame_len=self.chunk_secs,
                total_buffer=total_buffer_secs,
                batch_size=self.batch_size,
            )
            self._audio_feature_iter_cls = AudioFeatureIterator
            self._helper_class_name = "FrameBatchChunkedRNNT"
            self._helper_tokens_per_chunk = None
            self._helper_delay = None
            self._helper_model_stride_secs = None
            logger.info(
                "Streaming helper initialised via {} "
                "(frame_len={}, total_buffer={}, batch_size={})",
                self._helper_class_name,
                self.chunk_secs,
                total_buffer_secs,
                self.batch_size,
            )
            self.fallback_reason = None
        except Exception as exc:  # pragma: no cover - environment dependent
            logger.warning("Streaming helper init failed; using offline fallback: {}", exc)
            self.chunk_helper = None
            self._audio_feature_iter_cls = None
            self._helper_tokens_per_chunk = None
            self._helper_delay = None
            self._helper_model_stride_secs = None
            self.fallback_reason = f"init_failed:{exc.__class__.__name__}"

    def start_session(self, sample_rate: int) -> ParakeetStreamingSession:
        if self.chunk_helper is not None:
            try:
                self.chunk_helper.reset()
            except Exception as exc:  # pragma: no cover
                logger.debug("Streaming helper reset failed, falling back to offline: {}", exc)
                self.chunk_helper = None
                self._audio_feature_iter_cls = None
                self._helper_tokens_per_chunk = None
                self._helper_delay = None
                self._helper_model_stride_secs = None
                self.fallback_reason = f"reset_failed:{exc.__class__.__name__}"
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
