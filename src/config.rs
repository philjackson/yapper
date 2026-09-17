//! User configuration, read from `~/.config/yapper/config.toml`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Which model transcribes, by catalogue id — `canary-180m-flash`, say.
    /// See [`crate::models`].
    pub model: String,
    /// What each model has been told to listen for, keyed by model id, as
    /// `hears` or `hears>writes` — `de` for German, `de>en` to hear German and
    /// write English. Per model because the codes a model accepts are its own:
    /// `de` means nothing to SenseVoice, and a single setting shared between
    /// them stopped one loading.
    #[serde(default)]
    pub languages: BTreeMap<String, String>,
    /// The language as it was written before models carried their own. Read
    /// once to seed `languages`, then dropped from the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// As `language`: the old switch for translating into English.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translate: Option<bool>,
    /// Inference threads; 0 picks a sensible default from the CPU count. Not
    /// in preferences: the default is right on every machine we have seen, and
    /// a wrong one is a slow transcription rather than an obvious mistake.
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
    /// Words you say and the text they become: "minus minus" for `--`, and
    /// anything else dictation is bad at saying. See [`crate::replace`].
    #[serde(default)]
    pub replacements: Vec<crate::replace::Replacement>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: crate::models::DEFAULT.to_string(),
            languages: BTreeMap::new(),
            language: None,
            translate: None,
            threads: 0,
            copy_to_clipboard: true,
            type_on_finish: false,
            history_limit: 200,
            live_preview: true,
            silence_timeout: 0.0,
            silence_threshold: crate::audio::DEFAULT_SILENCE_RMS,
            pause_players: true,
            input_device: String::new(),
            replacements: Vec::new(),
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
        let mut config: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.adopt_older_language();
        Ok(config)
    }

    /// Carry a pre-model language over, for the model that was in use when it
    /// was written. The other models keep their own answer, which is the point
    /// of the move.
    fn adopt_older_language(&mut self) {
        let (Some(language), translate) = (self.language.take(), self.translate.take()) else {
            return;
        };
        let Some(model) = self.model() else {
            return;
        };
        if self.languages.contains_key(model.id) {
            return;
        }
        let hears = crate::models::clamp_language(model.engine, &language);
        let writes = if translate.unwrap_or(false) {
            "en"
        } else {
            hears
        };
        self.set_language(model, hears, writes);
    }

    /// The model in use, or `None` when the setting names something that is
    /// not in the catalogue — a model that has been dropped, or the path a
    /// config wrote back when ggml files could be pointed at.
    pub fn model(&self) -> Option<&'static crate::models::Model> {
        crate::models::find(&self.model)
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

    /// What a model has been told: the language it listens for, and the one it
    /// writes. Both are codes the model's own engine accepts, whatever the file
    /// says, so nothing unusable reaches sherpa-onnx.
    pub fn language(&self, model: &crate::models::Model) -> (&'static str, &'static str) {
        let stored = self
            .languages
            .get(model.id)
            .map(String::as_str)
            .unwrap_or("");
        let (hears, writes) = match stored.split_once('>') {
            Some((hears, writes)) => (hears, writes),
            None => (stored, stored),
        };
        let hears = crate::models::clamp_language(model.engine, hears);
        // Only Canary can write a language it did not hear; for the rest the
        // two are the same thing by definition.
        let writes = if model.engine.can_translate() {
            crate::models::clamp_language(model.engine, writes)
        } else {
            hears
        };
        (hears, writes)
    }

    /// Remember a model's language. `hears` alone when it writes what it hears,
    /// which is all but Canary and most of Canary's use.
    pub fn set_language(&mut self, model: &crate::models::Model, hears: &str, writes: &str) {
        let value = if hears == writes {
            hears.to_string()
        } else {
            format!("{hears}>{writes}")
        };
        self.languages.insert(model.id.to_string(), value);
    }

    pub fn thread_count(&self) -> i32 {
        if self.threads > 0 {
            return self.threads as i32;
        }
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        // The models scale poorly past ~8 threads and the UI should stay
        // responsive while one runs.
        cores.saturating_sub(1).clamp(1, 8) as i32
    }
}

pub fn config_path() -> PathBuf {
    gtk::glib::user_config_dir().join("yapper/config.toml")
}

pub fn models_dir() -> PathBuf {
    gtk::glib::user_data_dir().join("yapper/models")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("yapper-config-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    fn canary() -> &'static crate::models::Model {
        crate::models::find("canary-180m-flash").unwrap()
    }

    #[test]
    fn settings_survive_a_save_and_reload() {
        let path = scratch("roundtrip");
        let mut config = Config {
            threads: 6,
            copy_to_clipboard: false,
            live_preview: false,
            history_limit: 42,
            ..Default::default()
        };
        config.set_language(canary(), "de", "en");
        config.save_to(&path).unwrap();

        let reloaded = Config::load_from(&path).unwrap();
        assert_eq!(
            reloaded.language(canary()),
            ("de", "en"),
            "hearing one language and writing another survives the file"
        );
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
        assert_eq!(config.model, crate::models::DEFAULT);
        // Nothing has been said about a language, so the default model gets the
        // one it is best at rather than an empty setting.
        assert_eq!(config.language(canary()), ("en", "en"));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn an_older_file_keeps_its_settings_and_gains_the_new_ones() {
        // Fields yapper no longer knows about are ignored, and ones it has
        // since gained fall back to their defaults rather than failing to load.
        // `model_path` and `initial_prompt` are here because a config written
        // while Whisper was supported still has them.
        let path = scratch("older");
        std::fs::write(
            &path,
            "language = \"fr\"\ntranslate = true\nappend_transcripts = true\n\
             model_path = \"/models/ggml-base.en.bin\"\ninitial_prompt = \"Pitch\"\n",
        )
        .unwrap();
        let config = Config::load_from(&path).unwrap();
        assert_eq!(config.history_limit, Config::default().history_limit);
        // The old pair of settings becomes this model's own entry: French in,
        // English out, which is what `translate` meant.
        assert_eq!(config.language(canary()), ("fr", "en"));
        assert!(config.language.is_none(), "the old field is dropped");
        assert!(config.translate.is_none());
        assert_eq!(
            config.model,
            crate::models::DEFAULT,
            "a config that named a ggml file falls back to the default model"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn a_language_a_model_cannot_use_never_reaches_it() {
        let sense_voice = crate::models::find("sense-voice").unwrap();
        let mut config = Config::default();

        // Set while Canary was in use, then read for SenseVoice, which rejects
        // anything outside its own six and would refuse to load at all.
        config.set_language(canary(), "de", "de");
        config.languages.insert("sense-voice".into(), "de".into());
        assert_eq!(config.language(sense_voice), ("auto", "auto"));
        assert_eq!(config.language(canary()), ("de", "de"));

        // Only Canary can write a language it did not hear; asking anything
        // else to is quietly the same language.
        config
            .languages
            .insert("sense-voice".into(), "zh>en".into());
        assert_eq!(config.language(sense_voice), ("zh", "zh"));

        // A model nothing has been said about, and a model with nothing to say.
        let moonshine = crate::models::find("moonshine-tiny-en").unwrap();
        assert_eq!(config.language(moonshine), ("en", "en"));
    }

    #[test]
    fn a_model_keeps_its_own_language_when_another_is_chosen() {
        let path = scratch("per-model");
        let sense_voice = crate::models::find("sense-voice").unwrap();
        let mut config = Config::default();
        config.set_language(canary(), "fr", "fr");
        config.set_language(sense_voice, "ja", "ja");
        config.save_to(&path).unwrap();

        let reloaded = Config::load_from(&path).unwrap();
        assert_eq!(reloaded.language(canary()), ("fr", "fr"));
        assert_eq!(
            reloaded.language(sense_voice),
            ("ja", "ja"),
            "choosing one model must not rewrite what another was told"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
