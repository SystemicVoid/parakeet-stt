"""Quick inference probe plus repeatable offline benchmark harness for Parakeet."""

from __future__ import annotations

import sys
from pathlib import Path

# Bootstrap sibling package imports for both direct execution and importlib loading.
_THIS_DIR = Path(__file__).resolve().parent
if str(_THIS_DIR) not in sys.path:
    sys.path.insert(0, str(_THIS_DIR))

from check_model_lib.cli import (  # noqa: E402
    _apply_profile_defaults,  # noqa: F401 - re-export for test compatibility
    main,
    parse_args,  # noqa: F401
)
from check_model_lib.corpus import (  # noqa: E402
    _resolve_benchmark_cases,  # noqa: F401 - re-export for test compatibility
    collect_benchmark_cases,  # noqa: F401 - re-export for test compatibility
    parse_benchmark_manifest,  # noqa: F401 - re-export for test compatibility
    parse_benchmark_transcripts,  # noqa: F401 - re-export for test compatibility
)
from check_model_lib.metrics import (  # noqa: E402
    compute_command_exact_match_rate,  # noqa: F401 - re-export for test compatibility
    compute_command_match_metrics,  # noqa: F401 - re-export for test compatibility
    compute_critical_token_recall,  # noqa: F401 - re-export for test compatibility
    compute_normalized_wer,  # noqa: F401 - re-export for test compatibility
    compute_punctuation_metrics,  # noqa: F401 - re-export for test compatibility
    compute_weighted_wer,  # noqa: F401 - re-export for test compatibility
    normalize_command_text,  # noqa: F401 - re-export for test compatibility
    normalize_transcript,  # noqa: F401 - re-export for test compatibility
    parse_command_intent_slots,  # noqa: F401 - re-export for test compatibility
    summarize_timings_ms,  # noqa: F401 - re-export for test compatibility
)
from check_model_lib.runtime import (  # noqa: E402
    _transcribe_stream_seal,  # noqa: F401 - re-export for test compatibility
)
from check_model_lib.thresholds import evaluate_regression_thresholds  # noqa: E402, F401

if __name__ == "__main__":
    raise SystemExit(main())
