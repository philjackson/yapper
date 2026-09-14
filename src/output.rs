//! Getting the transcript out of the window and into whatever you were typing in.

use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow};
use std::io::Write;

/// Copy via `wl-copy` when it exists: unlike GTK's own clipboard, its contents
/// survive yapper exiting. Falls back to the GTK clipboard otherwise.
pub fn copy(text: &str) -> Result<()> {
    if which("wl-copy").is_some() {
        let mut child = Command::new("wl-copy")
            .arg("--")
            .arg(text)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("running wl-copy")?;
        // wl-copy daemonises itself, so this returns promptly.
        child.wait().ok();
        return Ok(());
    }

    use gtk::prelude::DisplayExt;
    let display = gtk::gdk::Display::default().ok_or_else(|| anyhow!("no display"))?;
    display.clipboard().set_text(text);
    Ok(())
}

/// Whether `wl-copy` is available. It matters because its clipboard survives
/// yapper exiting, while GTK's own does not.
pub fn has_wl_copy() -> bool {
    which("wl-copy").is_some()
}

/// The tool we'd use to type into the focused window, if any is installed.
pub fn typing_backend() -> Option<&'static str> {
    ["wtype", "ydotool"].into_iter().find(|t| which(t).is_some())
}

/// Type the text into whichever window has focus.
pub fn type_text(text: &str) -> Result<()> {
    match typing_backend() {
        Some("wtype") => {
            // `--` so text starting with a dash isn't read as a flag.
            let status = Command::new("wtype")
                .arg("--")
                .arg(text)
                .status()
                .context("running wtype")?;
            if !status.success() {
                return Err(anyhow!("wtype exited with {status}"));
            }
            Ok(())
        }
        Some("ydotool") => {
            let mut child = Command::new("ydotool")
                .args(["type", "--file", "-"])
                .stdin(Stdio::piped())
                .spawn()
                .context("running ydotool (is ydotoold running?)")?;
            child
                .stdin
                .as_mut()
                .ok_or_else(|| anyhow!("no stdin on ydotool"))?
                .write_all(text.as_bytes())?;
            let status = child.wait()?;
            if !status.success() {
                return Err(anyhow!("ydotool exited with {status}"));
            }
            Ok(())
        }
        _ => Err(anyhow!(
            "no typing backend found; install wtype (or ydotool with ydotoold running)"
        )),
    }
}

fn which(program: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}
