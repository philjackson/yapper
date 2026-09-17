//! User configuration, read from `~/.config/yapper/config.toml`.

use std::path::{Path, PathBuf};

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
    /// Type the transcript into the focused window (needs wtype).
    pub type_on_finish: bool,
    /// How many past transcripts to keep. Older ones fall off the end.
    pub history_limit: usize,
    /// Show a running transcript while you talk. Costs a re-transcription
    /// every half second, so it wants a GPU build to feel free.
    pub live_preview: bool,
    /// Stop recording after this many seconds of silence. 0 waits for you to
    /// stop it yourself.
    pub silence_timeout: f32,
    /// How loud counts as speech, as an RMS level. Room tone sits below it,
    /// speech above. Preferences can show you where yours falls.
    pub silence_threshold: f32,
    /// Pause anything that is playing while you dictate, and start it again
    /// afterwards. Speakers bleed into the microphone.
    pub pause_players: bool,
    /// Which microphone to record from, as a cpal device id. Empty means
    /// whichever one the system calls default.
    pub input_device: String,
    /// Words to expect: names and jargon Whisper would otherwise guess at.
    /// Passed to the model as context for every transcription.
    pub initial_prompt: String,
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
            history_limit: 200,
            live_preview: true,
            silence_timeout: 0.0,
            silence_threshold: crate::audio::DEFAULT_SILENCE_RMS,
            pause_players: true,
            input_device: String::new(),
            initial_prompt: String::new(),
        }
    }
}

impl Config {
    /// Load the config file, creating it with defaults on first run.
    pub fn load() -> Result<Self> {
        Self::load_from(&config_path())
    }

    /// As [`Self::load`], but against a given file. Tests use this.
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            let config = Config::default();
            config.save_to(path)?;
            return Ok(config);
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&config_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
    }

    /// Empty means no prompt at all, rather than an empty one.
    pub fn prompt(&self) -> Option<&str> {
        let prompt = self.initial_prompt.trim();
        (!prompt.is_empty()).then_some(prompt)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("yapper-config-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    #[test]
    fn settings_survive_a_save_and_reload() {
        let path = scratch("roundtrip");
        let config = Config {
            language: "de".into(),
            translate: true,
            threads: 6,
            copy_to_clipboard: false,
            live_preview: false,
            history_limit: 42,
            ..Default::default()
        };
        config.save_to(&path).unwrap();

        let reloaded = Config::load_from(&path).unwrap();
        assert_eq!(reloaded.language, "de");
        assert!(reloaded.translate);
        assert_eq!(reloaded.threads, 6);
        assert!(!reloaded.copy_to_clipboard);
        assert!(!reloaded.live_preview);
        assert_eq!(reloaded.history_limit, 42);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn a_missing_file_is_written_with_the_defaults() {
        let path = scratch("firstrun");
        std::fs::remove_file(&path).ok();
        let config = Config::load_from(&path).unwrap();
        assert!(path.exists(), "first run should write the file");
        assert_eq!(config.language, "auto");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn an_older_file_keeps_its_settings_and_gains_the_new_ones() {
        // Fields yapper no longer knows about are ignored, and ones it has
        // since gained fall back to their defaults rather than failing to load.
        let path = scratch("older");
        std::fs::write(
            &path,
            "language = \"fr\"\ntranslate = true\nappend_transcripts = true\n",
        )
        .unwrap();
        let config = Config::load_from(&path).unwrap();
        assert_eq!(config.language, "fr");
        assert!(config.translate);
        assert_eq!(config.history_limit, Config::default().history_limit);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn a_blank_prompt_is_no_prompt() {
        let mut config = Config::default();
        assert_eq!(config.prompt(), None);
        config.initial_prompt = "   \n ".into();
        assert_eq!(config.prompt(), None, "whitespace is not a prompt");
        config.initial_prompt = "  Hyprland, libadwaita  ".into();
        assert_eq!(config.prompt(), Some("Hyprland, libadwaita"));
    }

    #[test]
    fn auto_and_empty_both_mean_detect_the_language() {
        let mut config = Config::default();
        assert_eq!(config.language_code(), None);
        config.language = String::new();
        assert_eq!(config.language_code(), None);
        config.language = "en".into();
        assert_eq!(config.language_code(), Some("en"));
    }
}
