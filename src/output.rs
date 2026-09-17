//! Getting the transcript out of the window and into whatever you were typing in.

use std::process::{Command, Stdio};
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};

use crate::history::plural;

const TYPING_TOOL: &str = "wtype";

/// Copy via `wl-copy` when it exists: unlike GTK's own clipboard, its contents
/// survive yapper exiting. Falls back to the GTK clipboard otherwise.
///
/// A successful copy raises a desktop notification. Quick capture is gone from
/// the screen by the time the text lands, so without one there is nothing at
/// all to confirm it worked.
pub fn copy(text: &str) -> Result<()> {
    copy_to_clipboard(text)?;
    notify(&copied_message(text));
    Ok(())
}

fn copy_to_clipboard(text: &str) -> Result<()> {
    if has_wl_copy() {
        let mut child = quiet("wl-copy")
            .arg("--")
            .arg(text)
            .spawn()
            .context("running wl-copy")?;
        // wl-copy daemonises itself, so this returns promptly.
        let _ = child.wait();
        return Ok(());
    }

    use gtk::prelude::DisplayExt;
    let display = gtk::gdk::Display::default().ok_or_else(|| anyhow!("no display"))?;
    display.clipboard().set_text(text);
    Ok(())
}

/// What the notification says. Words are whitespace-separated, which is what a
/// person counting them would say too.
fn copied_message(text: &str) -> String {
    let words = text.split_whitespace().count();
    format!("Copied {words} word{} to the clipboard", plural(words))
}

/// Best effort: no notification daemon, or no notify-send, is not an error
/// worth interrupting a dictation for.
fn notify(body: &str) {
    if !installed("notify-send") {
        return;
    }
    let sent = quiet("notify-send")
        .args([
            "--app-name=yapper",
            "--icon=audio-input-microphone",
            "--expire-time=3000",
            body,
        ])
        .spawn();
    // Reaped rather than left behind, so a long-lived window doesn't collect
    // zombies. notify-send returns as soon as the daemon has the message.
    if let Ok(mut child) = sent {
        let _ = child.wait();
    }
}

/// Whether `wl-copy` is available. It matters because its clipboard survives
/// yapper exiting, while GTK's own does not.
pub fn has_wl_copy() -> bool {
    static FOUND: OnceLock<bool> = OnceLock::new();
    *FOUND.get_or_init(|| installed("wl-copy"))
}

/// Whether we can type into the focused window.
pub fn can_type() -> bool {
    static FOUND: OnceLock<bool> = OnceLock::new();
    *FOUND.get_or_init(|| installed(TYPING_TOOL))
}

/// Type the text into whichever window has focus.
pub fn type_text(text: &str) -> Result<()> {
    if !can_type() {
        return Err(anyhow!("wtype is not installed"));
    }
    // `--` so text starting with a dash isn't read as a flag.
    let status = Command::new(TYPING_TOOL)
        .arg("--")
        .arg(text)
        .status()
        .context("running wtype")?;
    if !status.success() {
        return Err(anyhow!("wtype exited with {status}"));
    }
    Ok(())
}

fn installed(program: &str) -> bool {
    gtk::glib::find_program_in_path(program).is_some()
}

/// A command with nothing attached to the terminal, for helpers whose output
/// is noise.
fn quiet(program: &str) -> Command {
    let mut command = Command::new(program);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_counts_words_and_gets_the_plural_right() {
        assert_eq!(copied_message("hello"), "Copied 1 word to the clipboard");
        assert_eq!(
            copied_message("  hello   there, world \n"),
            "Copied 3 words to the clipboard"
        );
        assert_eq!(copied_message(""), "Copied 0 words to the clipboard");
    }
}
