//! Scripted preview frames for the Galley sheet, written as PPM files so the
//! look can be checked (and diffed against the prototype) without a compositor.
//! `just overlay-preview` stitches them into a contact sheet.

use std::f32::consts::PI;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use uuid::Uuid;

use super::fonts::FontSet;
use super::galley::{buffer_size, Galley, SheetSpec};
use super::paint::Frame;
use super::TICK_MS;
use crate::overlay_ipc::{OverlayIpcMessage, OverlayTextProducer};
use crate::overlay_state::OverlayStateMachine;

/// The page behind the sheet in the previews (a light desktop, like the prototype).
const BACKDROP: [u8; 3] = [0xdd, 0xdc, 0xd8];
const LEVEL_PERIOD_MS: u64 = 50;

struct Scenario {
    name: &'static str,
    events: Vec<(u64, OverlayIpcMessage)>,
    shots: Vec<(u64, &'static str)>,
    /// Windows (start, end) during which the mic hears speech.
    speech: Vec<(u64, u64)>,
}

/// A speech-like dBFS envelope: syllables at ~4 Hz, words gated at ~0.7 Hz,
/// resting on a quiet desktop mic's noise floor.
fn level_db(t_ms: u64, speech: &[(u64, u64)]) -> f32 {
    let talking = speech.iter().any(|&(a, b)| t_ms >= a && t_ms < b);
    if !talking {
        return -56.0 + 1.5 * ((t_ms as f32) / 90.0).sin();
    }
    let t = t_ms as f32 / 1000.0;
    let syllable = (0.5 + 0.5 * (2.0 * PI * 4.3 * t).sin()).powf(1.4);
    let word = if (2.0 * PI * 0.7 * t).sin() > -0.35 {
        1.0
    } else {
        0.15
    };
    -56.0 + 28.0 * syllable * word
}

struct Script {
    session: Uuid,
    seq: u64,
    events: Vec<(u64, OverlayIpcMessage)>,
}

impl Script {
    fn new() -> Self {
        Self {
            session: Uuid::new_v4(),
            seq: 0,
            events: Vec::new(),
        }
    }

    fn state(&mut self, at: u64, producer: OverlayTextProducer, state: &str) {
        self.seq += 1;
        self.events.push((
            at,
            OverlayIpcMessage::InterimState {
                session_id: self.session,
                producer,
                seq: self.seq,
                state: state.to_string(),
            },
        ));
    }

    fn text(&mut self, at: u64, producer: OverlayTextProducer, text: &str) {
        self.seq += 1;
        self.events.push((
            at,
            OverlayIpcMessage::InterimText {
                session_id: self.session,
                producer,
                seq: self.seq,
                text: text.to_string(),
            },
        ));
    }

    /// Feeds `sentence` word by word from `start`, `gap` ms apart. Returns the end time.
    fn words(&mut self, start: u64, gap: u64, sentence: &str) -> u64 {
        let words: Vec<&str> = sentence.split_whitespace().collect();
        let mut at = start;
        for n in 1..=words.len() {
            self.text(
                at,
                OverlayTextProducer::DaemonSttInterim,
                &words[..n].join(" "),
            );
            at += gap;
        }
        at
    }

    fn ended(&mut self, at: u64, reason: &str) {
        self.events.push((
            at,
            OverlayIpcMessage::SessionEnded {
                session_id: self.session,
                reason: Some(reason.to_string()),
            },
        ));
    }

    fn injected(&mut self, at: u64, success: bool) {
        self.events.push((
            at,
            OverlayIpcMessage::InjectionComplete {
                session_id: self.session,
                success,
                copy_only: false,
            },
        ));
    }

    fn warning(&mut self, at: u64, remaining_seconds: f32) {
        self.events.push((
            at,
            OverlayIpcMessage::SessionWarning {
                session_id: self.session,
                remaining_seconds: Some(remaining_seconds),
                limit_seconds: Some(600.0),
            },
        ));
    }

    fn busy(&mut self, at: u64, until: u64) {
        self.events.push((
            at,
            OverlayIpcMessage::InterimState {
                session_id: Uuid::nil(),
                producer: OverlayTextProducer::LlmAnswerDelta,
                seq: 1,
                state: "LLM busy; wait for current answer".to_string(),
            },
        ));
        self.events.push((
            until,
            OverlayIpcMessage::SessionEnded {
                session_id: Uuid::nil(),
                reason: Some("busy".to_string()),
            },
        ));
    }
}

fn scenarios() -> Vec<Scenario> {
    let stt = OverlayTextProducer::DaemonSttInterim;
    let llm = OverlayTextProducer::LlmAnswerDelta;
    let mut out = Vec::new();

    // short: one sentence, sealed, pasted, lifted out.
    let mut s = Script::new();
    s.state(0, stt, "listening");
    let end = s.words(
        420,
        190,
        "Okay, let's ship the coil version and see how it reads.",
    );
    s.state(end + 350, stt, "finalizing");
    s.ended(end + 900, "final");
    s.injected(end + 980, true);
    out.push(Scenario {
        name: "short",
        shots: vec![
            (60, "in"),
            (700, "listening"),
            (1_400, "interim"),
            (end + 500, "sealing"),
            (end + 1_200, "pasted"),
            (end + 980 + 900 + 90, "exit"),
        ],
        speech: vec![(300, end + 100)],
        events: s.events,
    });

    // long: prose that wraps, then overflows the line cap.
    let mut s = Script::new();
    s.state(0, stt, "listening");
    let end = s.words(
        300,
        130,
        "So the plan for this afternoon is to finish the overlay renderer refactor, move the \
         waveform into its own module, add a proper test for the per character fade, wire the \
         seal timing into the aux line, check the countdown against the daemon cap, and then \
         write the release notes before the standup so nobody has to ask what changed.",
    );
    s.state(end + 200, stt, "finalizing");
    s.ended(end + 1_100, "final");
    s.injected(end + 1_150, true);
    out.push(Scenario {
        name: "long",
        shots: vec![
            (2_200, "two-lines"),
            (5_000, "four-lines"),
            (end - 100, "cap"),
            (end + 700, "sealing"),
        ],
        speech: vec![(250, end)],
        events: s.events,
    });

    // llm: a question, the status line, a streamed answer, a busy notice, pasted.
    let mut s = Script::new();
    s.state(0, stt, "listening");
    let end = s.words(
        400,
        170,
        "what's the difference between a mutex and a semaphore",
    );
    s.state(end + 200, stt, "finalizing");
    s.state(end + 700, llm, "Generating answer...");
    let answer = "A mutex is a lock owned by one holder at a time; a semaphore is a counter that \
                  admits up to N holders, so it can also signal between threads.";
    let mut at = end + 1_300;
    let chars: Vec<char> = answer.chars().collect();
    let mut shown = 0;
    while shown < chars.len() {
        shown = (shown + 5).min(chars.len());
        s.text(at, llm, &chars[..shown].iter().collect::<String>());
        at += 45;
    }
    s.busy(at + 200, at + 1_400);
    s.ended(at + 1_800, "final");
    s.injected(at + 1_900, true);
    out.push(Scenario {
        name: "llm",
        shots: vec![
            (end + 900, "status"),
            (end + 1_900, "answer"),
            (at + 500, "busy"),
            (at + 2_100, "pasted"),
        ],
        speech: vec![(300, end)],
        events: s.events,
    });

    // empty: nothing heard, sealed to nothing.
    let mut s = Script::new();
    s.state(0, stt, "listening");
    s.state(1_800, stt, "finalizing");
    s.ended(2_300, "final");
    s.injected(2_350, false);
    out.push(Scenario {
        name: "empty",
        shots: vec![(900, "listening"), (2_000, "sealing"), (2_600, "no-text")],
        speech: vec![],
        events: s.events,
    });

    // error: the session aborts mid-sentence and the sheet falls away.
    let mut s = Script::new();
    s.state(0, stt, "listening");
    let end = s.words(400, 180, "so the plan is to");
    s.ended(end + 300, "abort");
    out.push(Scenario {
        name: "error",
        shots: vec![(end + 450, "fault"), (end + 300 + 600 + 80, "fall")],
        speech: vec![(300, end)],
        events: s.events,
    });

    // cap: the daemon warns; the timer becomes a countdown.
    let mut s = Script::new();
    s.state(0, stt, "listening");
    let end = s.words(300, 160, "and we keep talking well past the cap warning");
    s.warning(1_500, 30.0);
    out.push(Scenario {
        name: "cap",
        shots: vec![(1_200, "before"), (2_600, "countdown")],
        speech: vec![(250, end + 2_000)],
        events: s.events,
    });

    out
}

/// Runs every scenario through the state machine and the sheet, writing one
/// PPM per shot into `dir`. Returns the paths written. Each scenario gets a
/// fresh sheet and state machine so its clock can start at zero.
pub(super) fn write_previews(dir: &Path, spec: SheetSpec) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let (width, height) = buffer_size(spec.content_width, spec.max_lines);
    let mut written = Vec::new();

    for scenario in scenarios() {
        let mut galley = Galley::new(FontSet::load()?, &spec);
        let mut machine = OverlayStateMachine::new(Duration::from_millis(600));
        let mut events = scenario.events.clone();
        events.sort_by_key(|(at, _)| *at);
        let mut next_event = 0;
        let mut shots = scenario.shots.clone();
        shots.sort_by_key(|(at, _)| *at);
        let mut next_shot = 0;
        let mut next_level = 0;
        let mut bytes = vec![0u8; (width * height * 4) as usize];

        let mut now = 0;
        while next_shot < shots.len() {
            while next_level <= now {
                galley.push_audio_level(level_db(next_level, &scenario.speech), next_level);
                next_level += LEVEL_PERIOD_MS;
            }
            while next_event < events.len() && events[next_event].0 <= now {
                machine.apply_event(events[next_event].1.clone(), now);
                next_event += 1;
            }
            machine.advance_time(now);
            galley.observe(machine.visibility(), &spec, now);
            while next_shot < shots.len() && shots[next_shot].0 <= now {
                let mut frame = Frame {
                    bytes: &mut bytes,
                    width,
                    height,
                };
                galley.paint(&mut frame);
                let path = dir.join(format!("{}-{}.ppm", scenario.name, shots[next_shot].1));
                write_ppm(&path, &bytes, width, height)?;
                written.push(path);
                next_shot += 1;
            }
            now += TICK_MS;
        }
    }
    Ok(written)
}

/// Composites the premultiplied BGRA buffer over the backdrop and writes a binary PPM.
fn write_ppm(path: &Path, bgra: &[u8], width: u32, height: u32) -> Result<()> {
    let mut out = Vec::with_capacity((width * height * 3) as usize + 32);
    write!(out, "P6\n{width} {height}\n255\n")?;
    let (pixels, _) = bgra.as_chunks::<4>();
    for &[b, g, r, a] in pixels {
        let inv = 255 - a as u32;
        for (src, back) in [(r, BACKDROP[0]), (g, BACKDROP[1]), (b, BACKDROP[2])] {
            out.push((src as u32 + (back as u32 * inv + 127) / 255).min(255) as u8);
        }
    }
    std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_cover_every_scenario_and_render_ink() {
        let dir = std::env::temp_dir().join(format!("galley-preview-{}", Uuid::new_v4()));
        let spec = SheetSpec {
            content_width: 920,
            max_lines: 4,
            opacity: 1.0,
            adaptive_width: true,
            anchor_top: false,
        };
        let written = write_previews(&dir, spec).expect("previews");
        let names: Vec<String> = written
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        for expected in [
            "short-pasted.ppm",
            "long-cap.ppm",
            "llm-answer.ppm",
            "empty-no-text.ppm",
            "error-fault.ppm",
            "cap-countdown.ppm",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "missing {expected} in {names:?}"
            );
        }
        // Every frame that should carry text does, and the fault frame is rubric.
        for (name, min_ink) in [
            ("short-interim.ppm", 500),
            ("long-four-lines.ppm", 2_000),
            ("llm-answer.ppm", 500),
            ("cap-countdown.ppm", 300),
            ("error-fault.ppm", 100),
        ] {
            let (ink, _) = ink_and_rubric(&dir.join(name));
            assert!(ink > min_ink, "{name} carries ink: {ink} dark px");
        }
        let (_, rubric) = ink_and_rubric(&dir.join("error-fault.ppm"));
        assert!(rubric > 50, "the fault frame shows rubric: {rubric} px");
        let (_, rubric) = ink_and_rubric(&dir.join("short-pasted.ppm"));
        assert!(rubric < 5, "a pasted frame shows no rubric: {rubric} px");
        let (ink, _) = ink_and_rubric(&dir.join("short-exit.ppm"));
        let (ink_full, _) = ink_and_rubric(&dir.join("short-pasted.ppm"));
        assert!(
            ink < ink_full,
            "the exit frame is fading: {ink} < {ink_full}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Counts dark pixels and rubric (red-leaning) pixels in a PPM.
    fn ink_and_rubric(path: &Path) -> (usize, usize) {
        let sample = std::fs::read(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        let header_end = sample
            .iter()
            .enumerate()
            .filter(|(_, b)| **b == b'\n')
            .nth(2)
            .unwrap()
            .0
            + 1;
        let body = &sample[header_end..];
        let ink = body
            .chunks(3)
            .filter(|px| px[0] < 140 && px[1] < 140 && px[2] < 140)
            .count();
        let rubric = body
            .chunks(3)
            .filter(|px| px[0] > 120 && px[0] as i32 - px[1] as i32 > 60)
            .count();
        (ink, rubric)
    }

    #[test]
    fn synthetic_level_rests_on_the_floor_and_rises_when_talking() {
        assert!(level_db(100, &[]) < -50.0);
        let peak = (0..2_000)
            .step_by(10)
            .map(|t| level_db(t, &[(0, 2_000)]))
            .fold(f32::MIN, f32::max);
        assert!(peak > -32.0, "speech peaks well above the floor: {peak}");
    }
}
