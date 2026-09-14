# yapper

Push-to-talk speech-to-text for Wayland, with a small GTK4 window instead of
[waystt](https://github.com/sevos/waystt)'s signal-only interface. Audio is
captured from PipeWire and transcribed on-device by whisper.cpp — nothing leaves
the machine and no API key is needed.

```
        ▁ ▃ ▁ ▂   ( ● )   ▂ ▁ ▃ ▁
             Listening  0:04
        Ctrl+Space or Escape to stop
 ┌──────────────────────────────────┐
 │ Remind me to pick up the...  0:04│
 │ 4 minutes ago                    │
 ├──────────────────────────────────┤
 │ Meeting notes: we agreed...  0:26│
 │ 5 hours ago                      │
 └──────────────────────────────────┘
 (delete)                (type)(copy)
```

The bars react to the microphone while you talk and settle into a still shape
when you stop — at which point the animation stops too, so an idle window costs
nothing. Colours follow the desktop's accent and light/dark preference.

Every transcript is kept, newest first. The buttons act on whatever row is
selected.

## Build

Needs a Rust toolchain plus GTK4, libadwaita, gtk4-layer-shell, cmake and clang
(whisper.cpp is built from source on the first compile, which takes a few
minutes). `wl-clipboard` is required for quick capture and recommended
otherwise.

```sh
cargo build --release
./scripts/fetch-model.sh base.en   # ~148 MB into ~/.local/share/yapper/models
./target/release/yapper
```

GPU inference is available as an opt-in feature if the SDK is installed:

```sh
cargo build --release --features vulkan   # or --features cuda
```

## Quick capture

`yapper --quick` is the keybind mode: a floating panel opens already recording,
and closing it puts the transcript on the clipboard.

```
bind = SUPER, D, exec, yapper --quick
```

Press the key, talk, press Escape. The text is on the clipboard by the time the
panel is gone.

```
      ╭──────────────────────────────╮
      │   ▁▃▁▂   ( ● )   ▂▁▃▁        │
      │     Listening  0:02          │
      │   Escape to stop and copy    │
      ╰──────────────────────────────╯
```

The panel is a layer-shell surface on the overlay layer, so it floats and takes
the keyboard without needing a compositor rule — the same treatment a launcher
like Vicinae gets. On a compositor without layer-shell it falls back to an
ordinary window, which you can float with a rule matching the app id
`dev.yapper.Yapper.Quick`.

Recording starts before the model has finished loading; the audio queues up
behind it. That's the difference between a keybind that feels instant and one
that doesn't.

Quick capture needs `wl-clipboard` installed. GTK's own clipboard is dropped
when the process exits, which is precisely when you want the text — so yapper
refuses to start in this mode without it rather than losing your words.

## Using it

| Action | How |
| --- | --- |
| Start/stop recording | The microphone button, or `Ctrl+Space` in the window |
| Stop recording | `Escape` |
| Toggle from anywhere | `pkill -USR1 yapper` |
| Quick capture | `yapper --quick` — records on open, copies on close |
| Reuse the text | Copied to the clipboard automatically; `Type` sends it to the focused window |
| Copy an older one | Select its row and press `Enter`, double-click it, or use the copy button |
| Delete one | Select its row and press `Delete`, or use the trash button |

To drive a window that's already open from a key, bind the signal instead:

```
bind = SUPER, SHIFT, D, exec, pkill -USR1 yapper || yapper
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
history_limit = 200        # how many past transcripts to keep
```

Bigger models are more accurate and slower: `tiny.en` (75 MB), `base.en`
(148 MB), `small.en` (488 MB), `medium.en` (1.5 GB), `large-v3` (3.1 GB). The
`.en` models are English-only; drop the suffix for multilingual ones.

## How it fits together

| File | Role |
| --- | --- |
| `src/audio.rs` | cpal capture, channel downmix, resampling to the 16 kHz mono Whisper wants |
| `src/transcribe.rs` | whisper.cpp on a dedicated thread, driven by async channels |
| `src/ui.rs` | The window and the recording state machine |
| `src/stage.rs` | The bars behind the button: layout, easing, and drawing |
| `src/style.css` | The button, status text and transcript frame |
| `src/output.rs` | Clipboard via `wl-copy`, typing via `wtype`/`ydotool` |
| `src/history.rs` | Past transcripts on disk, and the "5 minutes ago" labels |
| `src/config.rs` | `config.toml` handling |
| `src/cli.rs` | Argument parsing |

## Where things live

| What | Where |
| --- | --- |
| Settings | `~/.config/yapper/config.toml` |
| Models | `~/.local/share/yapper/models/` |
| Transcript history | `~/.local/state/yapper/history.jsonl` |

History is JSON Lines — one transcript per line, so it stays greppable and a
single damaged line costs you that entry rather than the file. Only the text is
kept; the audio is discarded once it has been transcribed.

## Not done yet

- Streaming transcription (audio is currently sent to Whisper once recording stops)
- A tray icon or a layer-shell overlay instead of a normal window
- Preferences UI — the config file is the only way to change settings
- Searching or editing past transcripts, and keeping the audio alongside them
- Proper voice activity detection (there is only a crude RMS gate that skips silent clips)
