//! The speech models yapper knows about, and getting them onto the disk.
//!
//! All of them run through sherpa-onnx, on the CPU, entirely on this machine:
//! NVIDIA's Canary and Parakeet, Moonshine, SenseVoice. They are small, they
//! are quick, and they punctuate as they go. The network is touched to fetch
//! one and never again.
//!
//! Each model lives in a directory of its own under `models_dir`, named after
//! its id, holding the files the engine needs under the names it expects.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};

use crate::config::models_dir;

/// Which family a model belongs to. They all decode the same way; what differs
/// is the files they want handed over, and what they can be told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// NVIDIA Canary: an encoder-decoder that also translates.
    Canary,
    /// A transducer in three parts, which is how NVIDIA's Parakeet ships.
    Transducer,
    /// Moonshine, whose decoder comes in a cached and an uncached half.
    Moonshine,
    /// SenseVoice: one file, five languages.
    SenseVoice,
}

impl Engine {
    /// The languages this family can be told to listen for, as a code and the
    /// word for it. Empty when the model has no say in the matter: Parakeet
    /// works it out for itself and Moonshine only knows English.
    ///
    /// These sets are not advice. Canary silently hears English if it is asked
    /// for a language it was not trained on, and SenseVoice refuses to load at
    /// all, so a language only ever reaches a model through
    /// [`clamp_language`].
    pub fn languages(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Engine::Canary => &[
                ("en", "English"),
                ("de", "German"),
                ("es", "Spanish"),
                ("fr", "French"),
            ],
            Engine::SenseVoice => &[
                ("auto", "Detect automatically"),
                ("zh", "Chinese"),
                ("en", "English"),
                ("ja", "Japanese"),
                ("ko", "Korean"),
                ("yue", "Cantonese"),
            ],
            Engine::Transducer | Engine::Moonshine => &[],
        }
    }

    /// Whether naming a language does anything at all.
    pub fn takes_language(self) -> bool {
        !self.languages().is_empty()
    }

    /// Whether it can write a language other than the one it hears — which is
    /// what translation is, and only Canary does it.
    pub fn can_translate(self) -> bool {
        self == Engine::Canary
    }

    /// What to fall back on: the one language Canary is happiest in, and for
    /// SenseVoice the option of not choosing.
    fn default_language(self) -> &'static str {
        match self {
            Engine::SenseVoice => "auto",
            _ => "en",
        }
    }

    /// The word for a language code, for a line of prose about a model.
    pub fn language_name(self, code: &str) -> &'static str {
        self.languages()
            .iter()
            .find(|(known, _)| *known == code)
            .map(|(_, name)| *name)
            .unwrap_or("English")
    }
}

/// The nearest language an engine will actually accept.
///
/// Every language reaches an engine through here. Canary would otherwise
/// transcribe English without saying it had ignored you, and SenseVoice
/// rejects anything outside its six, which stops the model loading — so a
/// setting left over from another model must never travel unchecked.
pub fn clamp_language(engine: Engine, wanted: &str) -> &'static str {
    engine
        .languages()
        .iter()
        .map(|(code, _)| *code)
        .find(|code| *code == wanted)
        .unwrap_or_else(|| engine.default_language())
}

/// One model in the catalogue: what it is, and where its files come from.
#[derive(Debug)]
pub struct Model {
    /// Stable name for the config file, and the directory the files live in.
    pub id: &'static str,
    pub name: &'static str,
    pub maker: &'static str,
    /// Spelled out for a human, not as codes.
    pub languages: &'static str,
    /// The one thing worth knowing before choosing it.
    pub note: &'static str,
    /// Total download, for the size in the list and the progress bar.
    pub bytes: u64,
    pub engine: Engine,
    /// Everything the engine needs, fetched in this order from `base`. The
    /// names double as the file names on the disk.
    pub files: &'static [&'static str],
    pub base: &'static str,
}

/// What the picker offers.
///
/// The list is deliberately short. Every model here earns its place by being
/// the best at something — fastest, smallest, most languages — rather than by
/// existing. Each is mirrored as a directory of plain files, which needs no
/// unpacking step and lets a cancelled download resume a file at a time.
pub const CATALOGUE: &[Model] = &[
    Model {
        id: "canary-180m-flash",
        name: "Canary 180M flash",
        maker: "NVIDIA",
        languages: "English, German, Spanish, French",
        note: "Punctuation and capitals for free, and a sentence back in a sixth of a second",
        bytes: 207_170_046,
        engine: Engine::Canary,
        files: &["encoder.int8.onnx", "decoder.int8.onnx", "tokens.txt"],
        base: "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-canary-180m-flash-en-es-de-fr-int8/resolve/main",
    },
    Model {
        id: "parakeet-tdt-0.6b-v3",
        name: "Parakeet TDT 0.6B v3",
        maker: "NVIDIA",
        languages: "25 European languages, detected as it listens",
        note: "The most accurate here, and the one to reach for if you dictate in more than one language",
        bytes: 670_478_772,
        engine: Engine::Transducer,
        files: &[
            "encoder.int8.onnx",
            "decoder.int8.onnx",
            "joiner.int8.onnx",
            "tokens.txt",
        ],
        base: "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/resolve/main",
    },
    Model {
        id: "moonshine-base-en",
        name: "Moonshine base",
        maker: "Useful Sensors",
        languages: "English",
        note: "Built for short dictation, and faster than anything else here at its accuracy",
        bytes: 286_929_760,
        engine: Engine::Moonshine,
        files: &[
            "preprocess.onnx",
            "encode.int8.onnx",
            "uncached_decode.int8.onnx",
            "cached_decode.int8.onnx",
            "tokens.txt",
        ],
        base: "https://huggingface.co/csukuangfj/sherpa-onnx-moonshine-base-en-int8/resolve/main",
    },
    Model {
        id: "moonshine-tiny-en",
        name: "Moonshine tiny",
        maker: "Useful Sensors",
        languages: "English",
        note: "The smallest and fastest of the lot — a sentence comes back in a few hundredths of a second",
        bytes: 123_967_539,
        engine: Engine::Moonshine,
        files: &[
            "preprocess.onnx",
            "encode.int8.onnx",
            "uncached_decode.int8.onnx",
            "cached_decode.int8.onnx",
            "tokens.txt",
        ],
        base: "https://huggingface.co/csukuangfj/sherpa-onnx-moonshine-tiny-en-int8/resolve/main",
    },
    Model {
        id: "sense-voice",
        name: "SenseVoice small",
        maker: "FunAudioLLM",
        languages: "Chinese, English, Japanese, Korean, Cantonese",
        note: "The pick for East Asian languages, and it writes numbers as digits",
        bytes: 239_549_735,
        engine: Engine::SenseVoice,
        files: &["model.int8.onnx", "tokens.txt"],
        base: "https://huggingface.co/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/resolve/main",
    },
];

/// What a fresh install is pointed at: small, fast, punctuated.
pub const DEFAULT: &str = "canary-180m-flash";

/// The model a `model` setting names, or `None` if nothing in the catalogue
/// answers to it.
pub fn find(id: &str) -> Option<&'static Model> {
    CATALOGUE.iter().find(|model| model.id == id)
}

// --- what is on the disk -----------------------------------------------------

/// The directory a model's files live in.
pub fn dir(model: &Model) -> PathBuf {
    models_dir().join(model.id)
}

/// Where one of a model's files is, if it is there at all.
///
/// Only its own directory is consulted. The families share file names — three
/// of them want an `encoder.int8.onnx` — so a file belonging to one model must
/// never stand in for another's.
pub fn file(model: &Model, name: &str) -> Option<PathBuf> {
    let path = dir(model).join(name);
    is_present(&path).then_some(path)
}

/// A file that exists and has something in it. A zero-length file is what a
/// download killed at the wrong moment leaves behind.
fn is_present(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() > 0)
}

pub fn is_installed(model: &Model) -> bool {
    model.files.iter().all(|name| file(model, name).is_some())
}

/// Delete a downloaded model.
pub fn remove(model: &Model) -> Result<()> {
    let dir = dir(model);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    Ok(())
}

/// How much room a model takes up, for the line under its name once it is
/// installed.
pub fn size_on_disk(model: &Model) -> u64 {
    model
        .files
        .iter()
        .filter_map(|name| file(model, name))
        .filter_map(|path| std::fs::metadata(path).ok())
        .map(|meta| meta.len())
        .sum()
}

// --- downloading -------------------------------------------------------------

/// How a download ended.
pub enum Outcome {
    Done,
    /// The user pressed cancel; nothing to report.
    Cancelled,
    Failed(String),
}

/// A download in progress. Dropping it leaves the download running; `cancel`
/// is what stops it.
pub struct Download {
    model: &'static Model,
    staging: PathBuf,
    cancelled: Arc<AtomicBool>,
    /// The curl fetching the current file, so cancelling can stop it mid-file
    /// rather than after it. Held rather than tracked by pid: killing a pid
    /// that has already been reaped could signal an unrelated process.
    child: Arc<Mutex<Option<Child>>>,
    /// Resolves exactly once, when the download stops for any reason.
    pub finished: async_channel::Receiver<Outcome>,
}

/// Start fetching a model in the background.
///
/// Files are fetched with curl rather than by linking an HTTP client: it
/// handles the redirect to the CDN and the retries, and it keeps TLS out of
/// this program.
pub fn fetch(model: &'static Model) -> Result<Download> {
    if gtk::glib::find_program_in_path("curl").is_none() {
        return Err(anyhow!(
            "curl is not installed, and it is what yapper downloads models with"
        ));
    }

    // Hidden, and beside the eventual directory so finishing is a rename on the
    // same filesystem rather than a copy.
    let staging = models_dir().join(format!(".{}.part", model.id));
    std::fs::create_dir_all(&staging).with_context(|| format!("making {}", staging.display()))?;

    let cancelled = Arc::new(AtomicBool::new(false));
    let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
    let (report, finished) = async_channel::bounded(1);

    std::thread::Builder::new()
        .name(format!("download {}", model.id))
        .spawn({
            let staging = staging.clone();
            let cancelled = Arc::clone(&cancelled);
            let child = Arc::clone(&child);
            move || {
                let outcome = run_download(model, &staging, &cancelled, &child);
                // A cancelled or failed download leaves nothing behind to be
                // mistaken for a usable model.
                if !matches!(outcome, Outcome::Done) {
                    let _ = std::fs::remove_dir_all(&staging);
                }
                let _ = report.send_blocking(outcome);
            }
        })
        .context("spawning the download thread")?;

    Ok(Download {
        model,
        staging,
        cancelled,
        child,
        finished,
    })
}

fn run_download(
    model: &Model,
    staging: &Path,
    cancelled: &AtomicBool,
    child: &Mutex<Option<Child>>,
) -> Outcome {
    for name in model.files {
        if cancelled.load(Ordering::Relaxed) {
            return Outcome::Cancelled;
        }
        let url = format!("{}/{name}", model.base);
        match curl(&url, &staging.join(name), child) {
            Ok(true) => {}
            Ok(false) => return Outcome::Cancelled,
            Err(err) => return Outcome::Failed(format!("{err:#}")),
        }
    }

    let destination = dir(model);
    // A re-download replaces what was there; the files have all arrived by now,
    // so the window where neither copy exists is a rename wide.
    if destination.exists() && std::fs::remove_dir_all(&destination).is_err() {
        return Outcome::Failed(format!("could not replace {}", destination.display()));
    }
    if let Err(err) = std::fs::rename(staging, &destination) {
        return Outcome::Failed(format!(
            "could not install {}: {err}",
            destination.display()
        ));
    }
    Outcome::Done
}

/// Fetch one file. `false` means it was cancelled rather than that it failed.
fn curl(url: &str, destination: &Path, child: &Mutex<Option<Child>>) -> Result<bool> {
    let mut command = Command::new("curl");
    command
        .arg("--fail")
        .arg("--location")
        .arg("--retry")
        .arg("3")
        .arg("--retry-delay")
        .arg("2")
        .arg("--connect-timeout")
        .arg("20")
        // Quiet, but still says what went wrong on stderr.
        .arg("--silent")
        .arg("--show-error")
        // So a retry of a half-transferred file carries on rather than
        // starting the 652 MB encoder again.
        .arg("--continue-at")
        .arg("-")
        .arg("--output")
        .arg(destination)
        .arg(url);

    *child.lock().unwrap() = Some(command.spawn().context("running curl")?);

    // Polled rather than waited on, because `cancel` needs the same handle to
    // kill and reap the process, and holding it across a blocking wait would
    // shut cancelling out until the download finished on its own.
    let status = loop {
        {
            let mut held = child.lock().unwrap();
            let Some(process) = held.as_mut() else {
                // Cancelled: the handle was taken, killed and reaped for us.
                return Ok(false);
            };
            if let Some(status) = process.try_wait().context("waiting for curl")? {
                *held = None;
                break status;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };

    if status.success() {
        return Ok(true);
    }
    match status.code() {
        // Being killed is how cancelling gets curl to stop, so it is not an
        // error — but by then the handle is usually gone and we never get here.
        None => Ok(false),
        Some(code) => Err(anyhow!("curl could not fetch {url}: exit status {code}")),
    }
}

impl Download {
    /// Bytes on the disk so far, counted from the staging directory. Reading
    /// the sizes back is simpler than parsing curl's progress, and it stays
    /// right when a file is resumed.
    pub fn bytes(&self) -> u64 {
        let Ok(entries) = std::fs::read_dir(&self.staging) else {
            return 0;
        };
        entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.metadata().ok())
            .map(|meta| meta.len())
            .sum()
    }

    /// How far along, for the progress bar. Clamped, because the recorded total
    /// is what the catalogue says rather than what the server sends.
    pub fn fraction(&self) -> f64 {
        if self.model.bytes == 0 {
            return 0.0;
        }
        (self.bytes() as f64 / self.model.bytes as f64).clamp(0.0, 1.0)
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        // Killed and reaped here, under the lock, so the thread doing the
        // fetching finds the handle gone and stops.
        if let Some(mut process) = self.child.lock().unwrap().take() {
            let _ = process.kill();
            let _ = process.wait();
        }
    }
}

/// A size as a person would say it. Model sizes are the sort of number where
/// one decimal place is the difference between useful and noise.
pub fn human_size(bytes: u64) -> String {
    const MB: f64 = 1_000_000.0;
    const GB: f64 = 1_000_000_000.0;
    let bytes = bytes as f64;
    if bytes >= GB {
        format!("{:.1} GB", bytes / GB)
    } else {
        format!("{:.0} MB", bytes / MB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogue_is_coherent() {
        for model in CATALOGUE {
            assert!(!model.files.is_empty(), "{} lists no files", model.id);
            assert!(model.bytes > 0, "{} has no size", model.id);
            assert!(
                model.files.contains(&"tokens.txt"),
                "{} has no tokens file, which every engine needs",
                model.id
            );
            assert!(
                !model.id.contains('/') && !model.id.starts_with('.'),
                "{} would not make a sane directory name",
                model.id
            );
            assert!(
                model.base.starts_with("https://"),
                "{} is not fetched over https",
                model.id
            );
        }

        let mut ids: Vec<&str> = CATALOGUE.iter().map(|model| model.id).collect();
        ids.sort();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "two models share an id");

        assert!(
            find(DEFAULT).is_some(),
            "the default is not in the catalogue"
        );
    }

    #[test]
    fn a_language_is_clamped_to_what_the_engine_accepts() {
        // Canary's four, and anything else becomes the one it is best at.
        assert_eq!(clamp_language(Engine::Canary, "de"), "de");
        assert_eq!(clamp_language(Engine::Canary, "fr"), "fr");
        assert_eq!(clamp_language(Engine::Canary, "cy"), "en");
        // Canary cannot detect, so "auto" is not one of its answers.
        assert_eq!(clamp_language(Engine::Canary, "auto"), "en");

        // SenseVoice rejects an unknown language outright rather than ignoring
        // it, so this is what keeps a German setting from stopping it loading.
        assert_eq!(clamp_language(Engine::SenseVoice, "de"), "auto");
        assert_eq!(clamp_language(Engine::SenseVoice, "yue"), "yue");
        assert_eq!(clamp_language(Engine::SenseVoice, ""), "auto");

        // The ones with nothing to choose from still answer with something the
        // rest of the code can carry around.
        assert_eq!(clamp_language(Engine::Transducer, "de"), "en");
        assert_eq!(clamp_language(Engine::Moonshine, "de"), "en");
    }

    #[test]
    fn every_language_a_row_offers_survives_the_clamp() {
        // The picker builds its dropdowns from these lists, so every entry has
        // to be one the engine will take.
        for engine in [
            Engine::Canary,
            Engine::Transducer,
            Engine::Moonshine,
            Engine::SenseVoice,
        ] {
            for (code, name) in engine.languages() {
                assert_eq!(clamp_language(engine, code), *code, "{name} was rejected");
                assert_eq!(engine.language_name(code), *name);
            }
        }
    }

    #[test]
    fn every_engine_knows_what_it_can_do() {
        let canary = find("canary-180m-flash").unwrap().engine;
        assert!(canary.takes_language(), "canary has to be told a language");
        assert!(canary.can_translate(), "canary is the only translator");

        let parakeet = find("parakeet-tdt-0.6b-v3").unwrap().engine;
        assert!(!parakeet.takes_language(), "parakeet detects the language");
        assert!(!parakeet.can_translate());

        let moonshine = find("moonshine-tiny-en").unwrap().engine;
        assert!(!moonshine.takes_language(), "moonshine only knows English");

        let sense_voice = find("sense-voice").unwrap().engine;
        assert!(sense_voice.takes_language());
        assert!(!sense_voice.can_translate());
        assert!(
            sense_voice
                .languages()
                .iter()
                .any(|(code, _)| *code == "auto"),
            "sense voice can be left to detect"
        );
        assert!(
            !canary.languages().iter().any(|(code, _)| *code == "auto"),
            "canary cannot detect, so it must not be offered the choice"
        );
    }

    #[test]
    fn a_setting_names_a_model_or_nothing() {
        assert_eq!(find(DEFAULT).unwrap().engine, Engine::Canary);
        assert!(find("no-such-model").is_none());
        assert!(find("").is_none());
        // A path was how the setting read while ggml models were supported. It
        // names nothing now, and the window offers the list instead.
        assert!(find("/models/ggml-medium.en-q8_0.bin").is_none());
    }

    /// The network tests both download into the models directory and both pick
    /// the same spare model, so they take turns rather than tripping over each
    /// other's staging directory.
    static DOWNLOADING: Mutex<()> = Mutex::new(());

    /// The smallest model this machine has not already got, so a network test
    /// neither deletes something you are using nor downloads more than it has
    /// to. `None` when everything is installed, which is a skip rather than a
    /// failure.
    fn spare_model() -> Option<&'static Model> {
        CATALOGUE
            .iter()
            .filter(|model| !is_installed(model))
            .min_by_key(|model| model.bytes)
    }

    /// The downloader end to end. It needs the network and fetches a model, so
    /// it runs on request:
    ///   cargo test -- --ignored fetches_a_model
    #[test]
    #[ignore]
    fn fetches_a_model_and_deletes_it_again() {
        let _turn = DOWNLOADING.lock().unwrap();
        let Some(model) = spare_model() else {
            println!("every model is installed; nothing safe to fetch");
            return;
        };
        println!("fetching {} ({})", model.name, human_size(model.bytes));

        let download = fetch(model).expect("starting the download");
        let outcome = download.finished.recv_blocking().expect("an outcome");
        assert!(matches!(outcome, Outcome::Done), "the download failed");

        assert!(is_installed(model), "the files are not where they belong");
        assert_eq!(
            size_on_disk(model),
            model.bytes,
            "the catalogue's size is not what arrived"
        );
        // Nothing is left in the staging directory it was assembled in.
        assert!(!models_dir().join(format!(".{}.part", model.id)).exists());

        remove(model).expect("removing it");
        assert!(!is_installed(model), "the files outlived the delete");
    }

    /// Cancelling has to stop curl mid-file and leave nothing half-installed.
    /// Needs the network, and fetches only the few megabytes that arrive before
    /// it is stopped:
    ///   cargo test -- --ignored cancelling
    #[test]
    #[ignore]
    fn cancelling_a_download_leaves_nothing_behind() {
        let _turn = DOWNLOADING.lock().unwrap();
        let Some(model) = spare_model() else {
            println!("every model is installed; nothing safe to fetch");
            return;
        };

        let download = fetch(model).expect("starting the download");
        std::thread::sleep(std::time::Duration::from_millis(600));
        download.cancel();

        let outcome = download.finished.recv_blocking().expect("an outcome");
        assert!(
            matches!(outcome, Outcome::Cancelled),
            "a cancelled download should say so rather than fail"
        );
        assert!(
            !is_installed(model),
            "a stopped download must not look usable"
        );
        assert!(
            !models_dir().join(format!(".{}.part", model.id)).exists(),
            "the staging directory outlived the download"
        );
        assert!(
            !dir(model).exists(),
            "an empty model directory was left behind"
        );
    }

    #[test]
    fn sizes_read_the_way_they_are_spoken() {
        assert_eq!(human_size(207_170_046), "207 MB");
        assert_eq!(human_size(123_967_539), "124 MB");
        assert_eq!(human_size(1_625_000_000), "1.6 GB");
    }
}
