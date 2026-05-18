# Two-process split: Python daemon + Rust client

The ASR runtime (NeMo Parakeet) is Python-native; the desktop integration (evdev global
hotkey, Wayland-aware text injection, overlay) is best in Rust. We split them into two
processes communicating over a localhost WebSocket rather than embedding either side
into the other. The cost is a small IPC surface and dual deployment; the benefit is that
each side stays idiomatic and we avoid PyO3 / FFI complexity for the hot path. This
shape is load-bearing: changing it would mean rewriting either the ASR loop or the OS
integration in the other language.
