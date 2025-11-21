"""Parakeet model loader helpers.

The import is deferred so environments without `nemo_toolkit[asr]` can still
start the daemon for protocol testing.
"""

from __future__ import annotations

from collections.abc import Iterable, Sequence
from dataclasses import dataclass
from importlib import import_module
from typing import TYPE_CHECKING, Any

from loguru import logger

DEFAULT_MODEL_NAME = "nvidia/parakeet-tdt-0.6b-v3"

if TYPE_CHECKING:  # pragma: no cover
    import nemo.collections.asr as nemo_asr
    from nemo.collections.asr.models import ASRModel
else:
    nemo_asr = None  # type: ignore
    ASRModel = Any  # type: ignore


def load_parakeet_model(model_name: str = DEFAULT_MODEL_NAME, device: str = "cuda") -> ASRModel:
    """Load the Parakeet model with a minimal amount of glue."""
    try:
        nemo_asr = import_module("nemo.collections.asr")
    except ImportError as exc:  # pragma: no cover - runtime guard
        raise RuntimeError(
            "nemo_toolkit[asr] is not installed; install with `uv sync --extra inference`"
        ) from exc

    model: ASRModel = nemo_asr.models.ASRModel.from_pretrained(model_name=model_name)
    model.to(device)

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
        try:
            _ = self.model.transcribe([""], batch_size=1)
        except Exception as exc:  # pragma: no cover - warmup is optional
            logger.debug("Warmup skipped: {}", exc)

    def transcribe_files(self, paths: Sequence[str], *, timestamps: bool = False) -> list[str]:
        if not paths:
            return []
        logger.info("Transcribing {} file(s) with Parakeet", len(paths))
        outputs = self.model.transcribe(list(paths), timestamps=timestamps)
        # `transcribe` returns a list; normalise to list[str]
        return [str(item) for item in outputs]

    def transcribe_iter(self, paths: Iterable[str], *, timestamps: bool = False) -> list[str]:
        return self.transcribe_files(list(paths), timestamps=timestamps)


__all__ = ["load_parakeet_model", "ParakeetTranscriber", "DEFAULT_MODEL_NAME"]
