//! The coil: one wire wound along a horizontal axis whose radius is the recent
//! microphone level, oldest at the left, newest at the right. It turns slowly
//! so the spring screws along the axis. Silence is a fine, even spring; a
//! phrase travels through it as a swell. DECODING freezes the last shape,
//! PASTED lets it fade.

use std::collections::VecDeque;
use std::f32::consts::PI;

use super::paint::Coverage;

/// Bins in the level profile (oldest first).
pub(super) const PROFILE_BINS: usize = 40;
/// The profile spans this window.
const PROFILE_SPAN_MS: u64 = 2_000;
const BIN_MS: u64 = PROFILE_SPAN_MS / PROFILE_BINS as u64;
/// Levels older than this are forgotten.
const HISTORY_KEEP_MS: u64 = PROFILE_SPAN_MS + 500;

/// dBFS the floor rests at before any level arrives (a quiet desktop mic).
const FLOOR_START_DB: f32 = -55.0;
/// The coil is fully open this far above the floor.
const DB_SPAN: f32 = 30.0;
/// The floor is the quietest level in the kept history: word gaps keep it at
/// the mic's noise while a sentence runs, and a hotter mic is learned within
/// one history window.
const FLOOR_MIN_DB: f32 = -90.0;
const FLOOR_MAX_DB: f32 = -35.0;
/// Fewer samples than this say nothing about the floor yet.
const FLOOR_MIN_SAMPLES: usize = 10;

const TURNS: f32 = 8.0;
const SEGMENTS: usize = 420;
const REST_RADIUS: f32 = 2.2;
const SQUASH: f32 = 0.22;
const ROTATION_LIVE: f32 = -2.2;
const ROTATION_IDLE: f32 = -0.8;
const FADE_MS: f32 = 900.0;

/// A time-stamped ring of microphone levels plus the noise floor it has
/// learned, so the coil reads the same on a quiet mic and a hot one.
#[derive(Debug, Default)]
pub(super) struct LevelHistory {
    samples: VecDeque<(u64, f32)>,
}

impl LevelHistory {
    pub(super) fn push(&mut self, now_ms: u64, level_db: f32) {
        if !level_db.is_finite() {
            return;
        }
        self.samples.push_back((now_ms, level_db));
        while let Some(&(t, _)) = self.samples.front() {
            if now_ms.saturating_sub(t) > HISTORY_KEEP_MS {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    pub(super) fn clear(&mut self) {
        self.samples.clear();
    }

    /// The mic's noise floor: the quietest level in the kept history.
    fn floor_db(&self) -> f32 {
        if self.samples.len() < FLOOR_MIN_SAMPLES {
            return FLOOR_START_DB;
        }
        self.samples
            .iter()
            .map(|&(_, db)| db)
            .reduce(f32::min)
            .unwrap_or(FLOOR_START_DB)
            .clamp(FLOOR_MIN_DB, FLOOR_MAX_DB)
    }

    /// Maps dBFS to 0..1 above the learned floor.
    fn norm(&self, db: f32) -> f32 {
        ((db - self.floor_db()) / DB_SPAN).clamp(0.0, 1.0)
    }

    /// The most recent level, normalised, or 0 when nothing has arrived.
    pub(super) fn latest(&self) -> f32 {
        self.samples
            .back()
            .map(|&(_, db)| self.norm(db))
            .unwrap_or(0.0)
    }

    /// Resamples the last two seconds into `PROFILE_BINS` bins (oldest first), holding
    /// the previous value across empty bins, then smooths three times with a 3-tap
    /// kernel so the coil reads as a turned form rather than a string of syllables.
    pub(super) fn profile(&self, now_ms: u64) -> [f32; PROFILE_BINS] {
        let start = now_ms.saturating_sub(PROFILE_SPAN_MS);
        let mut bins = [f32::NAN; PROFILE_BINS];
        for &(t, db) in &self.samples {
            if t < start || t > now_ms {
                continue;
            }
            let index = (((t - start) / BIN_MS) as usize).min(PROFILE_BINS - 1);
            let value = self.norm(db);
            bins[index] = if bins[index].is_nan() {
                value
            } else {
                bins[index].max(value)
            };
        }
        let mut held = 0.0;
        for bin in bins.iter_mut() {
            if bin.is_nan() {
                *bin = held;
            } else {
                held = *bin;
            }
        }
        for _ in 0..3 {
            let src = bins;
            for (i, bin) in bins.iter_mut().enumerate() {
                let left = if i == 0 { src[i] } else { src[i - 1] };
                let right = if i + 1 == PROFILE_BINS {
                    src[i]
                } else {
                    src[i + 1]
                };
                *bin = 0.25 * left + 0.5 * src[i] + 0.25 * right;
            }
        }
        bins
    }
}

#[derive(Debug)]
pub(super) struct Coil {
    rotation: f32,
    frozen: Option<[f32; PROFILE_BINS]>,
    fade: f32,
}

impl Default for Coil {
    fn default() -> Self {
        Self {
            rotation: 0.0,
            frozen: None,
            fade: 1.0,
        }
    }
}

impl Coil {
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    #[cfg(test)]
    fn fade(&self) -> f32 {
        self.fade
    }

    /// Advances rotation and fade. `live` keeps sampling the history; `fading`
    /// (after the paste or an error) drains the coil over 900 ms.
    pub(super) fn step(
        &mut self,
        dt_ms: u64,
        live: bool,
        fading: bool,
        history: &LevelHistory,
        now_ms: u64,
    ) {
        let dt = (dt_ms.min(50) as f32) / 1000.0;
        self.rotation += dt * if live { ROTATION_LIVE } else { ROTATION_IDLE };
        if live || self.frozen.is_none() {
            self.frozen = Some(history.profile(now_ms));
        }
        self.fade = if fading {
            (self.fade - dt_ms as f32 / FADE_MS).max(0.0)
        } else {
            1.0
        };
    }

    /// Draws the coil into a coverage canvas `scale` times larger than the 60 px cell.
    pub(super) fn draw(&self, canvas: &mut Coverage, cell: f32, scale: f32) {
        let profile = self.frozen.unwrap_or([0.0; PROFILE_BINS]);
        let cy = cell / 2.0;
        let x0 = 4.0;
        let x1 = cell - 4.0;
        let r_max = cell / 2.0 - 7.0;
        let mut prev: Option<(f32, f32)> = None;
        for j in 0..=SEGMENTS {
            let u = j as f32 / SEGMENTS as f32;
            let fi = u * (PROFILE_BINS - 1) as f32;
            let i0 = fi.floor() as usize;
            let f = fi - i0 as f32;
            let v = profile[i0] * (1.0 - f) + profile[(i0 + 1).min(PROFILE_BINS - 1)] * f;
            let r = REST_RADIUS + (r_max - REST_RADIUS) * v.powf(0.85);
            let theta = self.rotation + u * 2.0 * PI * TURNS;
            let (sin, cos) = theta.sin_cos();
            let x = x0 + u * (x1 - x0) + r * SQUASH * sin;
            let y = cy + r * cos;
            if let Some((px, py)) = prev {
                let depth = sin.max(0.0);
                let alpha = (0.12 + 0.62 * depth) * self.fade;
                let width = 0.7 + 0.5 * depth;
                canvas.stroke_segment(
                    (px * scale, py * scale),
                    (x * scale, y * scale),
                    width * scale,
                    alpha,
                );
            }
            prev = Some((x, y));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_floor_follows_the_mic_so_speech_reads_the_same_on_quiet_and_hot_mics() {
        let mut quiet = LevelHistory::default();
        for t in (0..1_000).step_by(50) {
            quiet.push(t, -55.0);
        }
        quiet.push(1_000, -32.0);
        let quiet_speech = quiet.latest();
        assert!(
            quiet_speech > 0.6 && quiet_speech < 0.9,
            "speech sits high in the range: {quiet_speech}"
        );

        let mut hot = LevelHistory::default();
        for t in (0..1_000).step_by(50) {
            hot.push(t, -35.0);
        }
        assert!(hot.latest() < 0.1, "steady noise is the floor, not signal");
        hot.push(1_000, -35.0 + 23.0);
        assert!(
            (hot.latest() - quiet_speech).abs() < 0.1,
            "the same lift above the floor reads the same: {}",
            hot.latest()
        );
    }

    #[test]
    fn a_sentence_with_word_gaps_does_not_lift_the_floor() {
        let mut history = LevelHistory::default();
        for t in (0..1_000).step_by(50) {
            history.push(t, -55.0);
        }
        // Words at -30 with 100 ms gaps back at the noise floor.
        for t in (1_000..6_000).step_by(50) {
            let in_gap = t % 400 < 100;
            history.push(t, if in_gap { -55.0 } else { -30.0 });
        }
        assert!(history.floor_db() < -52.0, "floor: {}", history.floor_db());
        assert!(history.latest() > 0.6);
    }

    #[test]
    fn non_finite_levels_are_ignored() {
        let mut history = LevelHistory::default();
        history.push(0, f32::NAN);
        history.push(10, f32::INFINITY);
        assert!(history.samples.is_empty());
        assert_eq!(history.floor_db(), FLOOR_START_DB);
    }

    #[test]
    fn profile_is_oldest_left_newest_right_and_holds_across_gaps() {
        let mut history = LevelHistory::default();
        for t in (0..2_000).step_by(50) {
            history.push(t, if t < 1_000 { -60.0 } else { -25.0 });
        }
        let profile = history.profile(2_000);
        assert!(
            profile[2] < 0.1,
            "old silence stays at the left: {}",
            profile[2]
        );
        assert!(
            profile[37] > 0.9,
            "recent speech is at the right: {}",
            profile[37]
        );

        let mut sparse = LevelHistory::default();
        sparse.push(0, -25.0);
        let held = sparse.profile(1_000);
        assert!(
            held[PROFILE_BINS - 1] > 0.9,
            "the last value is held across empty bins"
        );
    }

    #[test]
    fn history_forgets_old_samples() {
        let mut history = LevelHistory::default();
        history.push(0, -30.0);
        history.push(10_000, -60.0);
        assert_eq!(history.samples.len(), 1);
    }

    #[test]
    fn coil_freezes_when_not_live_and_fades_when_asked() {
        let mut history = LevelHistory::default();
        history.push(0, -25.0);
        let mut coil = Coil::default();
        coil.step(16, true, false, &history, 100);
        let live_profile = coil.frozen;
        history.push(3_000, -60.0);
        coil.step(16, false, false, &history, 3_000);
        assert_eq!(
            coil.frozen, live_profile,
            "a sealing coil keeps its last shape"
        );
        coil.step(450, false, true, &history, 3_450);
        assert!(
            coil.fade() < 0.6 && coil.fade() > 0.4,
            "half faded: {}",
            coil.fade()
        );
        coil.step(1_000, false, true, &history, 4_450);
        assert_eq!(coil.fade(), 0.0);
    }

    #[test]
    fn coil_draws_a_resting_spring_in_silence() {
        let coil = Coil::default();
        let mut canvas = Coverage::new(120, 120);
        coil.draw(&mut canvas, 60.0, 2.0);
        let mut bytes = vec![0u8; 60 * 60 * 4];
        let mut frame = super::super::paint::Frame {
            bytes: &mut bytes,
            width: 60,
            height: 60,
        };
        canvas.composite(&mut frame, 0, 0, 2, [0, 0, 0], 1.0);
        let inked = bytes.chunks(4).filter(|px| px[3] > 0).count();
        assert!(
            inked > 80,
            "a spring at rest still draws a thin coil: {inked} px"
        );
        let far = bytes
            .chunks(4)
            .enumerate()
            .filter(|(i, px)| px[3] > 0 && (i / 60) < 15)
            .count();
        assert_eq!(far, 0, "at rest nothing reaches the top of the cell");
    }
}
