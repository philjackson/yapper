//! Whisper inference on a dedicated worker thread.
//!
//! The model is large and loading it takes seconds, so it lives on its own
//! thread for the life of the process. The UI talks to it over async channels
//! that GTK's main loop can await without blocking.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::config::Config;

/// Work sent from the UI to the worker.
pub enum Request {
    Transcribe(Vec<f32>),
}

/// Progress reported back to the UI.
pub enum Event {
    ModelReady,
    ModelFailed(String),
    Transcribing,
    Done(String),
    Failed(String),
}

pub struct Worker {
    pub requests: async_channel::Sender<Request>,
    pub events: async_channel::Receiver<Event>,
}

/// Start the worker. Returns immediately; the model loads in the background and
/// announces itself with `Event::ModelReady`.
pub fn spawn(config: &Config) -> Worker {
    let (request_tx, request_rx) = async_channel::unbounded::<Request>();
    let (event_tx, event_rx) = async_channel::unbounded::<Event>();

    let model_path = config.model_path.clone();
    let language = config.language_code().map(str::to_owned);
    let threads = config.thread_count();
    let translate = config.translate;

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
                let Request::Transcribe(samples) = request;
                let _ = event_tx.send_blocking(Event::Transcribing);
                let event = match run(&context, &samples, language.as_deref(), threads, translate) {
                    Ok(text) => Event::Done(text),
                    Err(err) => Event::Failed(format!("{err:#}")),
                };
                let _ = event_tx.send_blocking(event);
            }
        })
        .expect("spawning the whisper worker thread");

    Worker {
        requests: request_tx,
        events: event_rx,
    }
}

fn load_model(path: &Path) -> Result<WhisperContext> {
    if !path.exists() {
        return Err(anyhow!(
            "model not found at {}\nrun ./scripts/fetch-model.sh to download one",
            path.display()
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
    language: Option<&str>,
    threads: i32,
    translate: bool,
) -> Result<String> {
    // Whisper invents speech when handed silence or a fragment, so screen both
    // out before they reach the model.
    if samples.len() < crate::audio::TARGET_RATE as usize / 2 || is_silent(samples) {
        return Ok(String::new());
    }

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(language);
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

/// Room tone sits well below this; even quiet speech sits above it.
const SILENCE_RMS: f32 = 0.004;

fn is_silent(samples: &[f32]) -> bool {
    let sum_squares: f64 = samples.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    let rms = (sum_squares / samples.len() as f64).sqrt() as f32;
    rms < SILENCE_RMS
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
        assert!(is_silent(&vec![0.0; 16_000]));
        assert!(is_silent(&[0.001, -0.002, 0.0015]));
        let speech: Vec<f32> = (0..16_000).map(|i| (i as f32 * 0.05).sin() * 0.2).collect();
        assert!(!is_silent(&speech));
    }

    #[test]
    #[ignore]
    fn transcribes_a_sample_wav() {
        let path = std::env::var("YAPPER_TEST_WAV").expect("set YAPPER_TEST_WAV");
        let samples = read_pcm16_wav(&path);
        let config = Config::load().expect("loading config");
        let context = load_model(&config.model_path).expect("loading the model");
        let text = run(&context, &samples, Some("en"), config.thread_count(), false)
            .expect("transcribing");
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
