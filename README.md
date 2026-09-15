# yapper

Push-to-talk dictation for Wayland. Records from PipeWire, transcribes with
whisper.cpp on your own machine, puts the text on your clipboard. No API key,
nothing leaves the box.

Inspired by [waystt](https://github.com/sevos/waystt), but with a window.

[Download the latest release](https://github.com/philjackson/yapper/releases/latest)
· [all releases](https://github.com/philjackson/yapper/releases)

> **Written by AI.** Claude wrote the code, the tests and this README from my
> prompts. Worth knowing before you run it.

<p align="center">
  <img src="docs/window.png" alt="The yapper window" width="480">
</p>

Past transcripts stay in a list and the buttons act on whichever one you pick.
While you talk the bars follow your voice and a running transcript appears under
the button.

## Install

Needs Rust, GTK4, libadwaita, gtk4-layer-shell, cmake, clang and wl-clipboard.
whisper.cpp builds from source the first time, which takes a few minutes.

```sh
cargo build --release
./scripts/fetch-model.sh base.en    # 148 MB
install -Dm755 target/release/yapper ~/.local/bin/yapper
install -Dm644 data/dev.yapper.Yapper.desktop \
    ~/.local/share/applications/dev.yapper.Yapper.desktop
```

Vulkan is on by default and makes transcription roughly 5× faster. Without the
SDK, build `--no-default-features` for CPU only, or add `-F cuda` for NVIDIA.

## Quick capture

The point of the whole thing. Bind it to a key:

```
bind = SUPER, D, exec, yapper --quick
```

<p align="center">
  <img src="docs/quick.png" alt="The quick capture panel" width="420">
</p>

A panel appears, already recording. Talk, press Enter, and the text is on your
clipboard before the panel has gone. Escape throws it away instead.

It's a layer-shell surface, so it floats above everything without needing a
window rule. Recording starts before the model has finished loading, so there's
nothing to wait for.

## Keys

| | |
| --- | --- |
| `Ctrl+Space` | start recording |
| `Enter` or `Space` | finish, transcribe, copy |
| `Escape` | discard the recording |
| `Ctrl+C` | copy the selected transcript |
| `Delete` | delete the selected transcript |
| `Ctrl+,` | preferences |

`pkill -USR1 yapper` toggles recording from outside, if you'd rather drive an
already-open window from a key.

## Settings

In the header bar menu, or `Ctrl+,`.

<p align="center">
  <img src="docs/preferences.png" alt="The preferences dialog" width="420">
</p>

The same settings live in `~/.config/yapper/config.toml`:

```toml
model_path = "/home/you/.local/share/yapper/models/ggml-base.en.bin"
language = "auto"          # or an ISO code like "en", "de"
translate = false          # translate to English rather than transcribe
threads = 0                # 0 picks from the CPU count
copy_to_clipboard = true
type_on_finish = false
history_limit = 200
live_preview = true
silence_timeout = 0.0
pause_players = true
initial_prompt = ""
```

Four worth knowing about:

**initial_prompt** is a vocabulary. Whisper reads it before your speech and
leans towards those words, so names and terms it keeps mangling — colleagues,
places, anything unusual — start coming out right. Preferences explains it
under **Vocabulary**. Keep it to a line or two; a long list starts crowding out
the audio.

**pause_players** pauses whatever is playing over MPRIS while you dictate and
starts it again afterwards, because anything out of the speakers ends up in the
transcript. Only players that were actually playing get resumed, so something
you had paused yourself stays that way. On by default.

**silence_timeout** ends a recording after that many seconds of quiet, so you
needn't press anything to finish. A ring drains around the button while it
counts down. It's off by default because a pause to think looks exactly like a
pause because you've finished; two or three seconds suits dictation.

**live_preview** re-transcribes what it has heard twice a second so words appear
as you talk. Earlier ones can change as context arrives. What gets copied is a
separate full pass at the end, not the preview.

Bigger models are slower and more accurate: tiny 75 MB, base 148 MB, small
488 MB, medium 1.5 GB, large 3.1 GB. Preferences has a link to download more.

Typing into the focused window instead of the clipboard needs
[wtype](https://github.com/atx/wtype) or ydotool; the button is disabled
without one.

## Files

| | |
| --- | --- |
| settings | `~/.config/yapper/config.toml` |
| models | `~/.local/share/yapper/models/` |
| history | `~/.local/state/yapper/history.jsonl` |

History is JSON Lines, one transcript per line, so it stays greppable and a
damaged line only costs you that entry. Only the text is kept. The audio goes
as soon as it has been transcribed.

## Todo

- search the history
- adaptive silence threshold rather than a fixed level
- keep the audio alongside the text
