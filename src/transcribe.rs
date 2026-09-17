//! Inference on a dedicated worker thread.
//!
//! Loading a model takes a moment and holding it costs memory, so it lives on
//! its own thread for the life of the process. The UI talks to it over async
//! channels that GTK's main loop can await without blocking.
//!
//! sherpa-onnx bakes the language and the thread count into a recognizer when
//! it is built, so changing one reloads the model — under a second, and only
//! when the setting actually moves.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use sherpa_onnx::*;

use crate::audio::rms;
use crate::config::Config;
use crate::models;

/// Work sent from the UI to the worker.
pub enum Request {
    /// A running transcript of speech so far, while the user is still talking.
    /// Disposable: accuracy matters less than keeping up.
    Preview(Vec<f32>),
    /// The real thing, once recording has stopped.
    Transcribe(Vec<f32>),
}

/// Progress reported back to the UI.
pub enum Event {
    ModelReady,
    ModelFailed(String),
    /// A live transcript, superseded by the next one and finally by `Done`.
    Preview(String),
    Done(String),
    Failed(String),
}

/// The settings a running worker consults for each job. Held behind a lock so
/// preferences take effect on the next transcription rather than on restart.
#[derive(Clone)]
pub struct Settings {
    pub silence_threshold: f32,
    /// The language the chosen model listens for, and the one it writes.
    /// Already clamped to codes its engine accepts, so nothing the file says
    /// can stop a model loading.
    pub hears: String,
    pub writes: String,
    pub threads: i32,
}

impl Settings {
    pub fn from_config(config: &Config) -> Self {
        let (hears, writes) = match config.model() {
            Some(model) => config.language(model),
            // No model, so nothing will be asked of these.
            None => ("en", "en"),
        };
        Self {
            silence_threshold: config.silence_threshold,
            hears: hears.to_string(),
            writes: writes.to_string(),
            threads: config.thread_count(),
        }
    }
}

pub struct Worker {
    pub requests: async_channel::Sender<Request>,
    pub events: async_channel::Receiver<Event>,
    settings: Arc<Mutex<Settings>>,
    /// Raised when the real transcription is queued, so a preview waiting its
    /// turn is dropped rather than delaying the text that counts.
    interrupt: Arc<AtomicBool>,
}

impl Worker {
    /// Apply changed preferences. The worker reloads the model if one of the
    /// settings baked into it moved.
    pub fn apply(&self, settings: Settings) {
        *self.settings.lock().unwrap() = settings;
    }

    /// Send the final audio, cutting short any preview in flight.
    pub fn transcribe(&self, samples: Vec<f32>) -> Result<()> {
        self.interrupt.store(true, Ordering::Relaxed);
        self.requests
            .send_blocking(Request::Transcribe(samples))
            .map_err(|_| anyhow!("the transcription worker stopped"))
    }

    /// Send a preview. Dropped silently if the worker is busy or gone — a
    /// missed preview costs nothing.
    pub fn preview(&self, samples: Vec<f32>) {
        let _ = self.requests.try_send(Request::Preview(samples));
    }
}

/// Start the worker. Returns immediately; the model loads in the background and
/// announces itself with `Event::ModelReady`.
pub fn spawn(config: &Config) -> Worker {
    let (request_tx, request_rx) = async_channel::unbounded::<Request>();
    let (event_tx, event_rx) = async_channel::unbounded::<Event>();
    let interrupt = Arc::new(AtomicBool::new(false));
    let worker_interrupt = Arc::clone(&interrupt);
    let settings = Arc::new(Mutex::new(Settings::from_config(config)));
    let worker_settings = Arc::clone(&settings);

    let chosen = config.model();
    let setting = config.model.clone();

    std::thread::Builder::new()
        .name("transcribe".into())
        .spawn(move || {
            let mut model = {
                let settings = worker_settings.lock().unwrap().clone();
                match chosen
                    .ok_or_else(|| missing_model(&setting))
                    .and_then(|model| Loaded::load(model, &settings))
                {
                    Ok(model) => {
                        let _ = event_tx.send_blocking(Event::ModelReady);
                        model
                    }
                    Err(err) => {
                        let _ = event_tx.send_blocking(Event::ModelFailed(format!("{err:#}")));
                        return;
                    }
                }
            };

            while let Ok(request) = request_rx.recv_blocking() {
                let settings = worker_settings.lock().unwrap().clone();
                match request {
                    Request::Preview(samples) => {
                        // Stale the moment the user stops talking. These models
                        // decode a whole utterance at once and cannot be
                        // stopped part way, so the check happens before the
                        // work rather than during it.
                        if worker_interrupt.load(Ordering::Relaxed) {
                            continue;
                        }
                        // A preview is not worth reloading a model for; it runs
                        // with whatever is already in memory.
                        let text = model.run(&samples, &settings).unwrap_or_default();
                        // Always answer unless we were cut short, so the UI
                        // knows the slot is free again even after a failure.
                        if !worker_interrupt.load(Ordering::Relaxed) {
                            let _ = event_tx.send_blocking(Event::Preview(text));
                        }
                    }
                    Request::Transcribe(samples) => {
                        worker_interrupt.store(false, Ordering::Relaxed);
                        // A language the loaded model was not built for. The old
                        // recognizer stays if this fails, so a typo in the
                        // language box costs a transcription rather than the
                        // session.
                        let event = match model.refresh(&settings) {
                            Err(err) => Event::Failed(format!("{err:#}")),
                            Ok(()) => match model.run(&samples, &settings) {
                                Ok(text) => Event::Done(text),
                                Err(err) => Event::Failed(format!("{err:#}")),
                            },
                        };
                        let _ = event_tx.send_blocking(event);
                    }
                }
            }
        })
        .expect("spawning the transcription worker thread");

    Worker {
        requests: request_tx,
        events: event_rx,
        settings,
        interrupt,
    }
}

fn missing_model(setting: &str) -> anyhow::Error {
    if setting.is_empty() {
        anyhow!("no model chosen yet. Pick one and yapper will download it.")
    } else {
        anyhow!("no model called {setting}. Pick one and yapper will download it.")
    }
}

/// A loaded recognizer, and the settings it was built for.
struct Loaded {
    model: &'static models::Model,
    recognizer: OfflineRecognizer,
    baked: Baked,
}

/// The settings a recognizer cannot be told about after it is built.
#[derive(Debug, PartialEq, Eq, Clone)]
struct Baked {
    hears: String,
    writes: String,
    threads: i32,
}

impl Baked {
    fn of(settings: &Settings) -> Self {
        Self {
            hears: settings.hears.clone(),
            writes: settings.writes.clone(),
            threads: settings.threads,
        }
    }
}

impl Loaded {
    fn load(model: &'static models::Model, settings: &Settings) -> Result<Self> {
        if !models::is_installed(model) {
            return Err(anyhow!(
                "{} is not downloaded yet. Choose it in Preferences and yapper will fetch it.",
                model.name
            ));
        }
        Ok(Self {
            model,
            recognizer: build(model, settings)?,
            baked: Baked::of(settings),
        })
    }

    /// Rebuild if the settings have moved somewhere the loaded recognizer
    /// cannot follow.
    fn refresh(&mut self, settings: &Settings) -> Result<()> {
        let wanted = Baked::of(settings);
        if self.baked == wanted {
            return Ok(());
        }
        // Built first, so a failure leaves the working recognizer in place.
        self.recognizer = build(self.model, settings)?;
        self.baked = wanted;
        Ok(())
    }

    /// Transcribe a clip. The recognizers hand back the whole utterance at
    /// once, which is why a preview runs one at a time.
    fn run(&self, samples: &[f32], settings: &Settings) -> Result<String> {
        // Every model here invents speech when handed silence or a fragment, so
        // screen both out before they reach it.
        if samples.len() < crate::audio::TARGET_RATE as usize / 2
            || is_silent(samples, settings.silence_threshold)
        {
            return Ok(String::new());
        }

        let stream = self.recognizer.create_stream();
        stream.accept_waveform(crate::audio::TARGET_RATE as i32, samples);
        self.recognizer.decode(&stream);
        let text = stream
            .get_result()
            .map(|result| result.text)
            .ok_or_else(|| anyhow!("the recognizer returned nothing"))?;
        let text = text.trim();
        if is_annotation(text) {
            return Ok(String::new());
        }
        Ok(text.to_string())
    }
}

/// Build a recognizer. Each family names its files differently, which is the
/// only reason they are told apart at all: the decoding itself is one call.
fn build(model: &models::Model, settings: &Settings) -> Result<OfflineRecognizer> {
    // Every file the catalogue promised, or a clear word about which one is
    // missing rather than sherpa's own silence.
    let file = |name: &str| -> Result<String> {
        let path = models::file(model, name).ok_or_else(|| {
            anyhow!(
                "{} is missing {name}. Download it again in Preferences.",
                model.name
            )
        })?;
        path.to_str()
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("model path is not valid UTF-8"))
    };

    let mut config = OfflineRecognizerConfig::default();
    config.model_config.tokens = Some(file("tokens.txt")?);
    config.model_config.num_threads = settings.threads;

    match model.engine {
        models::Engine::Canary => {
            // Canary has to be told which language it is listening to; it does
            // not detect one. Writing a different language out is what its
            // translation is.
            config.model_config.canary = OfflineCanaryModelConfig {
                encoder: Some(file("encoder.int8.onnx")?),
                decoder: Some(file("decoder.int8.onnx")?),
                src_lang: Some(settings.hears.clone()),
                tgt_lang: Some(settings.writes.clone()),
                use_pnc: true,
            };
        }
        models::Engine::Transducer => {
            config.model_config.transducer = OfflineTransducerModelConfig {
                encoder: Some(file("encoder.int8.onnx")?),
                decoder: Some(file("decoder.int8.onnx")?),
                joiner: Some(file("joiner.int8.onnx")?),
            };
        }
        models::Engine::Moonshine => {
            config.model_config.moonshine = OfflineMoonshineModelConfig {
                preprocessor: Some(file("preprocess.onnx")?),
                encoder: Some(file("encode.int8.onnx")?),
                uncached_decoder: Some(file("uncached_decode.int8.onnx")?),
                cached_decoder: Some(file("cached_decode.int8.onnx")?),
                merged_decoder: None,
            };
        }
        models::Engine::SenseVoice => {
            config.model_config.sense_voice = OfflineSenseVoiceModelConfig {
                model: Some(file("model.int8.onnx")?),
                // SenseVoice refuses to load on a language outside its own
                // six, which is why this has been through the clamp.
                language: Some(settings.hears.clone()),
                // Numbers as digits and dates as dates, which is what you want
                // in the middle of a sentence you are dictating.
                use_itn: true,
            };
        }
    }

    OfflineRecognizer::create(&config)
        .ok_or_else(|| anyhow!("loading {} — sherpa-onnx would not start", model.name))
        .context("building the recognizer")
}

/// True when no short stretch of the clip rises above the threshold.
///
/// Measured per block rather than across the whole recording, because the two
/// give wildly different numbers for the same audio: a sentence surrounded by
/// pauses averages out low, so a whole-clip average would throw away short or
/// quiet utterances as the threshold rises. The live detector works in blocks,
/// and one setting should mean one thing in both places.
fn is_silent(samples: &[f32], threshold: f32) -> bool {
    !samples
        .chunks(SILENCE_BLOCK)
        .any(|block| rms(block) >= threshold)
}

/// 30ms at 16 kHz: long enough to be a stable measure, short enough that one
/// word registers.
const SILENCE_BLOCK: usize = 480;

/// Models narrate non-speech as `[BLANK_AUDIO]`, `(music)`, `*sighs*` and
/// friends. Nobody wants that pasted into their editor.
fn is_annotation(segment: &str) -> bool {
    if segment.len() < 2 {
        return false;
    }
    let wrapped = [('[', ']'), ('(', ')'), ('*', '*'), ('{', '}')]
        .iter()
        .any(|(open, close)| segment.starts_with(*open) && segment.ends_with(*close));
    // Only a whole-segment annotation counts; real speech can contain brackets.
    wrapped && !segment[1..segment.len() - 1].contains([']', ')'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annotations_are_dropped_but_speech_is_kept() {
        assert!(is_annotation("[BLANK_AUDIO]"));
        assert!(is_annotation("(soft music)"));
        assert!(is_annotation("*coughs*"));
        assert!(!is_annotation("the array [1, 2] is sorted"));
        assert!(!is_annotation("hello"));
        assert!(!is_annotation("*"));
        assert!(!is_annotation(""));
        assert!(!is_annotation("(one) and (two)"));
    }

    #[test]
    fn silence_is_detected_but_speech_is_not() {
        let t = crate::audio::DEFAULT_SILENCE_RMS;
        assert!(is_silent(&vec![0.0; 16_000], t));
        assert!(is_silent(&[0.001, -0.002, 0.0015], t));
        let speech: Vec<f32> = (0..16_000).map(|i| (i as f32 * 0.05).sin() * 0.2).collect();
        assert!(!is_silent(&speech, t));
        // A threshold above the speech itself reclassifies it.
        assert!(is_silent(&speech, 0.5));
    }

    #[test]
    fn a_short_utterance_is_not_drowned_by_the_pauses_around_it() {
        // Half a second of speech in six seconds of room tone. Averaged over
        // the whole clip this reads as silence; measured in blocks it does not.
        let quiet = vec![0.0002f32; 16_000 * 3];
        let speech: Vec<f32> = (0..8_000).map(|i| (i as f32 * 0.05).sin() * 0.02).collect();
        let clip: Vec<f32> = quiet
            .iter()
            .chain(speech.iter())
            .chain(quiet.iter())
            .copied()
            .collect();

        let whole_clip = rms(&clip);
        assert!(whole_clip < 0.008, "whole-clip average is {whole_clip}");
        assert!(
            !is_silent(&clip, 0.008),
            "the utterance must survive a threshold its clip average falls under"
        );
    }

    /// Reloading is what makes a language change land, so the comparison that
    /// decides it has to notice each setting that is baked in.
    #[test]
    fn a_reload_is_triggered_by_the_settings_that_are_baked_in() {
        let base = Settings {
            silence_threshold: 0.01,
            hears: "en".into(),
            writes: "en".into(),
            threads: 4,
        };
        assert_eq!(Baked::of(&base), Baked::of(&base.clone()));

        // German in, German out.
        let german = Settings {
            hears: "de".into(),
            writes: "de".into(),
            ..base.clone()
        };
        assert_ne!(Baked::of(&base), Baked::of(&german));

        // German in, English out: the same language heard, a different one
        // written, and a recognizer that has to be rebuilt to do it.
        let translating = Settings {
            hears: "de".into(),
            writes: "en".into(),
            ..base.clone()
        };
        assert_ne!(Baked::of(&german), Baked::of(&translating));

        let busier = Settings {
            threads: 8,
            ..base.clone()
        };
        assert_ne!(Baked::of(&base), Baked::of(&busier));

        // The threshold is read per job, so it must not cost a reload.
        let fussier = Settings {
            silence_threshold: 0.05,
            ..base.clone()
        };
        assert_eq!(Baked::of(&base), Baked::of(&fussier));
    }

    /// End to end against a known clip, with whichever model the config points
    /// at. Needs that model downloaded, so it runs on request:
    ///   YAPPER_TEST_WAV=samples/jfk.wav cargo test --release -- --ignored sample_wav
    #[test]
    #[ignore]
    fn transcribes_a_sample_wav() {
        let config = Config::load().expect("loading config");
        let samples =
            read_pcm16_wav(&std::env::var("YAPPER_TEST_WAV").expect("set YAPPER_TEST_WAV"));
        let settings = Settings::from_config(&config);
        let model = Loaded::load(config.model().expect("a model"), &settings).expect("loading it");
        let text = model.run(&samples, &settings).expect("transcribing");
        println!("transcript: {text}");
        assert!(!text.is_empty());
    }

    /// Every model in the catalogue that is downloaded, against the same clip.
    /// The one test that proves each family is wired to the right files:
    ///   YAPPER_TEST_WAV=samples/jfk.wav cargo test --release -- --ignored every_installed
    #[test]
    #[ignore]
    fn every_installed_model_transcribes() {
        let config = Config::load().expect("loading config");
        let settings = Settings::from_config(&config);
        let samples =
            read_pcm16_wav(&std::env::var("YAPPER_TEST_WAV").expect("set YAPPER_TEST_WAV"));

        let mut tried = 0;
        for model in models::CATALOGUE {
            if !models::is_installed(model) {
                println!("  {} — not downloaded, skipped", model.name);
                continue;
            }
            let loaded = Loaded::load(model, &settings).expect("loading it");
            let started = std::time::Instant::now();
            let text = loaded.run(&samples, &settings).expect("transcribing");
            println!("  {} — {:?} — {text}", model.name, started.elapsed());
            assert!(!text.is_empty(), "{} transcribed nothing", model.name);
            tried += 1;
        }
        assert!(tried > 0, "no models are downloaded");
    }

    /// Minimal 16-bit PCM WAV reader, just enough for the test fixture.
    fn read_pcm16_wav(path: &str) -> Vec<f32> {
        let bytes = std::fs::read(path).expect("reading the wav");
        let mut offset = 12; // past "RIFF<size>WAVE"
        while offset + 8 <= bytes.len() {
            let id = &bytes[offset..offset + 4];
            let size =
                u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
            let body = offset + 8;
            if id == b"data" {
                return bytes[body..body + size]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&p| i16::from_le_bytes(p) as f32 / i16::MAX as f32)
                    .collect();
            }
            offset = body + size + (size % 2);
        }
        panic!("no data chunk in {path}");
    }
}
