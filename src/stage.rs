//! The animated centrepiece: a ring of bars that react to the microphone,
//! drawn behind the round record button.

use std::f64::consts::{FRAC_PI_2, PI, TAU};

use gtk::cairo;
use gtk::gdk;

/// Bars on each side of the button. They are mirrored, so the whole thing reads
/// as one symmetrical shape rather than two independent meters.
pub const BARS_PER_SIDE: usize = 4;

/// Resting height of each bar as a fraction of the space available, innermost
/// first. Hand-picked rather than random: an even row looks like a picket
/// fence, and a random one rarely looks deliberate.
const IDLE_HEIGHTS: [f32; BARS_PER_SIDE] = [0.40, 0.19, 0.30, 0.15];

/// How hard each bar answers the microphone, innermost first. The arch keeps
/// the movement centred on the button.
const RESPONSE: [f32; BARS_PER_SIDE] = [1.0, 0.82, 0.66, 0.48];

const BUTTON_RADIUS: f64 = 58.0;
const BAR_WIDTH: f64 = 17.0;
const BAR_GAP: f64 = 14.0;
const BAR_PITCH: f64 = BAR_WIDTH + BAR_GAP;
/// Clearance between the button's edge and the first bar.
const BUTTON_CLEARANCE: f64 = 16.0;
const MIN_BAR_HEIGHT: f64 = BAR_WIDTH;

/// One bar's animation state. Heights ease towards a target instead of
/// snapping, which is what makes the row look like it's dancing rather than
/// flickering.
pub struct Bar {
    /// Current height, 0.0..=1.0 of the available space.
    amplitude: f32,
    /// Height at rest, so the row still has shape in silence.
    idle: f32,
    /// How strongly this bar answers the microphone. Bars near the button move
    /// most, which gives the row its arch.
    weight: f32,
    /// Per-bar offsets, so neighbours don't move in lockstep.
    phase: f32,
    speed: f32,
}

pub struct Bars {
    bars: Vec<Bar>,
    /// Smoothed microphone level driving the whole row.
    energy: f32,
    /// Fades the row in while recording and out again afterwards.
    intensity: f32,
    /// Seconds since the window opened, kept for the pulse rings.
    elapsed: f32,
}

impl Bars {
    pub fn new() -> Self {
        let bars = (0..BARS_PER_SIDE)
            .map(|i| Bar {
                // Start at rest so the first frame isn't a jump from nothing.
                amplitude: IDLE_HEIGHTS[i],
                idle: IDLE_HEIGHTS[i],
                weight: RESPONSE[i],
                // Only the wobble is jittered, so neighbours never move in
                // lockstep while the row still holds its designed shape.
                phase: noise(i, 2) * std::f32::consts::TAU,
                speed: 5.0 + 4.0 * noise(i, 3),
            })
            .collect();

        Self {
            bars,
            energy: 0.0,
            intensity: 0.0,
            elapsed: 0.0,
        }
    }

    /// Advance the animation. `level` is the latest microphone peak (0.0..=1.0)
    /// and `active` is whether we're recording; `elapsed` is seconds since the
    /// window opened and `dt` seconds since the last frame.
    pub fn advance(&mut self, level: f32, active: bool, elapsed: f32, dt: f32) {
        self.elapsed = elapsed;

        // Frame-rate independent easing: the same visual speed at 60 or 144 Hz.
        let ease = |current: f32, target: f32, rate: f32| {
            current + (target - current) * (1.0 - (-rate * dt).exp())
        };

        // sqrt spreads quiet speech over more of the meter than raw amplitude.
        let target_energy = if active { level.sqrt() } else { 0.0 };
        // Rise fast so a sudden word registers, fall slowly so it doesn't flicker.
        let rate = if target_energy > self.energy { 22.0 } else { 7.0 };
        self.energy = ease(self.energy, target_energy.clamp(0.0, 1.0), rate);
        self.intensity = ease(self.intensity, if active { 1.0 } else { 0.0 }, 9.0);

        for bar in &mut self.bars {
            let wobble = 0.55 + 0.45 * (elapsed * bar.speed + bar.phase).sin();
            let driven = self.energy * bar.weight * wobble;
            // At rest every bar settles on its own idle height, so the row holds
            // a still shape and the animation can stop entirely.
            let target = (bar.idle + driven * (1.0 - bar.idle)).clamp(0.0, 1.0);
            bar.amplitude = ease(bar.amplitude, target, 26.0);
        }
    }

    /// True once the row has settled back to its resting shape, so the caller
    /// can stop asking for frames instead of burning a core on a still image.
    pub fn is_at_rest(&self) -> bool {
        self.intensity < 0.005
            && self.energy < 0.005
            && self
                .bars
                .iter()
                .all(|bar| (bar.amplitude - bar.idle).abs() < 0.002)
    }

    pub fn draw(&self, cr: &cairo::Context, width: f64, height: f64, accent: gdk::RGBA) {
        let (cx, cy) = (width / 2.0, height / 2.0);
        let max_height = (height - 8.0).max(MIN_BAR_HEIGHT);

        if self.intensity > 0.01 {
            self.draw_pulse(cr, cx, cy, accent);
        }

        for (i, bar) in self.bars.iter().enumerate() {
            let bar_height =
                MIN_BAR_HEIGHT + bar.amplitude as f64 * (max_height - MIN_BAR_HEIGHT);
            // Outer bars sit a little further back, which keeps the eye on the
            // button without the row looking like it's fading out.
            let depth = 1.0 - 0.05 * i as f64;
            let alpha = (0.60 + 0.38 * self.intensity as f64) * depth;
            let offset = BUTTON_RADIUS + BUTTON_CLEARANCE + BAR_WIDTH / 2.0 + i as f64 * BAR_PITCH;

            for side in [-1.0, 1.0] {
                let x = cx + side * offset - BAR_WIDTH / 2.0;
                if x < 0.0 || x + BAR_WIDTH > width {
                    continue;
                }
                cr.set_source_rgba(
                    accent.red() as f64,
                    accent.green() as f64,
                    accent.blue() as f64,
                    alpha,
                );
                rounded_bar(cr, x, cy - bar_height / 2.0, BAR_WIDTH, bar_height);
                let _ = cr.fill();
            }
        }
    }

    /// Rings expanding out from under the button while recording.
    fn draw_pulse(&self, cr: &cairo::Context, cx: f64, cy: f64, accent: gdk::RGBA) {
        for ring in 0..2 {
            // The two rings are half a cycle apart so one is always visible.
            let progress = ((self.pulse_phase() + ring as f64 * 0.5) % 1.0).clamp(0.0, 1.0);
            let radius = BUTTON_RADIUS + progress * 30.0;
            let alpha = (1.0 - progress) * 0.22 * self.intensity as f64;
            cr.set_source_rgba(
                accent.red() as f64,
                accent.green() as f64,
                accent.blue() as f64,
                alpha,
            );
            cr.set_line_width(2.5);
            cr.arc(cx, cy, radius, 0.0, TAU);
            let _ = cr.stroke();
        }
    }

    /// One ring every ~1.1 seconds.
    fn pulse_phase(&self) -> f64 {
        (self.elapsed as f64 * 0.9).fract()
    }
}

/// A vertical pill: a rectangle with fully rounded ends.
fn rounded_bar(cr: &cairo::Context, x: f64, y: f64, width: f64, height: f64) {
    let radius = (width / 2.0).min(height / 2.0);
    cr.new_sub_path();
    cr.arc(x + width - radius, y + radius, radius, -FRAC_PI_2, 0.0);
    cr.arc(x + width - radius, y + height - radius, radius, 0.0, FRAC_PI_2);
    cr.arc(x + radius, y + height - radius, radius, FRAC_PI_2, PI);
    cr.arc(x + radius, y + radius, radius, PI, 3.0 * FRAC_PI_2);
    cr.close_path();
}

/// Deterministic per-bar jitter, so the row looks irregular but identical on
/// every run. Cheaper than pulling in an RNG for fifteen numbers.
fn noise(index: usize, salt: u32) -> f32 {
    let mut x = (index as u32)
        .wrapping_mul(2_654_435_761)
        .wrapping_add(salt.wrapping_mul(40_503));
    x ^= x >> 13;
    x = x.wrapping_mul(1_274_126_177);
    x ^= x >> 16;
    (x % 10_000) as f32 / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_is_spread_and_stable() {
        let values: Vec<f32> = (0..BARS_PER_SIDE).map(|i| noise(i, 1)).collect();
        assert!(values.iter().all(|v| (0.0..1.0).contains(v)));
        assert_eq!(values[0], noise(0, 1));
        assert!(values.windows(2).any(|w| (w[0] - w[1]).abs() > 0.1));
    }

    #[test]
    fn bars_settle_when_not_recording() {
        let mut bars = Bars::new();
        for i in 0..60 {
            bars.advance(1.0, true, i as f32 / 60.0, 1.0 / 60.0);
        }
        assert!(!bars.is_at_rest());
        for i in 0..600 {
            bars.advance(0.0, false, 1.0 + i as f32 / 60.0, 1.0 / 60.0);
        }
        assert!(bars.is_at_rest(), "bars never settled");
    }
}
