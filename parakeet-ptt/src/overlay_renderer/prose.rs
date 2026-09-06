//! The prose column: per-glyph draft/ink state, arrival and set animations,
//! word wrapping and the leading-words clamp.
//!
//! Interim transcript text arrives in bursts. Each burst's new characters are
//! the draft tail, drawn in italic pencil; when the next burst arrives, the
//! previous draft sets into roman ink with a short dip. At finalization
//! everything sets. The engine keeps the common prefix of consecutive texts so
//! settled glyphs never re-animate, and drops leading words behind an ellipsis
//! when the text outgrows the line budget (the newest words always stay).

use super::fonts::{Face, FontSet};

const ARRIVE_MS: f32 = 110.0;
const ARRIVE_STAGGER_MS: u64 = 16;
const ARRIVE_STAGGER_CAP_MS: u64 = 560;
const SET_MS: f32 = 210.0;
const SET_STAGGER_MS: u64 = 10;
const SET_STAGGER_CAP_MS: u64 = 420;
/// The italic face gives way to roman this long into the set animation.
const SET_FACE_SWITCH_MS: u64 = 92;
const ELLIPSIS: &str = "… ";

#[derive(Debug, Clone)]
struct ProseGlyph {
    ch: char,
    draft: bool,
    arrive_at: u64,
    set_at: Option<u64>,
}

/// A glyph placed by `layout`, ready to draw.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Placed {
    pub ch: char,
    pub x: f32,
    pub line: usize,
    pub face: Face,
    /// Pencil colour while drafting, ink once set.
    pub pencil: bool,
    pub alpha: f32,
    pub dy: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Layout {
    pub placed: Vec<Placed>,
    pub lines: usize,
    pub widest: f32,
}

/// How one glyph looks this frame: which face, pencil or ink, its opacity and lift.
#[derive(Debug, Clone, Copy, PartialEq)]
struct GlyphVisual {
    face: Face,
    pencil: bool,
    alpha: f32,
    dy: f32,
}

#[derive(Debug, Default)]
pub(super) struct Prose {
    glyphs: Vec<ProseGlyph>,
    /// Characters dropped from the front of the text by the line clamp.
    dropped: usize,
    prev_text: Vec<char>,
    roman_only: bool,
}

impl Prose {
    pub(super) fn reset(&mut self) {
        self.glyphs.clear();
        self.dropped = 0;
        self.prev_text.clear();
    }

    /// Roman-only prose (the LLM answer) skips the draft tail entirely.
    pub(super) fn set_roman_only(&mut self, roman_only: bool) {
        if self.roman_only != roman_only {
            self.roman_only = roman_only;
            self.reset();
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.glyphs.is_empty()
    }

    /// Feeds the latest full text. Returns true when anything changed.
    pub(super) fn update(&mut self, text: &str, now_ms: u64) -> bool {
        if text.chars().eq(self.prev_text.iter().copied()) {
            return false;
        }
        let chars: Vec<char> = text.chars().collect();
        let mut common = 0;
        let limit = chars.len().min(self.prev_text.len());
        while common < limit && chars[common] == self.prev_text[common] {
            common += 1;
        }
        if common < self.dropped {
            self.reset();
            common = 0;
        } else {
            self.glyphs.truncate(common - self.dropped);
        }
        let appended = chars.len() > common;
        for (offset, &ch) in chars[common..].iter().enumerate() {
            let delay = (offset as u64 * ARRIVE_STAGGER_MS).min(ARRIVE_STAGGER_CAP_MS);
            self.glyphs.push(ProseGlyph {
                ch,
                draft: !self.roman_only,
                arrive_at: now_ms + delay,
                set_at: None,
            });
        }
        if appended {
            let survivors = common.saturating_sub(self.dropped);
            self.commit_range(0, survivors, now_ms);
        }
        self.prev_text = chars;
        true
    }

    /// Sets every remaining draft glyph (finalization).
    pub(super) fn commit_all(&mut self, now_ms: u64) {
        self.commit_range(0, self.glyphs.len(), now_ms);
    }

    fn commit_range(&mut self, start: usize, end: usize, now_ms: u64) {
        let mut first: Option<usize> = None;
        for index in start..end.min(self.glyphs.len()) {
            let glyph = &mut self.glyphs[index];
            if !glyph.draft || glyph.ch == ' ' {
                glyph.draft = false;
                continue;
            }
            let anchor = *first.get_or_insert(index);
            let delay = ((index - anchor) as u64 * SET_STAGGER_MS).min(SET_STAGGER_CAP_MS);
            glyph.set_at = Some(now_ms + delay);
            glyph.draft = false;
        }
    }

    fn visual(&self, glyph: &ProseGlyph, now_ms: u64) -> GlyphVisual {
        let arrive = if now_ms <= glyph.arrive_at {
            0.0
        } else {
            ((now_ms - glyph.arrive_at) as f32 / ARRIVE_MS).min(1.0)
        };
        let base_face = if self.roman_only {
            Face::Roman
        } else {
            Face::Italic
        };
        match glyph.set_at {
            None if glyph.draft => GlyphVisual {
                face: base_face,
                pencil: true,
                alpha: arrive,
                dy: 0.0,
            },
            None => GlyphVisual {
                face: Face::Roman,
                pencil: false,
                alpha: arrive,
                dy: 0.0,
            },
            Some(set_at) => {
                let (alpha, dy) = set_curve(now_ms, set_at);
                let italic = now_ms < set_at + SET_FACE_SWITCH_MS;
                GlyphVisual {
                    face: if italic { Face::Italic } else { Face::Roman },
                    pencil: italic,
                    alpha: arrive * alpha,
                    dy,
                }
            }
        }
    }

    /// Wraps the prose into at most `max_lines` lines of `measure` px, dropping leading
    /// words behind an ellipsis when it does not fit. Positions are relative to the
    /// column origin; the caller multiplies `line` by the line height.
    pub(super) fn layout(
        &mut self,
        fonts: &FontSet,
        px: f32,
        measure: f32,
        max_lines: usize,
        now_ms: u64,
    ) -> Layout {
        let max_lines = max_lines.max(1);
        let layout = self.layout_once(fonts, px, measure, now_ms, 0);
        if layout.lines <= max_lines {
            return layout;
        }
        // Binary-search the smallest leading drop that fits the budget: the
        // empty tail always fits, so the search always lands on a drop that
        // does, and a transcript thousands of words over budget costs a dozen
        // wrap passes rather than one per word. Then prefer the next word
        // boundary when a whole-word tail still fits.
        let fits =
            |skip: usize| self.layout_once(fonts, px, measure, now_ms, skip).lines <= max_lines;
        let (mut lo, mut hi) = (1usize, self.glyphs.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if fits(mid) {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        let mut drop = hi;
        if drop > 0
            && self
                .glyphs
                .get(drop - 1)
                .is_some_and(|glyph| glyph.ch != ' ')
        {
            let word_end = self.glyphs[drop..]
                .iter()
                .position(|glyph| glyph.ch == ' ')
                .map(|at| drop + at + 1);
            if let Some(end) = word_end.filter(|end| *end < self.glyphs.len() && fits(*end)) {
                drop = end;
            }
        }
        self.glyphs.drain(..drop);
        self.dropped += drop;
        self.layout_once(fonts, px, measure, now_ms, 0)
    }

    /// One wrap pass over the glyphs after `skip`, behind an ellipsis when
    /// anything has been dropped.
    fn layout_once(
        &self,
        fonts: &FontSet,
        px: f32,
        measure: f32,
        now_ms: u64,
        skip: usize,
    ) -> Layout {
        let dropped = self.dropped + skip;
        struct Item {
            ch: char,
            face: Face,
            pencil: bool,
            alpha: f32,
            dy: f32,
            advance: f32,
        }
        let mut items: Vec<Item> = Vec::with_capacity(self.glyphs.len() + 2);
        let ellipsis_len = if dropped > 0 {
            ELLIPSIS.chars().count()
        } else {
            0
        };
        if dropped > 0 {
            for ch in ELLIPSIS.chars() {
                items.push(Item {
                    ch,
                    face: Face::Roman,
                    pencil: true,
                    alpha: 1.0,
                    dy: 0.0,
                    advance: fonts.advance(Face::Roman, ch, px),
                });
            }
        }
        for glyph in &self.glyphs[skip.min(self.glyphs.len())..] {
            let GlyphVisual {
                face,
                pencil,
                alpha,
                dy,
            } = self.visual(glyph, now_ms);
            let ch = if glyph.ch == '\n' { ' ' } else { glyph.ch };
            items.push(Item {
                ch,
                face,
                pencil,
                alpha,
                dy,
                advance: fonts.advance(face, ch, px),
            });
        }

        let mut placed = Vec::with_capacity(items.len());
        let mut line = 0usize;
        let mut x = 0.0f32;
        let mut widest = 0.0f32;
        let mut content_end = 0.0f32;
        let mut index = 0;
        while index < items.len() {
            if items[index].ch == ' ' {
                if x > 0.0 {
                    placed.push(Placed {
                        ch: ' ',
                        x,
                        line,
                        face: items[index].face,
                        pencil: items[index].pencil,
                        alpha: items[index].alpha,
                        dy: 0.0,
                    });
                    x += items[index].advance;
                }
                index += 1;
                continue;
            }
            let mut end = index;
            let mut word_width = 0.0;
            while end < items.len() && items[end].ch != ' ' {
                if end > index && items[end - 1].face == items[end].face {
                    word_width += fonts.kern(items[end].face, items[end - 1].ch, items[end].ch, px);
                }
                word_width += items[end].advance;
                end += 1;
            }
            // The first word after the ellipsis stays glued to it and breaks
            // mid-word if it must: the ellipsis alone on a line is wasted
            // budget, and the clamp needs a drop index inside the text.
            if x > 0.0 && index != ellipsis_len && x + word_width > measure {
                widest = widest.max(content_end);
                line += 1;
                x = 0.0;
            }
            for k in index..end {
                if k > index && items[k - 1].face == items[k].face {
                    x += fonts.kern(items[k].face, items[k - 1].ch, items[k].ch, px);
                }
                if x > 0.0 && x + items[k].advance > measure {
                    // A single word wider than the measure breaks mid-word.
                    widest = widest.max(content_end);
                    line += 1;
                    x = 0.0;
                }
                placed.push(Placed {
                    ch: items[k].ch,
                    x,
                    line,
                    face: items[k].face,
                    pencil: items[k].pencil,
                    alpha: items[k].alpha,
                    dy: items[k].dy,
                });
                x += items[k].advance;
                content_end = x;
            }
            index = end;
        }
        widest = widest.max(content_end);
        Layout {
            placed,
            lines: line + 1,
            widest,
        }
    }
}

/// The set keyframes: opacity 1 -> .10 (lifted 1.6 px) at 38 % -> .80 at 62 % -> 1.
fn set_curve(now_ms: u64, set_at: u64) -> (f32, f32) {
    if now_ms <= set_at {
        return (1.0, 0.0);
    }
    let u = ((now_ms - set_at) as f32 / SET_MS).min(1.0);
    let e = u * u * (3.0 - 2.0 * u);
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    if e < 0.38 {
        let t = e / 0.38;
        (lerp(1.0, 0.10, t), lerp(0.0, -1.6, t))
    } else if e < 0.62 {
        let t = (e - 0.38) / 0.24;
        (lerp(0.10, 0.80, t), lerp(-1.6, 0.0, t))
    } else {
        let t = (e - 0.62) / 0.38;
        (lerp(0.80, 1.0, t), 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fonts() -> FontSet {
        FontSet::load().expect("bundled fonts parse")
    }

    fn faces(prose: &mut Prose, fonts: &FontSet, now: u64) -> Vec<(char, Face)> {
        prose
            .layout(fonts, 17.0, 10_000.0, 4, now)
            .placed
            .into_iter()
            .map(|p| (p.ch, p.face))
            .collect()
    }

    #[test]
    fn new_burst_arrives_as_draft_and_previous_burst_sets() {
        let fonts = fonts();
        let mut prose = Prose::default();
        prose.update("hello", 0);
        let first = faces(&mut prose, &fonts, 1_000);
        assert!(first.iter().all(|(_, face)| *face == Face::Italic));

        prose.update("hello world", 2_000);
        let mid = faces(&mut prose, &fonts, 2_600);
        assert!(
            mid[..5].iter().all(|(_, face)| *face == Face::Roman),
            "{mid:?}"
        );
        assert!(
            mid[6..].iter().all(|(_, face)| *face == Face::Italic),
            "{mid:?}"
        );
    }

    #[test]
    fn rewrite_drops_the_changed_tail_and_keeps_the_prefix() {
        let fonts = fonts();
        let mut prose = Prose::default();
        prose.update("send the invoice", 0);
        prose.update("send the invite", 500);
        let text: String = faces(&mut prose, &fonts, 5_000)
            .iter()
            .map(|(ch, _)| *ch)
            .collect();
        assert_eq!(text, "send the invite");
        prose.update("send the invite", 600);
        assert!(
            !prose.update("send the invite", 700),
            "unchanged text is a no-op"
        );
    }

    #[test]
    fn commit_all_sets_every_draft_glyph() {
        let fonts = fonts();
        let mut prose = Prose::default();
        prose.update("okay so", 0);
        prose.commit_all(1_000);
        let after = faces(&mut prose, &fonts, 3_000);
        assert!(after.iter().all(|(_, face)| *face == Face::Roman));
    }

    #[test]
    fn set_curve_dips_then_recovers() {
        assert_eq!(set_curve(0, 0), (1.0, 0.0));
        let (dip, lift) = set_curve(80, 0);
        assert!(dip < 0.5, "mid-set opacity dips: {dip}");
        assert!(lift < 0.0, "mid-set glyph lifts: {lift}");
        let (end, dy) = set_curve(210, 0);
        assert!((end - 1.0).abs() < 1e-4 && dy == 0.0);
    }

    #[test]
    fn wraps_at_the_measure_and_reports_widest_line() {
        let fonts = fonts();
        let mut prose = Prose::default();
        prose.update("one two three four five six seven eight nine ten", 0);
        let layout = prose.layout(&fonts, 17.0, 120.0, 10, 5_000);
        assert!(
            layout.lines >= 3,
            "expected wrapping, got {} lines",
            layout.lines
        );
        assert!(layout.widest <= 120.0);
        assert!(layout.placed.iter().all(|p| p.x + 1.0 <= 120.0));
        let line_starts: Vec<char> = layout
            .placed
            .iter()
            .filter(|p| p.x == 0.0)
            .map(|p| p.ch)
            .collect();
        assert!(
            !line_starts.contains(&' '),
            "wrapped lines do not start with a space"
        );
    }

    #[test]
    fn clamp_drops_leading_words_behind_an_ellipsis() {
        let fonts = fonts();
        let mut prose = Prose::default();
        prose.update(
            "alpha beta gamma delta epsilon zeta eta theta iota kappa",
            0,
        );
        let layout = prose.layout(&fonts, 17.0, 90.0, 2, 5_000);
        assert!(layout.lines <= 2);
        assert_eq!(layout.placed[0].ch, '…');
        let text: String = layout.placed.iter().map(|p| p.ch).collect();
        assert!(text.ends_with("kappa"), "newest words survive: {text}");
        assert!(!text.contains("alpha"));
    }

    #[test]
    fn clamp_settles_a_huge_transcript_in_one_call() {
        let fonts = fonts();
        let mut prose = Prose::default();
        let text = "alpha ".repeat(2_000);
        prose.update(text.trim_end(), 0);
        let layout = prose.layout(&fonts, 17.0, 300.0, 4, 5_000);
        assert!(layout.lines <= 4, "lines: {}", layout.lines);
        assert!(prose.dropped > 10_000, "dropped: {}", prose.dropped);
        // A second call with the same text is stable.
        let again = prose.layout(&fonts, 17.0, 300.0, 4, 5_000);
        assert_eq!(again.lines, layout.lines);
    }

    #[test]
    fn clamp_keeps_the_tail_of_an_oversized_word() {
        let fonts = fonts();
        let mut prose = Prose::default();
        let word = "a".repeat(1_000);
        prose.update(&word, 0);
        let layout = prose.layout(&fonts, 17.0, 200.0, 2, 5_000);
        assert!(layout.lines <= 2);
        let kept = layout.placed.iter().filter(|p| p.ch == 'a').count();
        assert!(kept > 20, "a useful suffix survives: {kept}");
    }

    #[test]
    fn clamp_to_one_line_never_leaves_the_ellipsis_alone_on_a_line() {
        let fonts = fonts();
        let word = "supercalifragilisticexpialidocious".repeat(3);
        let mut probe = Prose::default();
        probe.update(&word, 0);
        let word_width = probe.layout(&fonts, 17.0, 1.0e6, 1, 5_000).widest;

        let mut prose = Prose::default();
        prose.update(&format!("alpha {word}"), 0);
        let layout = prose.layout(&fonts, 17.0, word_width, 1, 5_000);
        assert_eq!(
            layout.lines, 1,
            "the ellipsis shares the line with the tail"
        );
        let kept = layout
            .placed
            .iter()
            .filter(|p| p.ch != '…' && p.ch != ' ')
            .count();
        assert!(kept > 10, "a useful tail survives: {kept}");
    }

    #[test]
    fn roman_only_prose_never_drafts() {
        let fonts = fonts();
        let mut prose = Prose::default();
        prose.set_roman_only(true);
        prose.update("A mutex", 0);
        let placed = faces(&mut prose, &fonts, 10);
        assert!(placed.iter().all(|(_, face)| *face == Face::Roman));
    }
}
