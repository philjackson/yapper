# yapper

Yapper is a small dictation app for Wayland. Press a key, say what you want to
write, and paste the transcript wherever you need it. It records through
PipeWire and uses whisper.cpp to transcribe on your machine. You don't need an
API key, and your audio stays local.

Inspired by [waystt](https://github.com/sevos/waystt), but with a modal window.

[Download the latest release](https://github.com/philjackson/yapper/releases/latest)
· [all releases](https://github.com/philjackson/yapper/releases)

> **A note on the code:** AI was used exstensively to build this.

<p align="center">
  <img src="docs/window.png" alt="The yapper window" width="480">
</p>

The window keeps a list of past transcripts. Select one to copy or delete it.
While you're recording, the bars respond to your voice and a live transcript
appears below the button.

## Install

To build from source, you'll need Rust, GTK4, libadwaita, gtk4-layer-shell, cmake,
clang and wl-clipboard. The first build also compiles whisper.cpp.

```sh
cargo build --release
./scripts/fetch-model.sh base.en    # 148 MB
install -Dm755 target/release/yapper ~/.local/bin/yapper
install -Dm644 data/dev.yapper.Yapper.desktop \
    ~/.local/share/applications/dev.yapper.Yapper.desktop
```

Vulkan support is enabled by default and can make transcription roughly 5×
faster. If you don't have the Vulkan SDK, use `cargo build --release
--no-default-features` for a CPU-only build. For NVIDIA CUDA, use
`cargo build --release --no-default-features -F cuda`.

## Quick capture

Bind `yapper --quick` to a key to start dictating from wherever you're working.
For example, in Hyprland:

```ini
bind = SUPER, D, exec, yapper --quick
```

<p align="center">
  <img src="docs/quick.png" alt="The quick capture panel" width="420">
</p>

The panel opens and starts recording straight away. When you're done, press
Enter to transcribe and copy the text to your clipboard. The panel then closes.
Press Escape if you want to discard the recording.

The panel uses layer-shell to stay above other windows, so you don't need a
window rule. Recording starts while the model is loading; you can begin
speaking as soon as the panel opens.

The first launch takes a couple of hundred milliseconds. After that, yapper
stays running in the background with the panel hidden. There's no separate
daemon to set up.

To stop the background process and free the model's memory:

```sh
yapper --stop-daemon
```

## Keyboard shortcuts

| Key | Action |
| --- | --- |
| `Ctrl+Space` | Start recording |
| `Enter` or `Space` | Finish, transcribe and copy |
| `Escape` | Discard the recording |
| `Ctrl+C` | Copy the selected transcript |
| `Delete` | Delete the selected transcript |
| `Ctrl+,` | Open preferences |

You can also toggle recording in an already-open window with
`pkill -USR1 yapper`, which is handy for a separate key binding.

## Settings

Open preferences from the header bar menu or press `Ctrl+,`.

<p align="center">
  <img src="docs/preferences.png" alt="The preferences dialog" width="420">
</p>

You can also edit the settings in `~/.config/yapper/config.toml`:

```toml
model_path = "/home/you/.local/share/yapper/models/ggml-base.en.bin"
language = "auto"           # or an ISO code like "en", "de"
translate = false           # translate speech into English
threads = 0                 # 0 chooses based on your CPU count
input_device = ""           # empty uses the system default
copy_to_clipboard = true
type_on_finish = false
history_limit = 200
live_preview = true
silence_timeout = 0.0
pause_players = true
initial_prompt = ""
```

**Vocabulary (`initial_prompt`)** helps with names and terms that Whisper gets
wrong. Add a line or two of words you'd like it to recognise, such as colleagues'
names or places you mention often. Whisper uses these as context when
transcribing. Keep it short, as a long list can crowd out the audio context.
You'll find this under **Vocabulary** in preferences.

**Pause players (`pause_players`)** pauses media players that support MPRIS while
you dictate, so sound from your speakers is less likely to end up in the
transcript. It resumes them afterwards, but only if they were playing when you
started. This is on by default.

**Silence timeout (`silence_timeout`)** finishes a recording after the number of
quiet seconds you choose. A ring around the button shows the countdown. It's
off by default, since you might just be pausing to think. Try two or three
seconds if you'd like recordings to finish automatically.

**Live preview (`live_preview`)** updates the transcript twice a second while
you talk. Earlier words may change as Whisper gets more context. When you
finish, yapper transcribes the full recording again and copies that final
version to the clipboard.

Larger models generally give more accurate results but take longer to run.
Their approximate download sizes are: tiny 75 MB, base 148 MB, small 488 MB,
medium 1.5 GB and large 3.1 GB. Preferences includes a link to download more
models.

To type a transcript directly into the focused window, you'll need
[wtype](https://github.com/atx/wtype). The typing button is disabled if it isn't
installed.

## Where files live

| File | Location |
| --- | --- |
| Settings | `~/.config/yapper/config.toml` |
| Models | `~/.local/share/yapper/models/` |
| History | `~/.local/state/yapper/history.jsonl` |

History is stored as JSON Lines, with one transcript per line. You can search
it with grep, and a damaged line won't prevent the other entries from loading.
Only the text is saved; audio is discarded after transcription.

## Todo

- Search transcript history in the app
- Adjust the silence threshold automatically
- Add an option to keep audio alongside the text
