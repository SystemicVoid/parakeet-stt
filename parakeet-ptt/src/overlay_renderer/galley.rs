//! The Galley sheet: the composition that turns a `SessionView` into pixels.
//!
//! A paper sheet carries two columns. The instrument column at the left holds
//! the coil, the lamp word with its dot, the session timer and an aux line,
//! all in Fira Code. The prose column sets the transcript in Newsreader with
//! the draft tail in italic pencil, or the LLM question above a rule and the
//! answer beneath it. A seal rule fills under the text while the Seal path
//! runs. The sheet rises in, lifts out on success and falls out on failure.
//! Every number here comes from the prototype (prototypes/overlay-look/variant-f.js).

use uuid::Uuid;

use super::coil::{Coil, LevelHistory};
use super::fonts::{Face, FontSet};
use super::paint::{Coverage, Frame, Rect, Rgb};
use super::prose::{Layout, Prose};
use crate::overlay_state::{OverlayMode, OverlayPhase, OverlayVisibility, SessionView};

pub(super) const DEFAULT_SHEET_WIDTH: u32 = 920;
pub(super) const MIN_SHEET_WIDTH: u32 = 480;
pub(super) const MAX_SHEET_WIDTH: u32 = 1600;

const PAD_L: f32 = 180.0;
const PAD_R: f32 = 40.0;
const PAD_T: f32 = 18.0;
const PAD_B: f32 = 14.0;
const MIN_SHEET_HEIGHT: f32 = 88.0;
const CORNER_RADIUS: f32 = 3.0;
const PROSE_PX: f32 = 17.0;
const LINE_HEIGHT: f32 = 25.5;
const RULE_GAP: f32 = 14.0;
const QUESTION_PX: f32 = 13.5;
const QUESTION_LINE: f32 = 20.0;
const QUESTION_RULE_ABOVE: f32 = 9.0;
const QUESTION_RULE_BELOW: f32 = 13.0;
const ANSWER_INDENT: f32 = 22.0;

const COIL_X: f32 = 22.0;
const COIL_Y: f32 = 14.0;
const COIL_CELL: f32 = 60.0;
const COIL_SCALE: usize = 2;
const COLUMN_X: f32 = COIL_X + COIL_CELL + 12.0;
const COLUMN_Y: f32 = 24.0;
const LAMP_PX: f32 = 10.5;
const LAMP_TRACKING: f32 = 0.13 * LAMP_PX;
const TIMER_PX: f32 = 11.0;
const TIMER_TRACKING: f32 = 0.02 * TIMER_PX;
const AUX_PX: f32 = 9.5;
const AUX_TRACKING: f32 = 0.08 * AUX_PX;
const META_PX: f32 = 10.5;
const META_TRACKING: f32 = 0.6;
const DOT_RADIUS: f32 = 3.0;

/// Shadow: `0 18px 38px .34` plus `0 2px 6px .20`, so the buffer pads are asymmetric.
pub(super) const SHADOW_PAD_TOP: u32 = 10;
pub(super) const SHADOW_PAD_SIDE: u32 = 26;
pub(super) const SHADOW_PAD_BOTTOM: u32 = 46;
/// Room for the entrance drop (+7) and the exit lift (-13) / fall (+9).
pub(super) const SLIDE_ROOM: u32 = 13;

const PAPER_TOP: Rgb = [0xf7, 0xf2, 0xe9];
const PAPER_BOTTOM: Rgb = [0xf0, 0xea, 0xdf];
const INK: Rgb = [0x1a, 0x17, 0x12];
const PENCIL: Rgb = [0x8c, 0x83, 0x77];
const RUBRIC: Rgb = [0xa3, 0x3a, 0x22];
const SAGE: Rgb = [0x5f, 0x8f, 0x6c];
const OCHRE: Rgb = [0xc2, 0x95, 0x3f];
const SLATE: Rgb = [0x5f, 0x7e, 0xa6];
const SHADOW: Rgb = [18, 12, 4];

const ENTRANCE_OPACITY_MS: f32 = 140.0;
const ENTRANCE_SLIDE_MS: f32 = 170.0;
const ENTRANCE_DROP_PX: f32 = 7.0;
const EXIT_LIFT_MS: f32 = 220.0;
const EXIT_LIFT_PX: f32 = -13.0;
const EXIT_FALL_MS: f32 = 200.0;
const EXIT_FALL_PX: f32 = 9.0;
const HEIGHT_ANIM_MS: f32 = 180.0;
const WIDTH_ANIM_MS: f32 = 200.0;
const QUOTE_BAR_MS: f32 = 220.0;

/// What the instrument column says and in which colours (dot, word).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Lamp {
    pub word: &'static str,
    pub dot: Rgb,
    pub ink: Rgb,
}

pub(super) fn lamp_for(view: &SessionView) -> Lamp {
    let lamp = |word, dot, ink| Lamp { word, dot, ink };
    match view.phase {
        OverlayPhase::Listening | OverlayPhase::Interim => match view.mode {
            OverlayMode::Llm => lamp("ASK", SLATE, PENCIL),
            OverlayMode::Stt => lamp("REC", SAGE, PENCIL),
        },
        OverlayPhase::Finalizing if view.failed() => lamp("FAULT", RUBRIC, RUBRIC),
        OverlayPhase::Finalizing => lamp("DECODING", OCHRE, PENCIL),
        OverlayPhase::Answering => lamp("ANSWER", SLATE, SLATE),
        OverlayPhase::Done { success: true } => lamp("PASTED", SAGE, SAGE),
        OverlayPhase::Done { success: false } if view.has_text() => lamp("FAILED", RUBRIC, RUBRIC),
        OverlayPhase::Done { success: false } => lamp("NO TEXT", RUBRIC, RUBRIC),
    }
}

/// `m:ss.t`, counting up.
pub(super) fn format_elapsed(ms: u64) -> String {
    let tenths = ms / 100;
    let minutes = tenths / 600;
    let rest = tenths % 600;
    format!("{}:{:02}.{}", minutes, rest / 10, rest % 10)
}

/// `-m:ss`, counting down (rounded up to the next second).
pub(super) fn format_remaining(ms: u64) -> String {
    let seconds = ms.div_ceil(1000);
    format!("-{}:{:02}", seconds / 60, seconds % 60)
}

/// The tallest sheet the layout can produce: the LLM layout at `max_lines` answer lines.
pub(super) fn max_sheet_height(max_lines: u32) -> u32 {
    let lines = max_lines.clamp(1, 10) as f32;
    let llm = PAD_T
        + QUESTION_LINE
        + QUESTION_RULE_ABOVE
        + 1.0
        + QUESTION_RULE_BELOW
        + lines * LINE_HEIGHT
        + RULE_GAP
        + PAD_B;
    llm.max(MIN_SHEET_HEIGHT).ceil() as u32
}

/// Buffer size for a sheet of `content_width`: the sheet plus shadow pads plus slide room.
pub(super) fn buffer_size(content_width: u32, max_lines: u32) -> (u32, u32) {
    (
        content_width + 2 * SHADOW_PAD_SIDE,
        max_sheet_height(max_lines) + SHADOW_PAD_TOP + SHADOW_PAD_BOTTOM + SLIDE_ROOM,
    )
}

/// Where the sheet's top edge sits inside the buffer. Bottom anchors keep the sheet
/// against the bottom pad so it grows upward; top anchors keep it against the top pad.
pub(super) fn sheet_origin_y(
    anchor_top: bool,
    buffer_height: u32,
    sheet_height: f32,
    dy: f32,
) -> f32 {
    if anchor_top {
        (SHADOW_PAD_TOP + SLIDE_ROOM) as f32 + dy
    } else {
        buffer_height as f32 - SHADOW_PAD_BOTTOM as f32 - sheet_height + dy
    }
}

/// What the renderer needs from the CLI to place the sheet.
#[derive(Debug, Clone, Copy)]
pub(super) struct SheetSpec {
    pub content_width: u32,
    pub max_lines: u32,
    pub opacity: f32,
    pub adaptive_width: bool,
    pub anchor_top: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MotionKind {
    In,
    OutLift,
    OutFall,
}

#[derive(Debug, Clone, Copy)]
struct Motion {
    kind: MotionKind,
    started_ms: u64,
}

impl Motion {
    fn elapsed(&self, now_ms: u64) -> f32 {
        now_ms.saturating_sub(self.started_ms) as f32
    }

    fn opacity(&self, now_ms: u64) -> f32 {
        let t = self.elapsed(now_ms);
        match self.kind {
            MotionKind::In => ease_out((t / ENTRANCE_OPACITY_MS).min(1.0)),
            MotionKind::OutLift => 1.0 - ease_out((t / EXIT_LIFT_MS).min(1.0)),
            MotionKind::OutFall => 1.0 - ease_in((t / EXIT_FALL_MS).min(1.0)),
        }
    }

    fn dy(&self, now_ms: u64) -> f32 {
        let t = self.elapsed(now_ms);
        match self.kind {
            MotionKind::In => ENTRANCE_DROP_PX * (1.0 - ease_out((t / ENTRANCE_SLIDE_MS).min(1.0))),
            MotionKind::OutLift => EXIT_LIFT_PX * ease_out((t / EXIT_LIFT_MS).min(1.0)),
            MotionKind::OutFall => EXIT_FALL_PX * ease_in((t / EXIT_FALL_MS).min(1.0)),
        }
    }

    fn finished(&self, now_ms: u64) -> bool {
        let t = self.elapsed(now_ms);
        match self.kind {
            MotionKind::In => t >= ENTRANCE_SLIDE_MS,
            MotionKind::OutLift => t >= EXIT_LIFT_MS,
            MotionKind::OutFall => t >= EXIT_FALL_MS,
        }
    }

    fn exiting(&self) -> bool {
        self.kind != MotionKind::In
    }
}

fn ease_out(t: f32) -> f32 {
    let inv = 1.0 - t.clamp(0.0, 1.0);
    1.0 - inv * inv * inv
}

fn ease_in(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * t
}

/// A value that eases towards a target over a fixed duration.
#[derive(Debug, Clone, Copy)]
struct Tween {
    from: f32,
    to: f32,
    started_ms: u64,
    duration_ms: f32,
}

impl Tween {
    fn snapped(value: f32, duration_ms: f32) -> Self {
        Self {
            from: value,
            to: value,
            started_ms: 0,
            duration_ms,
        }
    }

    fn retarget(&mut self, target: f32, now_ms: u64) {
        if (target - self.to).abs() < 0.5 {
            return;
        }
        self.from = self.value(now_ms);
        self.to = target;
        self.started_ms = now_ms;
    }

    fn snap(&mut self, value: f32) {
        self.from = value;
        self.to = value;
    }

    fn value(&self, now_ms: u64) -> f32 {
        let t = now_ms.saturating_sub(self.started_ms) as f32 / self.duration_ms;
        self.from + (self.to - self.from) * ease_out(t.min(1.0))
    }

    fn settled(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.started_ms) as f32 >= self.duration_ms
    }
}

/// The sheet rectangle painted this frame (for damage and window geometry).
pub(super) type SheetRect = Rect;

/// What `observe` measured for the current frame: the prose layout, the width the
/// sheet wants, and the height it has. `paint` only draws it.
#[derive(Debug, Clone)]
struct Measured {
    layout: Layout,
    status_line: Option<String>,
    llm_layout: bool,
    prose_indent: f32,
    lines_now: f32,
    sheet_height: f32,
    sheet_width: f32,
}

pub(super) struct Galley {
    fonts: FontSet,
    prose: Prose,
    levels: LevelHistory,
    coil: Coil,
    canvas: Coverage,
    motion: Option<Motion>,
    /// The view being drawn; kept through the exit motion after the state went hidden.
    view: Option<SessionView>,
    phase: Option<OverlayPhase>,
    phase_at_ms: u64,
    seal_started_ms: Option<u64>,
    seal_took_ms: u64,
    answering_at_ms: Option<u64>,
    lines: Tween,
    width: Tween,
    measured: Option<Measured>,
    last_tick_ms: u64,
}

impl Galley {
    pub(super) fn new(fonts: FontSet, spec: &SheetSpec) -> Self {
        let cell = COIL_CELL as usize * COIL_SCALE;
        let rest_width = if spec.adaptive_width {
            MIN_SHEET_WIDTH.min(spec.content_width)
        } else {
            spec.content_width
        };
        Self {
            fonts,
            prose: Prose::default(),
            levels: LevelHistory::default(),
            coil: Coil::default(),
            canvas: Coverage::new(cell, cell),
            motion: None,
            view: None,
            phase: None,
            phase_at_ms: 0,
            seal_started_ms: None,
            seal_took_ms: 0,
            answering_at_ms: None,
            lines: Tween::snapped(1.0, HEIGHT_ANIM_MS),
            width: Tween::snapped(rest_width as f32, WIDTH_ANIM_MS),
            measured: None,
            last_tick_ms: 0,
        }
    }

    pub(super) fn push_audio_level(&mut self, level_db: f32, now_ms: u64) {
        self.levels.push(now_ms, level_db);
    }

    /// Feeds the latest state; tracks transitions for the animations.
    pub(super) fn observe(
        &mut self,
        visibility: &OverlayVisibility,
        spec: &SheetSpec,
        now_ms: u64,
    ) {
        let dt_ms = now_ms.saturating_sub(self.last_tick_ms);
        self.last_tick_ms = now_ms;

        match visibility {
            OverlayVisibility::Visible(incoming) => {
                let arriving = self
                    .view
                    .as_ref()
                    .is_none_or(|current| current.session_id != incoming.session_id)
                    || self.motion.is_some_and(|m| m.exiting());
                if arriving {
                    self.start_session(incoming.session_id, now_ms);
                }
                if self.phase != Some(incoming.phase) {
                    if self.phase == Some(OverlayPhase::Finalizing) {
                        if let Some(started) = self.seal_started_ms {
                            self.seal_took_ms = now_ms.saturating_sub(started);
                        }
                    }
                    if incoming.phase == OverlayPhase::Finalizing {
                        self.seal_started_ms = Some(now_ms);
                    }
                    if incoming.phase == OverlayPhase::Answering && self.answering_at_ms.is_none() {
                        self.answering_at_ms = Some(now_ms);
                    }
                    self.phase = Some(incoming.phase);
                    self.phase_at_ms = now_ms;
                }
                let roman_only = answer_layout(incoming);
                self.prose.set_roman_only(roman_only);
                let text = if roman_only {
                    incoming.answer.as_str()
                } else {
                    incoming.transcript.as_str()
                };
                self.prose.update(text, now_ms);
                if matches!(
                    incoming.phase,
                    OverlayPhase::Finalizing | OverlayPhase::Done { .. }
                ) {
                    self.prose.commit_all(now_ms);
                }
                self.view = Some(incoming.clone());
            }
            OverlayVisibility::Hidden => {
                if let Some(view) = &self.view {
                    let exiting = self.motion.is_some_and(|m| m.exiting());
                    if !exiting {
                        let fall = view.failed()
                            || matches!(view.phase, OverlayPhase::Done { success: false });
                        self.motion = Some(Motion {
                            kind: if fall {
                                MotionKind::OutFall
                            } else {
                                MotionKind::OutLift
                            },
                            started_ms: now_ms,
                        });
                    } else if self.motion.is_some_and(|m| m.finished(now_ms)) {
                        self.view = None;
                        self.motion = None;
                        self.phase = None;
                        self.measured = None;
                    }
                }
            }
        }

        let (live, fading) = match &self.view {
            Some(view) => (
                view.is_live() && !self.motion.is_some_and(|m| m.exiting()),
                matches!(view.phase, OverlayPhase::Done { .. }) || view.failed(),
            ),
            None => (false, false),
        };
        self.coil.step(dt_ms, live, fading, &self.levels, now_ms);
        self.measured = self.measure(spec, now_ms);
    }

    /// Lays the prose out at the full measure and moves the sheet's width and height
    /// targets. Called from `observe`, so the tweens advance on every tick even when
    /// a frame is not painted.
    fn measure(&mut self, spec: &SheetSpec, now_ms: u64) -> Option<Measured> {
        let view = self.view.as_ref()?;
        let exiting = self.motion.is_some_and(|m| m.exiting());

        let full_measure = spec.content_width as f32 - PAD_L - PAD_R;
        let llm_layout = question_layout(view);
        let prose_indent = if llm_layout { ANSWER_INDENT } else { 0.0 };
        let measure = (full_measure - prose_indent).max(40.0);
        let layout = self.prose.layout(
            &self.fonts,
            PROSE_PX,
            measure,
            spec.max_lines as usize,
            now_ms,
        );
        let status_line = status_text(view).filter(|_| self.prose.is_empty());
        let mut widest = layout.widest + prose_indent;
        if let Some(status) = &status_line {
            widest =
                widest.max(self.fonts.measure(Face::Italic, PROSE_PX, status, 0.0) + prose_indent);
        }
        if let Some(question) = &view.question {
            widest = widest.max(
                self.fonts
                    .measure(Face::Italic, QUESTION_PX, question, 0.0)
                    .min(full_measure),
            );
        }

        let line_count = layout.lines.max(1) as f32;
        if !exiting {
            self.lines.retarget(line_count, now_ms);
        }
        let lines_now = self.lines.value(now_ms);
        let body_height = lines_now * LINE_HEIGHT + RULE_GAP;
        let sheet_height = if llm_layout {
            PAD_T
                + QUESTION_LINE
                + QUESTION_RULE_ABOVE
                + 1.0
                + QUESTION_RULE_BELOW
                + body_height
                + PAD_B
        } else {
            PAD_T + body_height + 1.0 + PAD_B
        }
        .max(MIN_SHEET_HEIGHT);

        let max_width = spec.content_width as f32;
        if spec.adaptive_width {
            let mut target = (PAD_L + widest + PAD_R)
                .ceil()
                .clamp(MIN_SHEET_WIDTH as f32, max_width);
            if view.phase == OverlayPhase::Interim {
                target = target.max(self.width.to);
            }
            if !exiting {
                self.width.retarget(target, now_ms);
            }
        } else {
            self.width.snap(max_width);
        }
        let sheet_width = self
            .width
            .value(now_ms)
            .round()
            .clamp(MIN_SHEET_WIDTH as f32, max_width);

        Some(Measured {
            layout,
            status_line,
            llm_layout,
            prose_indent,
            lines_now,
            sheet_height,
            sheet_width,
        })
    }

    fn start_session(&mut self, session_id: Uuid, now_ms: u64) {
        let _ = session_id;
        self.prose.reset();
        self.levels.clear();
        self.coil.reset();
        self.motion = Some(Motion {
            kind: MotionKind::In,
            started_ms: now_ms,
        });
        self.phase = None;
        self.seal_started_ms = None;
        self.seal_took_ms = 0;
        self.answering_at_ms = None;
        self.lines.snap(1.0);
    }

    pub(super) fn is_animating(&self, now_ms: u64) -> bool {
        self.view.is_some()
            || self.motion.is_some_and(|m| !m.finished(now_ms))
            || !self.lines.settled(now_ms)
            || !self.width.settled(now_ms)
    }

    /// Paints the sheet into `frame`. Returns the sheet rectangle, or `None` when
    /// nothing is on screen.
    pub(super) fn paint(
        &mut self,
        frame: &mut Frame,
        spec: &SheetSpec,
        now_ms: u64,
    ) -> Option<SheetRect> {
        frame.clear();
        let view = self.view.clone()?;
        let motion = self.motion.unwrap_or(Motion {
            kind: MotionKind::In,
            started_ms: 0,
        });
        let opacity = (motion.opacity(now_ms) * spec.opacity).clamp(0.0, 1.0);
        if opacity <= 0.0 {
            return None;
        }
        let exiting = motion.exiting();
        let dy = motion.dy(now_ms);

        let Measured {
            layout,
            status_line,
            llm_layout,
            prose_indent,
            lines_now,
            sheet_height,
            sheet_width,
        } = self.measured.clone()?;

        let sheet = Rect {
            x: SHADOW_PAD_SIDE as f32,
            y: sheet_origin_y(spec.anchor_top, frame.height, sheet_height, dy).round(),
            w: sheet_width,
            h: sheet_height.round(),
        };
        let visible_measure = sheet_width - PAD_L - PAD_R;

        // ---- paper
        frame.draw_shadow(sheet, CORNER_RADIUS, 18.0, 38.0, SHADOW, 0.34);
        frame.draw_shadow(sheet, CORNER_RADIUS, 2.0, 6.0, SHADOW, 0.20);
        frame.fill_rounded_rect_gradient(sheet, CORNER_RADIUS, PAPER_TOP, PAPER_BOTTOM, 1.0);
        frame.stroke_rounded_rect_inset(sheet, CORNER_RADIUS, INK, 0.10);

        // ---- instrument column
        let live = view.is_live() && !exiting;
        let sealing = view.phase == OverlayPhase::Finalizing && !view.failed();
        let error = view.failed();
        let answering = answer_layout(&view);
        let lamp = lamp_for(&view);

        let coil_color = if error {
            RUBRIC
        } else if answering {
            SLATE
        } else {
            INK
        };
        self.canvas.clear();
        self.coil
            .draw(&mut self.canvas, COIL_CELL, COIL_SCALE as f32);
        self.canvas.composite(
            frame,
            (sheet.x + COIL_X) as i32,
            (sheet.y + COIL_Y) as i32,
            COIL_SCALE,
            coil_color,
            1.0,
        );

        let column_x = sheet.x + COLUMN_X;
        let lamp_baseline = sheet.y + COLUMN_Y + 10.2;
        let pulse = if live {
            0.85 + 0.75 * self.levels.latest()
        } else if view.phase == (OverlayPhase::Done { success: true }) {
            1.25
        } else {
            1.0
        };
        let dot_alpha = if sealing {
            0.55 + 0.45 * ((now_ms as f32) / 160.0).sin().abs()
        } else {
            1.0
        };
        frame.fill_disc(
            column_x + DOT_RADIUS,
            lamp_baseline - 4.0,
            DOT_RADIUS * pulse,
            lamp.dot,
            dot_alpha,
        );
        self.fonts.draw_text(
            frame,
            Face::Mono,
            LAMP_PX,
            column_x + 12.0,
            lamp_baseline,
            lamp.word,
            LAMP_TRACKING,
            lamp.ink,
            1.0,
        );

        let (timer, timer_color) = match (view.warning, view.remaining_ms(now_ms)) {
            (Some(_), Some(remaining)) => (format_remaining(remaining), RUBRIC),
            (Some(_), None) => (format_elapsed(view.elapsed_ms(now_ms)), RUBRIC),
            (None, _) => (format_elapsed(view.elapsed_ms(now_ms)), PENCIL),
        };
        self.fonts.draw_text(
            frame,
            Face::Mono,
            TIMER_PX,
            column_x,
            lamp_baseline + 16.0,
            &timer,
            TIMER_TRACKING,
            timer_color,
            1.0,
        );
        if matches!(view.phase, OverlayPhase::Done { .. }) && self.seal_took_ms > 0 {
            let aux = format!("seal {}ms", self.seal_took_ms);
            self.fonts.draw_text(
                frame,
                Face::Mono,
                AUX_PX,
                column_x,
                lamp_baseline + 30.0,
                &aux,
                AUX_TRACKING,
                PENCIL,
                1.0,
            );
        }

        // ---- prose column
        let prose_x = sheet.x + PAD_L + prose_indent;
        let mut body_top = sheet.y + PAD_T;
        if llm_layout {
            let question = view.question.as_deref().unwrap_or_default();
            let fitted =
                self.fonts
                    .fit_with_ellipsis(Face::Italic, QUESTION_PX, question, visible_measure);
            self.fonts.draw_text(
                frame,
                Face::Italic,
                QUESTION_PX,
                sheet.x + PAD_L,
                body_top + 13.2,
                &fitted,
                0.0,
                PENCIL,
                1.0,
            );
            body_top += QUESTION_LINE + QUESTION_RULE_ABOVE + 1.0 + QUESTION_RULE_BELOW;
        }
        let first_baseline = body_top + 16.75;
        for placed in &layout.placed {
            if placed.ch == ' ' || placed.alpha <= 0.0 {
                continue;
            }
            let color = if placed.pencil { PENCIL } else { INK };
            let glyph = self.fonts.glyph(placed.face, placed.ch, PROSE_PX);
            self.fonts.draw_glyph(
                frame,
                &glyph,
                prose_x + placed.x,
                first_baseline + placed.line as f32 * LINE_HEIGHT + placed.dy,
                color,
                placed.alpha,
            );
        }
        if let Some(status) = &status_line {
            self.fonts.draw_text(
                frame,
                Face::Italic,
                PROSE_PX,
                prose_x,
                first_baseline,
                status,
                0.0,
                PENCIL,
                1.0,
            );
        }

        // ---- quote bar (LLM answer)
        if llm_layout {
            let grow = self
                .answering_at_ms
                .map(|at| ease_out((now_ms.saturating_sub(at) as f32 / QUOTE_BAR_MS).min(1.0)))
                .unwrap_or(0.0);
            let bar_top = body_top;
            let bar_bottom = body_top + lines_now * LINE_HEIGHT;
            frame.vline(
                (sheet.x + PAD_L) as i32,
                bar_top,
                bar_top + (bar_bottom - bar_top) * grow,
                SLATE,
                1.0,
            );
        }

        // ---- the rule: the measure line, then the seal drawn under the text
        let rule_y = if llm_layout {
            sheet.y + PAD_T + QUESTION_LINE + QUESTION_RULE_ABOVE
        } else {
            sheet.bottom() - PAD_B - 1.0
        };
        let since_phase = now_ms.saturating_sub(self.phase_at_ms) as f32;
        let (track, ink_width, rule_color) = match view.phase {
            OverlayPhase::Listening if view.transcript.trim().is_empty() => (true, 0.0, INK),
            OverlayPhase::Finalizing | OverlayPhase::Done { .. } if error => (
                true,
                visible_measure * (1.0 - (-since_phase / 110.0).exp()),
                RUBRIC,
            ),
            OverlayPhase::Finalizing => (
                true,
                visible_measure * (1.0 - (-((since_phase - 110.0).max(0.0)) / 330.0).exp()),
                INK,
            ),
            OverlayPhase::Done { .. } | OverlayPhase::Answering => {
                if view.has_text() {
                    (true, visible_measure, INK)
                } else {
                    (true, visible_measure * 0.5, PENCIL)
                }
            }
            _ => (false, 0.0, INK),
        };
        if llm_layout || track {
            let x0 = sheet.x + PAD_L;
            frame.hline(x0, x0 + visible_measure, rule_y as i32, INK, 0.14);
            frame.hline(x0, x0 + ink_width, rule_y as i32, rule_color, 1.0);
        }

        // ---- meta: the error reason, or a transient notice
        let meta = if error {
            Some((
                view.reason
                    .clone()
                    .unwrap_or_else(|| "session failed".to_string()),
                RUBRIC,
            ))
        } else {
            view.notice.clone().map(|notice| (notice, PENCIL))
        };
        if let Some((text, color)) = meta {
            let text = text.to_uppercase();
            let width = self
                .fonts
                .measure(Face::Roman, META_PX, &text, META_TRACKING);
            let meta_y = if llm_layout {
                sheet.bottom() - PAD_B - 1.0
            } else {
                rule_y
            };
            self.fonts.draw_text(
                frame,
                Face::Roman,
                META_PX,
                sheet.x + PAD_L + visible_measure - width,
                meta_y - 10.0,
                &text,
                META_TRACKING,
                color,
                1.0,
            );
        }

        if opacity < 1.0 {
            frame.scale_alpha(opacity);
        }

        Some(sheet)
    }

    /// The lamp word plus the first prose words, for the fallback window title.
    pub(super) fn title(&self) -> Option<String> {
        let view = self.view.as_ref()?;
        let text = if answer_layout(view) {
            view.answer.as_str()
        } else {
            view.transcript.as_str()
        };
        let snippet: String = text
            .split_whitespace()
            .take(8)
            .collect::<Vec<_>>()
            .join(" ");
        Some(
            format!("{} {}", lamp_for(view).word, snippet)
                .trim()
                .to_string(),
        )
    }
}

/// The answer is set roman-only (no draft tail) while the LLM answers or after it pasted.
fn answer_layout(view: &SessionView) -> bool {
    view.mode == OverlayMode::Llm
        && matches!(
            view.phase,
            OverlayPhase::Answering | OverlayPhase::Done { .. }
        )
}

/// The question / rule / answer layout applies once the LLM has a question to show.
fn question_layout(view: &SessionView) -> bool {
    view.mode == OverlayMode::Llm && view.question.is_some()
}

/// An LLM status line ("Generating answer...") shows until the first answer delta.
fn status_text(view: &SessionView) -> Option<String> {
    if view.mode == OverlayMode::Llm && view.answer.trim().is_empty() {
        view.status.clone().filter(|s| !s.trim().is_empty())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay_state::CapWarning;

    fn view(phase: OverlayPhase) -> SessionView {
        SessionView {
            session_id: Uuid::new_v4(),
            mode: OverlayMode::Stt,
            phase,
            transcript: "hello".to_string(),
            question: None,
            answer: String::new(),
            status: None,
            notice: None,
            reason: None,
            started_ms: 0,
            ended_ms: None,
            warning: None,
        }
    }

    #[test]
    fn lamp_vocabulary_matches_the_prototype() {
        assert_eq!(lamp_for(&view(OverlayPhase::Listening)).word, "REC");
        assert_eq!(lamp_for(&view(OverlayPhase::Finalizing)).word, "DECODING");
        assert_eq!(
            lamp_for(&view(OverlayPhase::Done { success: true })).word,
            "PASTED"
        );
        assert_eq!(
            lamp_for(&view(OverlayPhase::Done { success: false })).word,
            "FAILED"
        );

        let mut empty = view(OverlayPhase::Done { success: false });
        empty.transcript.clear();
        assert_eq!(lamp_for(&empty).word, "NO TEXT");

        let mut llm = view(OverlayPhase::Interim);
        llm.mode = OverlayMode::Llm;
        assert_eq!(lamp_for(&llm).word, "ASK");
        llm.phase = OverlayPhase::Answering;
        assert_eq!(lamp_for(&llm).word, "ANSWER");

        let mut failed = view(OverlayPhase::Finalizing);
        failed.reason = Some("abort".to_string());
        assert_eq!(lamp_for(&failed).word, "FAULT");
    }

    #[test]
    fn timer_formats_count_up_and_count_down() {
        assert_eq!(format_elapsed(0), "0:00.0");
        assert_eq!(format_elapsed(7_460), "0:07.4");
        assert_eq!(format_elapsed(125_900), "2:05.9");
        assert_eq!(format_remaining(117_001), "-1:58");
        assert_eq!(format_remaining(60_000), "-1:00");
        assert_eq!(format_remaining(0), "-0:00");
    }

    #[test]
    fn sheet_origin_follows_the_anchor_family() {
        let (_, height) = buffer_size(DEFAULT_SHEET_WIDTH, 4);
        let bottom = sheet_origin_y(false, height, 88.0, 0.0);
        assert_eq!(bottom + 88.0 + SHADOW_PAD_BOTTOM as f32, height as f32);
        let top = sheet_origin_y(true, height, 88.0, 0.0);
        assert_eq!(top, (SHADOW_PAD_TOP + SLIDE_ROOM) as f32);
        assert!(sheet_origin_y(false, height, 88.0, -13.0) >= SHADOW_PAD_TOP as f32);
    }

    #[test]
    fn buffer_fits_the_tallest_layout_and_the_slide() {
        let (width, height) = buffer_size(DEFAULT_SHEET_WIDTH, 4);
        assert_eq!(width, DEFAULT_SHEET_WIDTH + 2 * SHADOW_PAD_SIDE);
        assert!(height >= max_sheet_height(4) + SHADOW_PAD_TOP + SHADOW_PAD_BOTTOM + SLIDE_ROOM);
        assert!(max_sheet_height(4) >= 190);
        assert!(max_sheet_height(1) >= MIN_SHEET_HEIGHT as u32);
    }

    fn spec() -> SheetSpec {
        SheetSpec {
            content_width: DEFAULT_SHEET_WIDTH,
            max_lines: 4,
            opacity: 1.0,
            adaptive_width: false,
            anchor_top: false,
        }
    }

    fn paint_view(galley: &mut Galley, view: &SessionView, now_ms: u64) -> (Option<Rect>, Vec<u8>) {
        let (w, h) = buffer_size(DEFAULT_SHEET_WIDTH, 4);
        let mut bytes = vec![0u8; (w * h * 4) as usize];
        let mut frame = Frame {
            bytes: &mut bytes,
            width: w,
            height: h,
        };
        galley.observe(&OverlayVisibility::Visible(view.clone()), &spec(), now_ms);
        galley.observe(
            &OverlayVisibility::Visible(view.clone()),
            &spec(),
            now_ms + 1_000,
        );
        let rect = galley.paint(&mut frame, &spec(), now_ms + 1_000);
        (rect, bytes)
    }

    #[test]
    fn sheet_height_follows_the_line_count_and_layout() {
        let fonts = FontSet::load().expect("fonts");
        let mut galley = Galley::new(fonts, &spec());
        let mut short = view(OverlayPhase::Interim);
        let (rect, _) = paint_view(&mut galley, &short, 0);
        assert_eq!(rect.expect("visible").h, MIN_SHEET_HEIGHT);

        short.transcript = "okay so the plan for this afternoon is to finish the overlay renderer refactor, move the waveform into its own module, add a test for the per character fade and then write the release notes".to_string();
        let mut galley = Galley::new(FontSet::load().expect("fonts"), &spec());
        let (rect, _) = paint_view(&mut galley, &short, 0);
        let two_lines = PAD_T + 2.0 * LINE_HEIGHT + RULE_GAP + 1.0 + PAD_B;
        assert!(
            rect.expect("visible").h >= two_lines,
            "long text grows the sheet"
        );

        let mut llm = view(OverlayPhase::Answering);
        llm.mode = OverlayMode::Llm;
        llm.question = Some("what is a mutex".to_string());
        llm.answer = "A lock.".to_string();
        let mut galley = Galley::new(FontSet::load().expect("fonts"), &spec());
        let (rect, _) = paint_view(&mut galley, &llm, 0);
        let expected = PAD_T
            + QUESTION_LINE
            + QUESTION_RULE_ABOVE
            + 1.0
            + QUESTION_RULE_BELOW
            + LINE_HEIGHT
            + RULE_GAP
            + PAD_B;
        assert_eq!(rect.expect("visible").h, expected.round());
    }

    #[test]
    fn hidden_state_paints_nothing_after_the_exit_motion() {
        let mut galley = Galley::new(FontSet::load().expect("fonts"), &spec());
        let v = view(OverlayPhase::Done { success: true });
        paint_view(&mut galley, &v, 0);
        galley.observe(&OverlayVisibility::Hidden, &spec(), 2_000);
        assert!(
            galley.is_animating(2_000),
            "the exit motion is still running"
        );
        galley.observe(&OverlayVisibility::Hidden, &spec(), 3_000);
        assert!(!galley.is_animating(3_000));
        let (w, h) = buffer_size(DEFAULT_SHEET_WIDTH, 4);
        let mut bytes = vec![0u8; (w * h * 4) as usize];
        let mut frame = Frame {
            bytes: &mut bytes,
            width: w,
            height: h,
        };
        assert!(galley.paint(&mut frame, &spec(), 3_000).is_none());
        assert!(bytes.iter().all(|b| *b == 0));
    }

    #[test]
    fn opacity_scales_the_whole_sheet() {
        let mut galley = Galley::new(FontSet::load().expect("fonts"), &spec());
        let v = view(OverlayPhase::Interim);
        let (_, full) = paint_view(&mut galley, &v, 0);
        let (w, h) = buffer_size(DEFAULT_SHEET_WIDTH, 4);
        let mut bytes = vec![0u8; (w * h * 4) as usize];
        let mut frame = Frame {
            bytes: &mut bytes,
            width: w,
            height: h,
        };
        let dim = SheetSpec {
            opacity: 0.5,
            ..spec()
        };
        galley.observe(&OverlayVisibility::Visible(v.clone()), &dim, 2_500);
        galley.paint(&mut frame, &dim, 2_500);
        let max_full = full.iter().skip(3).step_by(4).max().copied().unwrap_or(0);
        let max_dim = bytes.iter().skip(3).step_by(4).max().copied().unwrap_or(0);
        assert_eq!(max_full, 255);
        assert!((120..=135).contains(&max_dim), "half opacity: {max_dim}");
    }

    #[test]
    fn countdown_replaces_the_timer_after_the_warning() {
        let mut v = view(OverlayPhase::Interim);
        v.warning = Some(CapWarning {
            at_ms: 0,
            remaining_ms: Some(120_000),
        });
        assert_eq!(v.remaining_ms(5_000), Some(115_000));
        assert_eq!(format_remaining(v.remaining_ms(5_000).unwrap()), "-1:55");
    }
}
