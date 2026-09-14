//! Microphone capture via cpal (which talks to PipeWire through ALSA/Pulse).
//!
//! Whisper wants 16 kHz mono f32, so we ask the device for that directly when it
//! can do it and resample afterwards when it can't.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};

pub const TARGET_RATE: u32 = 16_000;

/// Room tone sits well below this; even quiet speech sits above it.
pub const SILENCE_RMS: f32 = 0.004;

/// Tracks how long the microphone has been quiet, so a recording can end
/// itself. Silence before the first word does not count — otherwise a
/// recording started a moment early would stop before you spoke.
#[derive(Default)]
struct SilenceTracker {
    /// Per-channel frames of quiet since the last sound.
    silent_frames: usize,
    heard_speech: bool,
}

impl SilenceTracker {
    fn observe(&mut self, rms: f32, frames: usize) {
        if rms >= SILENCE_RMS {
            self.silent_frames = 0;
            self.heard_speech = true;
        } else if self.heard_speech {
            self.silent_frames += frames;
        }
    }
}

/// Shared between the audio callback and the UI thread.
#[derive(Default)]
struct Capture {
    samples: Vec<f32>,
    /// Loudest sample since the UI last looked, for the level meter.
    peak: f32,
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
    /// Open the default input device and start filling the buffer.
    pub fn start() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| anyhow!("no input device available"))?;

        let supported = pick_config(&device)?;
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();

        let capture = Arc::new(Mutex::new(Capture::default()));
        let err_fn = |err| eprintln!("yapper: audio stream error: {err}");

        let stream = match sample_format {
            SampleFormat::F32 => build_stream::<f32>(&device, &config, &capture, err_fn),
            SampleFormat::I16 => build_stream::<i16>(&device, &config, &capture, err_fn),
            SampleFormat::I32 => build_stream::<i32>(&device, &config, &capture, err_fn),
            SampleFormat::U16 => build_stream::<u16>(&device, &config, &capture, err_fn),
            SampleFormat::I8 => build_stream::<i8>(&device, &config, &capture, err_fn),
            SampleFormat::U8 => build_stream::<u8>(&device, &config, &capture, err_fn),
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

    /// Stop capturing and hand back 16 kHz mono audio ready for Whisper.
    pub fn finish(self) -> Vec<f32> {
        let raw = {
            let mut capture = self.capture.lock().unwrap();
            std::mem::take(&mut capture.samples)
        };
        // Convert before dropping the stream, since prepare needs the
        // stream's rate and channel count.
        let prepared = self.prepare(raw);
        drop(self._stream);
        prepared
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
                let mut capture = capture.lock().unwrap();
                capture.samples.reserve(data.len());
                let mut sum_squares = 0.0f64;
                for &sample in data {
                    let value = f32::from_sample(sample);
                    capture.samples.push(value);
                    sum_squares += (value as f64) * (value as f64);
                    let magnitude = value.abs();
                    if magnitude > capture.peak {
                        capture.peak = magnitude;
                    }
                }
                // One RMS per callback buffer — a few tens of milliseconds,
                // which is the right window for "is anyone talking".
                if !data.is_empty() {
                    let rms = (sum_squares / data.len() as f64).sqrt() as f32;
                    capture.silence.observe(rms, data.len() / channels);
                }
            },
            err_fn,
            None,
        )
        .context("building the input stream")
}

/// Prefer a config we can use as-is: 16 kHz so there is nothing to resample,
/// mono so there is nothing to downmix, and a sample format that survives the
/// trip to f32 without losing resolution.
fn pick_config(device: &cpal::Device) -> Result<cpal::SupportedStreamConfig> {
    let mut best: Option<(u32, cpal::SupportedStreamConfigRange)> = None;

    if let Ok(configs) = device.supported_input_configs() {
        for candidate in configs {
            if format_rank(candidate.sample_format()).is_none() {
                continue;
            }
            if candidate.min_sample_rate() > TARGET_RATE
                || candidate.max_sample_rate() < TARGET_RATE
            {
                continue;
            }
            // Sample format dominates; channel count breaks ties.
            let score = format_rank(candidate.sample_format()).unwrap() * 100
                + u32::from(candidate.channels());
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
        let mut tracker = SilenceTracker::default();
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
        let mut tracker = SilenceTracker::default();
        tracker.observe(0.3, 1_600);
        tracker.observe(0.0, 16_000);
        assert_eq!(tracker.silent_frames, 16_000);
        // A pause mid-sentence must not count towards the one that ends it.
        tracker.observe(0.3, 1_600);
        assert_eq!(tracker.silent_frames, 0);
    }

    #[test]
    fn the_threshold_sits_between_room_tone_and_speech() {
        let mut tracker = SilenceTracker::default();
        tracker.observe(0.3, 100);
        tracker.observe(SILENCE_RMS - 0.001, 100);
        assert_eq!(tracker.silent_frames, 100, "room tone counts as silence");
        tracker.observe(SILENCE_RMS + 0.001, 100);
        assert_eq!(tracker.silent_frames, 0, "quiet speech does not");
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

    /// Needs a real microphone, so it only runs on request:
    /// `cargo test -- --ignored live_capture`
    #[test]
    #[ignore]
    fn live_capture_produces_16k_mono_audio() {
        let recorder = Recorder::start().expect("opening the default input device");
        std::thread::sleep(std::time::Duration::from_millis(500));
        let samples = recorder.finish();
        assert!(
            samples.len() > TARGET_RATE as usize / 4,
            "expected ~0.5s of audio, got {} samples",
            samples.len()
        );
    }
}
