//! Microphone capture via cpal (which talks to PipeWire through ALSA/Pulse).
//!
//! The models want 16 kHz mono f32, so we ask the device for that directly when it
//! can do it and resample afterwards when it can't.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};

pub const TARGET_RATE: u32 = 16_000;

/// Where the threshold starts before anyone adjusts it.
pub const DEFAULT_SILENCE_RMS: f32 = 0.004;

/// Tracks how long the microphone has been quiet, so a recording can end
/// itself. Silence before the first word does not count — otherwise a
/// recording started a moment early would stop before you spoke.
struct SilenceTracker {
    /// Per-channel frames of quiet since the last sound.
    silent_frames: usize,
    heard_speech: bool,
    threshold: f32,
}

impl SilenceTracker {
    fn new(threshold: f32) -> Self {
        Self {
            silent_frames: 0,
            heard_speech: false,
            threshold,
        }
    }

    fn observe(&mut self, rms: f32, frames: usize) {
        if rms >= self.threshold {
            self.silent_frames = 0;
            self.heard_speech = true;
        } else if self.heard_speech {
            self.silent_frames += frames;
        }
    }
}

/// Shared between the audio callback and the UI thread.
struct Capture {
    samples: Vec<f32>,
    /// Loudest sample since the UI last looked, for the level meter.
    peak: f32,
    /// Loudness of the most recent buffer, which is what the threshold is
    /// compared against and what the microphone test displays.
    rms: f32,
    silence: SilenceTracker,
}

pub struct Recorder {
    // Dropping the stream stops capture, so it has to outlive the recording.
    _stream: cpal::Stream,
    capture: Arc<Mutex<Capture>>,
    sample_rate: u32,
    channels: u16,
}

impl Recorder {
    /// Open an input device and start filling the buffer. `wanted` is a device
    /// id from [`input_devices`]; an empty or unknown one falls back to the
    /// system default, since an unplugged microphone should not stop yapper
    /// from recording at all.
    pub fn start(wanted: &str, threshold: f32) -> Result<Self> {
        Self::open(wanted, threshold, true)
    }

    /// Levels only, for the microphone test: audio is measured and thrown away
    /// rather than accumulated.
    pub fn monitor(wanted: &str, threshold: f32) -> Result<Self> {
        Self::open(wanted, threshold, false)
    }

    fn open(wanted: &str, threshold: f32, retain: bool) -> Result<Self> {
        let host = cpal::default_host();
        let device = if wanted.is_empty() {
            host.default_input_device()
        } else {
            chosen_device(&host, wanted).or_else(|| {
                eprintln!("yapper: chosen input device is not connected, using the default");
                host.default_input_device()
            })
        }
        .ok_or_else(|| anyhow!("no input device available"))?;

        let supported = pick_config(&device)?;
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();

        let capture = Arc::new(Mutex::new(Capture {
            samples: Vec::new(),
            peak: 0.0,
            rms: 0.0,
            silence: SilenceTracker::new(threshold),
        }));
        let err_fn = |err| eprintln!("yapper: audio stream error: {err}");

        let stream = match sample_format {
            SampleFormat::F32 => build_stream::<f32>(&device, &config, &capture, retain, err_fn),
            SampleFormat::I16 => build_stream::<i16>(&device, &config, &capture, retain, err_fn),
            SampleFormat::I32 => build_stream::<i32>(&device, &config, &capture, retain, err_fn),
            SampleFormat::U16 => build_stream::<u16>(&device, &config, &capture, retain, err_fn),
            SampleFormat::I8 => build_stream::<i8>(&device, &config, &capture, retain, err_fn),
            SampleFormat::U8 => build_stream::<u8>(&device, &config, &capture, retain, err_fn),
            other => Err(anyhow!("unsupported sample format {other:?}")),
        }?;

        stream.play().context("starting the input stream")?;

        Ok(Self {
            _stream: stream,
            capture,
            sample_rate: config.sample_rate,
            channels: config.channels,
        })
    }

    /// How long the microphone has been quiet, in seconds. Stays at zero until
    /// the first sound, so it cannot trip before anyone has spoken.
    pub fn silence_secs(&self) -> f32 {
        let capture = self.capture.lock().unwrap();
        capture.silence.silent_frames as f32 / self.sample_rate as f32
    }

    /// Loudness of the most recent buffer, for the microphone test.
    pub fn current_rms(&self) -> f32 {
        self.capture.lock().unwrap().rms
    }

    /// Loudest sample since the last call, as a 0.0..=1.0 level. Resets the peak.
    pub fn take_peak(&self) -> f32 {
        let mut capture = self.capture.lock().unwrap();
        std::mem::take(&mut capture.peak).clamp(0.0, 1.0)
    }

    pub fn duration_secs(&self) -> f32 {
        let capture = self.capture.lock().unwrap();
        capture.samples.len() as f32 / (self.sample_rate as f32 * self.channels as f32)
    }

    /// Everything captured so far, without interrupting the recording. Used to
    /// keep a live transcript up to date while the user is still talking.
    pub fn snapshot(&self) -> Vec<f32> {
        let raw = self.capture.lock().unwrap().samples.clone();
        self.prepare(raw)
    }

    /// Stop capturing and hand back 16 kHz mono audio ready to transcribe.
    pub fn finish(self) -> Vec<f32> {
        let raw = std::mem::take(&mut self.capture.lock().unwrap().samples);
        self.prepare(raw)
    }

    fn prepare(&self, raw: Vec<f32>) -> Vec<f32> {
        let mono = downmix(&raw, self.channels);
        resample(&mono, self.sample_rate, TARGET_RATE)
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    capture: &Arc<Mutex<Capture>>,
    retain: bool,
    err_fn: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let capture = Arc::clone(capture);
    let channels = config.channels.max(1) as usize;
    device
        .build_input_stream(
            *config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                if data.is_empty() {
                    return;
                }
                let mut sum_squares = 0.0f64;
                let mut peak = 0.0f32;
                for &sample in data {
                    let value = f32::from_sample(sample);
                    sum_squares += (value as f64) * (value as f64);
                    peak = peak.max(value.abs());
                }
                // One RMS per callback buffer — a few tens of milliseconds,
                // which is the right window for "is anyone talking".
                let rms = (sum_squares / data.len() as f64).sqrt() as f32;

                let mut capture = capture.lock().unwrap();
                if retain {
                    capture.samples.extend(data.iter().map(|&s| f32::from_sample(s)));
                }
                capture.peak = capture.peak.max(peak);
                capture.rms = rms;
                capture.silence.observe(rms, data.len() / channels);
            },
            err_fn,
            None,
        )
        .context("building the input stream")
}

/// The device a saved id names, if it is still connected.
fn chosen_device(host: &cpal::Host, id: &str) -> Option<cpal::Device> {
    match id.parse::<cpal::DeviceId>() {
        Ok(id) => host.device_by_id(&id),
        Err(err) => {
            eprintln!("yapper: cannot read input device id {id:?}: {err}");
            None
        }
    }
}

/// Root mean square of a block of samples: its loudness, as the silence
/// threshold measures it. Shared with the transcriber so one setting means
/// one thing on both sides.
pub(crate) fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_squares: f64 = samples.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    (sum_squares / samples.len() as f64).sqrt() as f32
}

/// An input device a person might actually want to choose, with the id to
/// persist and a name to show.
pub struct InputDevice {
    pub id: String,
    pub name: String,
}

/// The microphones worth offering in a picker.
///
/// ALSA advertises dozens of entries, most of them plugins ("Rate Converter",
/// "Discard all samples") or lower-level duplicates of the same card. The
/// `sysdefault:CARD=` form is the one canonical entry per piece of hardware,
/// so that is what gets listed.
pub fn input_devices() -> Vec<InputDevice> {
    let host = cpal::default_host();
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };

    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for device in devices {
        let (Ok(id), Ok(description)) = (device.id(), device.description()) else {
            continue;
        };
        let id = id.to_string();
        if !is_real_microphone(&id) {
            continue;
        }
        // Several ids can describe one microphone; one entry each is plenty.
        if seen.insert(description.name().to_string()) {
            found.push(InputDevice {
                id,
                name: description.name().to_string(),
            });
        }
    }
    found
}

/// A real capture device rather than one of ALSA's processing plugins.
fn is_real_microphone(id: &str) -> bool {
    id.contains(":sysdefault:CARD=")
}

/// Prefer a config we can use as-is: 16 kHz so there is nothing to resample,
/// mono so there is nothing to downmix, and a sample format that survives the
/// trip to f32 without losing resolution.
fn pick_config(device: &cpal::Device) -> Result<cpal::SupportedStreamConfig> {
    let mut best: Option<(u32, cpal::SupportedStreamConfigRange)> = None;

    if let Ok(configs) = device.supported_input_configs() {
        for candidate in configs {
            let Some(rank) = format_rank(candidate.sample_format()) else {
                continue;
            };
            if candidate.min_sample_rate() > TARGET_RATE
                || candidate.max_sample_rate() < TARGET_RATE
            {
                continue;
            }
            // Sample format dominates; channel count breaks ties.
            let score = rank * 100 + u32::from(candidate.channels());
            if best.as_ref().is_none_or(|(best_score, _)| score < *best_score) {
                best = Some((score, candidate));
            }
        }
    }

    if let Some((_, candidate)) = best {
        return Ok(candidate.with_sample_rate(TARGET_RATE));
    }

    // Nothing offered 16 kHz, so take whatever the device calls default and
    // resample on the way out.
    let default = device
        .default_input_config()
        .context("querying the default input config")?;
    if format_rank(default.sample_format()).is_none() {
        return Err(anyhow!(
            "input device only offers unsupported sample format {:?}",
            default.sample_format()
        ));
    }
    Ok(default)
}

/// Lower is better. `None` means we can't read that format at all.
fn format_rank(format: SampleFormat) -> Option<u32> {
    match format {
        SampleFormat::F32 => Some(0),
        SampleFormat::I16 => Some(1),
        SampleFormat::I32 => Some(2),
        SampleFormat::U16 => Some(3),
        SampleFormat::I8 => Some(4),
        SampleFormat::U8 => Some(5),
        _ => None,
    }
}

fn downmix(input: &[f32], channels: u16) -> Vec<f32> {
    if channels <= 1 {
        return input.to_vec();
    }
    let channels = channels as usize;
    input
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

/// Rate conversion good enough for speech: a box filter over the source window
/// when downsampling (which keeps aliasing in check), linear interpolation when
/// upsampling. Swap in `rubato` if transcription quality ever needs more.
fn resample(input: &[f32], in_rate: u32, out_rate: u32) -> Vec<f32> {
    if in_rate == out_rate || input.is_empty() {
        return input.to_vec();
    }

    let ratio = in_rate as f64 / out_rate as f64;
    let out_len = (input.len() as f64 / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);

    if ratio > 1.0 {
        for i in 0..out_len {
            let start = i as f64 * ratio;
            let end = start + ratio;
            let first = start.floor() as usize;
            let last = (end.ceil() as usize).min(input.len());
            let (mut acc, mut weight_sum) = (0.0f64, 0.0f64);
            for (offset, &sample) in input[first..last].iter().enumerate() {
                let index = (first + offset) as f64;
                let lo = index.max(start);
                let hi = (index + 1.0).min(end);
                let weight = (hi - lo).max(0.0);
                acc += sample as f64 * weight;
                weight_sum += weight;
            }
            out.push(if weight_sum > 0.0 {
                (acc / weight_sum) as f32
            } else {
                0.0
            });
        }
    } else {
        let last = input.len() - 1;
        for i in 0..out_len {
            let pos = i as f64 * ratio;
            let index = pos.floor() as usize;
            let frac = (pos - index as f64) as f32;
            let a = input[index.min(last)];
            let b = input[(index + 1).min(last)];
            out.push(a + (b - a) * frac);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_is_not_counted_until_something_has_been_said() {
        let mut tracker = SilenceTracker::new(DEFAULT_SILENCE_RMS);
        // A recording started a moment early must not end itself before the
        // speaker begins.
        tracker.observe(0.0, 16_000);
        tracker.observe(0.0, 16_000);
        assert_eq!(tracker.silent_frames, 0);

        tracker.observe(0.2, 1_600);
        assert!(tracker.heard_speech);
        tracker.observe(0.0, 8_000);
        assert_eq!(tracker.silent_frames, 8_000);
    }

    #[test]
    fn a_word_resets_the_silence() {
        let mut tracker = SilenceTracker::new(DEFAULT_SILENCE_RMS);
        tracker.observe(0.3, 1_600);
        tracker.observe(0.0, 16_000);
        assert_eq!(tracker.silent_frames, 16_000);
        // A pause mid-sentence must not count towards the one that ends it.
        tracker.observe(0.3, 1_600);
        assert_eq!(tracker.silent_frames, 0);
    }

    #[test]
    fn the_threshold_sits_between_room_tone_and_speech() {
        let mut tracker = SilenceTracker::new(DEFAULT_SILENCE_RMS);
        tracker.observe(0.3, 100);
        tracker.observe(DEFAULT_SILENCE_RMS - 0.001, 100);
        assert_eq!(tracker.silent_frames, 100, "room tone counts as silence");
        tracker.observe(DEFAULT_SILENCE_RMS + 0.001, 100);
        assert_eq!(tracker.silent_frames, 0, "quiet speech does not");
    }

    #[test]
    fn plugins_are_not_offered_as_microphones() {
        assert!(is_real_microphone("alsa:sysdefault:CARD=BRIO"));
        assert!(is_real_microphone("alsa:sysdefault:CARD=Audio"));
        // ALSA's processing plugins and lower-level duplicates of a card.
        for plugin in [
            "alsa:null",
            "alsa:lavrate",
            "alsa:speexrate",
            "alsa:upmix",
            "alsa:vdownmix",
            "alsa:pipewire",
            "alsa:pulse",
            "alsa:sysdefault",
            "alsa:default",
            "alsa:hw:CARD=0,DEV=0",
            "alsa:plughw:CARD=0,DEV=0",
            "alsa:front:CARD=BRIO,DEV=0",
            "alsa:usbstream:CARD=BRIO",
        ] {
            assert!(!is_real_microphone(plugin), "{plugin} should not be listed");
        }
    }

    #[test]
    fn downmix_averages_channels() {
        let stereo = [1.0, 0.0, 0.5, 0.5];
        assert_eq!(downmix(&stereo, 2), vec![0.5, 0.5]);
        assert_eq!(downmix(&stereo, 1), stereo.to_vec());
    }

    #[test]
    fn resample_matches_the_rate_ratio() {
        let input: Vec<f32> = (0..48_000).map(|i| (i as f32 * 0.01).sin()).collect();
        let out = resample(&input, 48_000, TARGET_RATE);
        assert_eq!(out.len(), 16_000);
        assert!(out.iter().all(|s| s.abs() <= 1.0));
    }

    #[test]
    fn resample_is_a_no_op_at_the_target_rate() {
        let input = vec![0.1, -0.2, 0.3];
        assert_eq!(resample(&input, TARGET_RATE, TARGET_RATE), input);
    }

    /// Levels only, as the microphone test reads them:
    ///   cargo test --release -- --ignored --nocapture monitor_reports
    #[test]
    #[ignore]
    fn monitor_reports_levels_without_keeping_audio() {
        let recorder = Recorder::monitor("", DEFAULT_SILENCE_RMS).expect("opening the device");
        let mut highest = 0.0f32;
        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            highest = highest.max(recorder.current_rms());
        }
        let kept = recorder.finish();
        println!("  peak RMS seen: {highest:.4}");
        println!("  threshold:     {DEFAULT_SILENCE_RMS:.4}");
        println!("  above it:      {}", highest >= DEFAULT_SILENCE_RMS);
        assert!(kept.is_empty(), "monitoring must not accumulate audio");
    }

    /// Needs a real microphone, so it only runs on request:
    /// `cargo test -- --ignored live_capture`
    #[test]
    #[ignore]
    fn live_capture_produces_16k_mono_audio() {
        let recorder = Recorder::start("", DEFAULT_SILENCE_RMS).expect("opening the default input device");
        std::thread::sleep(std::time::Duration::from_millis(500));
        let samples = recorder.finish();
        assert!(
            samples.len() > TARGET_RATE as usize / 4,
            "expected ~0.5s of audio, got {} samples",
            samples.len()
        );
    }

    /// What the picker will show: `cargo test -- --ignored --nocapture offered_microphones`
    #[test]
    #[ignore]
    fn offered_microphones() {
        println!("  System default");
        for device in input_devices() {
            println!("  {}  [{}]", device.name, device.id);
        }
    }

    /// Opening a named device really opens that one:
    /// `YAPPER_TEST_DEVICE=alsa:sysdefault:CARD=BRIO cargo test -- --ignored --nocapture opens_the_chosen`
    #[test]
    #[ignore]
    fn opens_the_chosen_device() {
        let wanted = std::env::var("YAPPER_TEST_DEVICE").unwrap_or_default();
        let recorder = Recorder::start(&wanted, DEFAULT_SILENCE_RMS).expect("opening the device");
        std::thread::sleep(std::time::Duration::from_millis(400));
        let samples = recorder.finish();
        println!("  captured {} samples at 16 kHz mono", samples.len());
        assert!(samples.len() > TARGET_RATE as usize / 8);
    }
}
