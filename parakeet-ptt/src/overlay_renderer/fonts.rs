//! The Overlay's three bundled faces and a glyph cache over them.
//!
//! Newsreader Regular and Italic set the prose; Fira Code sets the instrument
//! column. The assets are built by `scripts/build-overlay-fonts.py`, which
//! also bakes Newsreader's pair kerning into the legacy `kern` table because
//! fontdue reads nothing else.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use fontdue::{Font, FontSettings, Metrics};

use super::paint::{Frame, Rgb};

const NEWSREADER_REGULAR: &[u8] = include_bytes!("../../assets/fonts/Newsreader-Regular.ttf");
const NEWSREADER_ITALIC: &[u8] = include_bytes!("../../assets/fonts/Newsreader-Italic.ttf");
const FIRA_CODE_REGULAR: &[u8] = include_bytes!("../../assets/fonts/FiraCode-Regular.ttf");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Face {
    Roman,
    Italic,
    Mono,
}

impl Face {
    fn index(self) -> usize {
        match self {
            Self::Roman => 0,
            Self::Italic => 1,
            Self::Mono => 2,
        }
    }
}

#[derive(Debug)]
pub(super) struct GlyphBitmap {
    pub metrics: Metrics,
    pub bitmap: Vec<u8>,
}

/// Rasterised glyphs are keyed by glyph index, so every character a face lacks
/// shares one missing-glyph bitmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphKey {
    face: Face,
    glyph: u16,
    px_bits: u32,
}

/// The cache is cleared once it holds this many bitmaps; a session's worth of
/// prose is a few hundred, so the bound only matters over a long process life.
const GLYPH_CACHE_MAX: usize = 2_048;

/// Face, size, tracking and colour for one run of text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct TextStyle {
    pub face: Face,
    pub px: f32,
    /// Extra advance after each glyph.
    pub tracking: f32,
    pub rgb: Rgb,
}

impl TextStyle {
    pub(super) const fn with_rgb(self, rgb: Rgb) -> Self {
        Self { rgb, ..self }
    }
}

pub(super) struct FontSet {
    fonts: [Font; 3],
    cache: HashMap<GlyphKey, Arc<GlyphBitmap>>,
}

impl FontSet {
    /// Parses the bundled faces. The `scale` hint tells fontdue which size the
    /// outlines are optimised for; it is the size each face is drawn at.
    pub(super) fn load() -> Result<Self> {
        let load = |bytes: &'static [u8], scale: f32, name: &str| -> Result<Font> {
            Font::from_bytes(
                bytes,
                FontSettings {
                    scale,
                    ..FontSettings::default()
                },
            )
            .map_err(|err| anyhow::anyhow!("{err}"))
            .with_context(|| format!("bundled font {name} failed to parse"))
        };
        Ok(Self {
            fonts: [
                load(NEWSREADER_REGULAR, 17.0, "Newsreader-Regular")?,
                load(NEWSREADER_ITALIC, 17.0, "Newsreader-Italic")?,
                load(FIRA_CODE_REGULAR, 11.0, "FiraCode-Regular")?,
            ],
            cache: HashMap::new(),
        })
    }

    fn font(&self, face: Face) -> &Font {
        &self.fonts[face.index()]
    }

    pub(super) fn advance(&self, face: Face, ch: char, px: f32) -> f32 {
        self.font(face).metrics(ch, px).advance_width
    }

    pub(super) fn kern(&self, face: Face, left: char, right: char, px: f32) -> f32 {
        self.font(face)
            .horizontal_kern(left, right, px)
            .unwrap_or(0.0)
    }

    pub(super) fn glyph(&mut self, face: Face, ch: char, px: f32) -> Arc<GlyphBitmap> {
        let index = self.font(face).lookup_glyph_index(ch);
        let key = GlyphKey {
            face,
            glyph: index,
            px_bits: px.to_bits(),
        };
        if let Some(glyph) = self.cache.get(&key) {
            return Arc::clone(glyph);
        }
        if self.cache.len() >= GLYPH_CACHE_MAX {
            self.cache.clear();
        }
        let (metrics, bitmap) = self.font(face).rasterize_indexed(index, px);
        let glyph = Arc::new(GlyphBitmap { metrics, bitmap });
        self.cache.insert(key, Arc::clone(&glyph));
        glyph
    }

    /// Width of `text` set in one face with `tracking` px added after each glyph.
    pub(super) fn measure(&self, face: Face, px: f32, text: &str, tracking: f32) -> f32 {
        let mut width = 0.0;
        let mut prev: Option<char> = None;
        for ch in text.chars() {
            if let Some(left) = prev {
                width += self.kern(face, left, ch, px);
            }
            width += self.advance(face, ch, px) + tracking;
            prev = Some(ch);
        }
        width
    }

    /// Draws `text` in `style` from `x` on `baseline`; returns the x after the last glyph.
    pub(super) fn draw_text(
        &mut self,
        frame: &mut Frame,
        style: TextStyle,
        x: f32,
        baseline: f32,
        text: &str,
        alpha: f32,
    ) -> f32 {
        let TextStyle {
            face,
            px,
            tracking,
            rgb,
        } = style;
        let mut cursor = x;
        let mut prev: Option<char> = None;
        for ch in text.chars() {
            if let Some(left) = prev {
                cursor += self.kern(face, left, ch, px);
            }
            let glyph = self.glyph(face, ch, px);
            self.draw_glyph(frame, &glyph, cursor, baseline, rgb, alpha);
            cursor += glyph.metrics.advance_width + tracking;
            prev = Some(ch);
        }
        cursor
    }

    /// Width of `text` set in `style`.
    pub(super) fn measure_styled(&self, style: TextStyle, text: &str) -> f32 {
        self.measure(style.face, style.px, text, style.tracking)
    }

    /// Blits one rasterised glyph with its left edge at `x` on `baseline`.
    pub(super) fn draw_glyph(
        &self,
        frame: &mut Frame,
        glyph: &GlyphBitmap,
        x: f32,
        baseline: f32,
        rgb: Rgb,
        alpha: f32,
    ) {
        let m = &glyph.metrics;
        if m.width == 0 || m.height == 0 {
            return;
        }
        let gx = x.round() as i32 + m.xmin;
        let gy = baseline.round() as i32 - m.height as i32 - m.ymin;
        frame.blend_bitmap((gx, gy), (m.width, m.height), &glyph.bitmap, rgb, alpha);
    }

    /// Truncates `text` with an ellipsis so it fits `max_width` in one face.
    pub(super) fn fit_with_ellipsis(
        &self,
        face: Face,
        px: f32,
        text: &str,
        max_width: f32,
    ) -> String {
        if self.measure(face, px, text, 0.0) <= max_width {
            return text.to_string();
        }
        let ellipsis_width = self.measure(face, px, "…", 0.0);
        let mut out = String::new();
        let mut width = 0.0;
        let mut prev: Option<char> = None;
        for ch in text.chars() {
            let mut step = self.advance(face, ch, px);
            if let Some(left) = prev {
                step += self.kern(face, left, ch, px);
            }
            if width + step + ellipsis_width > max_width {
                break;
            }
            out.push(ch);
            width += step;
            prev = Some(ch);
        }
        let trimmed = out.trim_end();
        format!("{trimmed}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_faces_load_and_kern() {
        let fonts = FontSet::load().expect("bundled fonts parse");
        assert!(fonts.advance(Face::Roman, 'm', 17.0) > fonts.advance(Face::Roman, 'i', 17.0));
        assert!(
            fonts.kern(Face::Roman, 'T', 'o', 17.0) < 0.0,
            "Newsreader pair kerning should be baked into the kern table"
        );
        let mono_a = fonts.advance(Face::Mono, 'a', 11.0);
        let mono_m = fonts.advance(Face::Mono, 'm', 11.0);
        assert!((mono_a - mono_m).abs() < 0.01, "Fira Code is monospaced");
    }

    #[test]
    fn fit_with_ellipsis_respects_the_width() {
        let fonts = FontSet::load().expect("bundled fonts parse");
        let text = "what is the difference between a mutex and a semaphore";
        let fitted = fonts.fit_with_ellipsis(Face::Italic, 13.5, text, 120.0);
        assert!(fitted.ends_with('…'));
        assert!(fonts.measure(Face::Italic, 13.5, &fitted, 0.0) <= 120.0);
        assert_eq!(
            fonts.fit_with_ellipsis(Face::Italic, 13.5, "short", 120.0),
            "short"
        );
    }

    #[test]
    fn glyph_cache_is_bounded_and_shares_the_missing_glyph() {
        let mut fonts = FontSet::load().expect("bundled fonts parse");
        let missing_a = fonts.glyph(Face::Roman, '\u{E000}', 17.0);
        let missing_b = fonts.glyph(Face::Roman, '\u{E001}', 17.0);
        assert!(Arc::ptr_eq(&missing_a, &missing_b));
        for code in 0x4E00..0x4E00 + 3 * GLYPH_CACHE_MAX as u32 {
            let ch = char::from_u32(code).expect("cjk char");
            // Distinct sizes defeat glyph-index sharing for characters the face lacks.
            fonts.glyph(Face::Roman, ch, 17.0 + (code % 512) as f32);
        }
        assert!(fonts.cache.len() <= GLYPH_CACHE_MAX);
    }
}
