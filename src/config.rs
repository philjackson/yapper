//! User configuration, read from `~/.config/yapper/config.toml`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Path to a ggml Whisper model. See `scripts/fetch-model.sh`.
    pub model_path: PathBuf,
    /// ISO language code, or "auto" to let Whisper detect it.
    pub language: String,
    /// Translate to English instead of transcribing verbatim.
    pub translate: bool,
    /// Inference threads; 0 picks a sensible default from the CPU count.
    pub threads: u32,
    /// Put the transcript on the clipboard as soon as it arrives.
    pub copy_to_clipboard: bool,
    /// Type the transcript into the focused window (needs wtype or ydotool).
    pub type_on_finish: bool,
    /// Keep earlier transcripts in the window instead of replacing them.
    pub append_transcripts: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model_path: default_model_path(),
            language: "auto".to_string(),
            translate: false,
            threads: 0,
            copy_to_clipboard: true,
            type_on_finish: false,
            append_transcripts: true,
        }
    }
}

impl Config {
    /// Load the config file, creating it with defaults on first run.
    pub fn load() -> Result<Self> {
        let path = config_path();
        if !path.exists() {
            let cfg = Config::default();
            cfg.save()?;
            return Ok(cfg);
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
    }

    /// `None` means "auto-detect", which is what whisper.cpp expects.
    pub fn language_code(&self) -> Option<&str> {
        match self.language.as_str() {
            "" | "auto" => None,
            other => Some(other),
        }
    }

    pub fn thread_count(&self) -> i32 {
        if self.threads > 0 {
            return self.threads as i32;
        }
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        // Whisper scales poorly past ~8 threads and we want the UI to stay responsive.
        cores.saturating_sub(1).clamp(1, 8) as i32
    }
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("yapper/config.toml")
}

pub fn models_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("yapper/models")
}

fn default_model_path() -> PathBuf {
    models_dir().join("ggml-base.en.bin")
}
