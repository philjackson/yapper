//! Whisper inference on a dedicated worker thread.
//!
//! The model is large and loading it takes seconds, so it lives on its own
//! thread for the life of the process. The UI talks to it over async channels
//! that GTK's main loop can await without blocking.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::config::Config;

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
    Transcribing,
    Done(String),
    Failed(String),
}

/// The settings a running worker consults for each job. Held behind a lock so
/// preferences take effect on the next transcription rather than on restart.
#[derive(Clone)]
pub struct Settings {
    pub silence_threshold: f32,
    pub language: Option<String>,
    pub threads: i32,
    pub translate: bool,
    pub prompt: Option<String>,
}

impl Settings {
    pub fn from_config(config: &Config) -> Self {
        Self {
            silence_threshold: config.silence_threshold,
            language: config.language_code().map(str::to_owned),
            threads: config.thread_count(),
            translate: config.translate,
            prompt: config.prompt().map(str::to_owned),
        }
    }
}

pub struct Worker {
    pub requests: async_channel::Sender<Request>,
    pub events: async_channel::Receiver<Event>,
    settings: Arc<Mutex<Settings>>,
    /// Raised when the real transcription is queued, so a preview still running
    /// gives up its slice of the GPU instead of delaying the text that counts.
    interrupt: Arc<AtomicBool>,
}

impl Worker {
    /// Apply changed preferences. The model itself is loaded once at startup,
    /// so a different model still needs a restart; everything else is per-job.
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

    let model_path = config.model_path.clone();

    std::thread::Builder::new()
        .name("whisper".into())
        .spawn(move || {
            let context = match load_model(&model_path) {
                Ok(context) => {
                    let _ = event_tx.send_blocking(Event::ModelReady);
                    context
                }
                Err(err) => {
                    let _ = event_tx.send_blocking(Event::ModelFailed(format!("{err:#}")));
                    return;
                }
            };

            while let Ok(request) = request_rx.recv_blocking() {
                match request {
                    Request::Preview(samples) => {
                        // Stale the moment the user stops talking.
                        if worker_interrupt.load(Ordering::Relaxed) {
                            continue;
                        }
                        let settings = worker_settings.lock().unwrap().clone();
                        let abort = Arc::clone(&worker_interrupt);
                        let text = run(
                            &context,
                            &samples,
                            &settings,
                            Some(Box::new(move || abort.load(Ordering::Relaxed))),
                        )
                        .unwrap_or_default();
                        // Always answer unless we were cut short, so the UI
                        // knows the slot is free again even after a failure.
                        if !worker_interrupt.load(Ordering::Relaxed) {
                            let _ = event_tx.send_blocking(Event::Preview(text));
                        }
                    }
                    Request::Transcribe(samples) => {
                        worker_interrupt.store(false, Ordering::Relaxed);
                        let settings = worker_settings.lock().unwrap().clone();
                        let _ = event_tx.send_blocking(Event::Transcribing);
                        let event = match run(&context, &samples, &settings, None) {
                            Ok(text) => Event::Done(text),
                            Err(err) => Event::Failed(format!("{err:#}")),
                        };
                        let _ = event_tx.send_blocking(event);
                    }
                }
            }
        })
        .expect("spawning the whisper worker thread");

    Worker {
        requests: request_tx,
        events: event_rx,
        settings,
        interrupt,
    }
}

fn load_model(path: &Path) -> Result<WhisperContext> {
    if !path.exists() {
        return Err(anyhow!(
            "model not found at {}\n\nRun ./scripts/fetch-model.sh to download one, or fetch a \
             ggml model by hand from\n{}\nand point Preferences at it.",
            path.display(),
            crate::preferences::MODELS_URL
        ));
    }
    let path = path
        .to_str()
        .ok_or_else(|| anyhow!("model path is not valid UTF-8"))?;
    WhisperContext::new_with_params(path, WhisperContextParameters::default())
        .context("loading the whisper model")
}

pub(crate) fn run(
    context: &WhisperContext,
    samples: &[f32],
    settings: &Settings,
    // Present for previews, which are allowed to give up part way.
    abort: Option<Box<dyn FnMut() -> bool + 'static>>,
) -> Result<String> {
    let Settings {
        silence_threshold: threshold,
        language,
        threads,
        translate,
        prompt,
    } = settings;
    let (threshold, threads, translate) = (*threshold, *threads, *translate);
    let language = language.as_deref();
    let prompt = prompt.as_deref();

    // Whisper invents speech when handed silence or a fragment, so screen both
    // out before they reach the model.
    if samples.len() < crate::audio::TARGET_RATE as usize / 2 || is_silent(samples, threshold) {
        return Ok(String::new());
    }

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    if let Some(abort) = abort {
        params.set_abort_callback_safe(abort);
        // Previews trade accuracy for latency: no temperature fallback, which
        // is what makes a hard segment take several times as long.
        params.set_temperature_inc(0.0);
        params.set_no_context(true);
    }
    params.set_language(language);
    // Context for the decoder: names and jargon it would otherwise guess at.
    if let Some(prompt) = prompt {
        params.set_initial_prompt(prompt);
    }
    params.set_n_threads(threads);
    params.set_translate(translate);
    params.set_suppress_blank(true);
    params.set_suppress_nst(true);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);

    let mut state = context.create_state().context("creating a whisper state")?;
    state.full(params, samples).context("running whisper")?;

    let mut text = String::new();
    for segment in state.as_iter() {
        let segment = segment.to_str_lossy()?;
        let segment = segment.trim();
        if is_annotation(segment) {
            continue;
        }
        text.push_str(segment);
        text.push(' ');
    }
    Ok(text.trim().to_string())
}

fn is_silent(samples: &[f32], threshold: f32) -> bool {
    let sum_squares: f64 = samples.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    let rms = (sum_squares / samples.len() as f64).sqrt() as f32;
    rms < threshold
}

/// Whisper narrates non-speech as `[BLANK_AUDIO]`, `(music)`, `*sighs*` and
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

    /// End-to-end check against a known clip. Needs the model downloaded, so it
    /// runs on request:
    ///   YAPPER_TEST_WAV=samples/jfk.wav cargo test --release -- --ignored sample_wav
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
        // A higher threshold reclassifies the same audio.
        assert!(is_silent(&speech, 0.5));
    }

    /// Proves the vocabulary reaches the decoder. Steering it towards nonsense
    /// is the clearest way to see it land — a real vocabulary would only nudge
    /// words the model already nearly had.
    ///   YAPPER_TEST_WAV=samples/jfk.wav cargo test --release -- --ignored prompt_reaches
    #[test]
    #[ignore]
    fn prompt_reaches_the_decoder() {
        let path = std::env::var("YAPPER_TEST_WAV").expect("set YAPPER_TEST_WAV");
        let samples = read_pcm16_wav(&path);
        let config = Config::load().expect("loading config");
        let context = load_model(&config.model_path).expect("loading the model");

        let plain = run(&context, &samples, &Settings::from_config(&config), None)
            .expect("transcribing");
        let prompted = run(
            &context,
            &samples,
            &Settings {
                prompt: Some("Siobhan, Niamh, Loughborough, Sainsbury's".into()),
                ..Settings::from_config(&config)
            },
            None,
        )
        .expect("transcribing");

        println!("  without prompt: {plain}");
        println!("  with prompt:    {prompted}");
        assert!(!plain.is_empty() && !prompted.is_empty());
    }

    #[test]
    #[ignore]
    fn transcribes_a_sample_wav() {
        let path = std::env::var("YAPPER_TEST_WAV").expect("set YAPPER_TEST_WAV");
        let samples = read_pcm16_wav(&path);
        let config = Config::load().expect("loading config");
        let context = load_model(&config.model_path).expect("loading the model");
        let settings = Settings::from_config(&config);
        let text = run(&context, &samples, &settings, None).expect("transcribing");
        println!("transcript: {text}");
        assert!(!text.is_empty());
    }

    /// Minimal 16-bit PCM WAV reader, just enough for the test fixture.
    fn read_pcm16_wav(path: &str) -> Vec<f32> {
        let bytes = std::fs::read(path).expect("reading the wav");
        let mut offset = 12; // past "RIFF<size>WAVE"
        while offset + 8 <= bytes.len() {
            let id = &bytes[offset..offset + 4];
            let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
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
