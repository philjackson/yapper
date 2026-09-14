# yapper

Push-to-talk speech-to-text for Wayland, with a small GTK4 window instead of
[waystt](https://github.com/sevos/waystt)'s signal-only interface. Audio is
captured from PipeWire and transcribed on-device by whisper.cpp — nothing leaves
the machine and no API key is needed.

```
┌──────────────────────────────┐
│ ● Recording  0:04            │
│ ▁▃▅▇▇▅▃▂▁▁▂▄▆▇▅▃▁            │
│        [  Stop  ]            │
│ ┌──────────────────────────┐ │
│ │ the transcript lands here│ │
│ └──────────────────────────┘ │
│        Clear  Type  Copy     │
└──────────────────────────────┘
```

## Build

Needs a Rust toolchain plus GTK4, libadwaita, cmake and clang (whisper.cpp is
built from source on the first compile, which takes a few minutes).

```sh
cargo build --release
./scripts/fetch-model.sh base.en   # ~148 MB into ~/.local/share/yapper/models
./target/release/yapper
```

GPU inference is available as an opt-in feature if the SDK is installed:

```sh
cargo build --release --features vulkan   # or --features cuda
```

## Using it

| Action | How |
| --- | --- |
| Start/stop recording | The Record button, or `Ctrl+Space` in the window |
| Stop recording | `Escape` |
| Toggle from anywhere | `pkill -USR1 yapper` |
| Reuse the text | Copied to the clipboard automatically; `Type` sends it to the focused window |

Bind the signal to a key in Hyprland so the window doesn't need focus:

```
bind = SUPER, D, exec, pkill -USR1 yapper || yapper
```

`Type` needs [`wtype`](https://github.com/atx/wtype) or `ydotool` (with
`ydotoold` running); the button stays disabled when neither is installed.

## Configuration

Written on first run to `~/.config/yapper/config.toml`:

```toml
model_path = "/home/you/.local/share/yapper/models/ggml-base.en.bin"
language = "auto"          # or an ISO code like "en", "de"
translate = false          # translate to English instead of transcribing
threads = 0                # 0 = pick from the CPU count
copy_to_clipboard = true
type_on_finish = false     # type straight into the focused window
append_transcripts = true  # keep earlier transcripts in the window
```

Bigger models are more accurate and slower: `tiny.en` (75 MB), `base.en`
(148 MB), `small.en` (488 MB), `medium.en` (1.5 GB), `large-v3` (3.1 GB). The
`.en` models are English-only; drop the suffix for multilingual ones.

## How it fits together

| File | Role |
| --- | --- |
| `src/audio.rs` | cpal capture, channel downmix, resampling to the 16 kHz mono Whisper wants |
| `src/transcribe.rs` | whisper.cpp on a dedicated thread, driven by async channels |
| `src/ui.rs` | The window, the level meter, and the recording state machine |
| `src/output.rs` | Clipboard via `wl-copy`, typing via `wtype`/`ydotool` |
| `src/config.rs` | `config.toml` handling |

## Not done yet

- Streaming transcription (audio is currently sent to Whisper once recording stops)
- A tray icon or a layer-shell overlay instead of a normal window
- Preferences UI — the config file is the only way to change settings
- Proper voice activity detection (there is only a crude RMS gate that skips silent clips)
