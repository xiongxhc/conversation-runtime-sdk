# Desktop Reference App

The desktop app will prove the SDK through a local voice companion on macOS Apple Silicon.

This directory intentionally contains no Tauri or frontend dependencies yet. The desktop shell starts after the deterministic runtime contracts pass and the feasibility benchmark selects concrete ASR, language-model, TTS, audio-capture, and playback backends.

Its first implementation must consume the runtime through public protocol and adapter interfaces. Desktop-only types must not enter the core crates.
