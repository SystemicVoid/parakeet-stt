use std::collections::VecDeque;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use fontdb::{Database, Family, Query, Source};
use fontdue::{Font, FontSettings};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use parakeet_ptt::overlay_ipc::OverlayIpcMessage;
use parakeet_ptt::overlay_state::{
    ApplyOutcome, OverlayRenderIntent, OverlayRenderPhase, OverlayStateMachine, OverlayVisibility,
};

const FALLBACK_WINDOW_TITLE: &str = "Parakeet Overlay";
const LAYER_NAMESPACE: &str = "parakeet-overlay";
const DEFAULT_FONT_FAMILY: &str = "Sans";
const DEFAULT_FONT_SIZE_PX: f32 = 18.0;
const FONT_SIZE_RANGE: std::ops::RangeInclusive<f32> = 10.0..=64.0;
const LINE_HEIGHT_FACTOR: f32 = 1.45;

// --- Design System ---
const BG_COLOR: (u8, u8, u8) = (22, 22, 26);
const BG_ALPHA: u8 = 230; // ~90%
const BORDER_COLOR: (u8, u8, u8) = (58, 58, 68);
const BORDER_ALPHA: u8 = 255;
const BORDER_THICKNESS: f32 = 1.0;
const CORNER_RADIUS: f32 = 12.0;
const SHADOW_RADIUS: u32 = 8;
const SHADOW_ALPHA: u8 = 80; // ~31%
const TEXT_COLOR_RGB: (u8, u8, u8) = (245, 245, 250);
const TEXT_SHADOW_COLOR: [u8; 4] = [0, 0, 0, 60];
const PADDING_H: u32 = 24;
const PADDING_V: u32 = 16;
const PADDING_LEFT: u32 = 32;
const ACCENT_STRIPE_WIDTH: f32 = 3.0;
const ACCENT_STRIPE_MARGIN: f32 = 6.0;
const FADE_DURATION_MS: u64 = 250;

const PREFERRED_FONTS: &[&str] = &["Inter", "Cantarell", "Noto Sans"];

#[derive(Parser, Debug)]
#[command(
    name = "parakeet-overlay",
    version,
    about = "Parakeet overlay renderer process (Phase 4 MVP)"
)]
struct Cli {
    /// Rendering backend mode: auto, layer-shell, or fallback-window
    #[arg(long, value_enum, default_value_t = CliBackendMode::Auto)]
    backend: CliBackendMode,

    /// Auto-hide delay after session end.
    #[arg(long, default_value_t = 1200)]
    auto_hide_ms: u64,

    /// Overlay opacity (0.0-1.0).
    #[arg(long, default_value_t = 0.92)]
    opacity: f32,

    /// Font descriptor used for text rendering.
    #[arg(long, default_value = "Sans 16")]
    font: String,

    /// Screen anchor for overlay placement.
    #[arg(long, value_enum, default_value_t = CliAnchor::TopCenter)]
    anchor: CliAnchor,

    /// Horizontal margin from anchor reference point.
    #[arg(long, default_value_t = 24)]
    margin_x: u32,

    /// Vertical margin from anchor reference point.
    #[arg(long, default_value_t = 24)]
    margin_y: u32,

    /// Maximum text box width in pixels.
    #[arg(long, default_value_t = 960)]
    max_width: u32,

    /// Maximum rendered lines.
    #[arg(long, default_value_t = 4)]
    max_lines: u32,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum CliBackendMode {
    Auto,
    LayerShell,
    FallbackWindow,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum CliAnchor {
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

// --- Geometry & Rendering Primitives ---

#[derive(Debug, Clone, Copy)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

#[derive(Debug, Clone, Copy)]
struct ContentArea {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

fn argb_pixel_premul(r: u8, g: u8, b: u8, a: u8) -> [u8; 4] {
    let aa = u16::from(a);
    [
        ((u16::from(b) * aa) / 255) as u8,
        ((u16::from(g) * aa) / 255) as u8,
        ((u16::from(r) * aa) / 255) as u8,
        a,
    ]
}

fn blend_pixel(frame: &mut [u8], dims: SurfaceDimensions, x: i32, y: i32, color: [u8; 4]) {
    if x < 0 || y < 0 || x >= dims.width as i32 || y >= dims.height as i32 {
        return;
    }
    let sa = color[3];
    if sa == 0 {
        return;
    }
    let idx = ((y as u32 * dims.width + x as u32) * 4) as usize;
    if idx + 3 >= frame.len() {
        return;
    }
    // Source is premultiplied, destination is premultiplied.
    let inv = 255 - u16::from(sa);
    for ch in 0..3 {
        frame[idx + ch] = (u16::from(color[ch]) + (u16::from(frame[idx + ch]) * inv) / 255) as u8;
    }
    frame[idx + 3] = (u16::from(sa) + (u16::from(frame[idx + 3]) * inv) / 255) as u8;
}

/// Returns coverage 0.0–1.0 for a pixel at (px, py) against a rounded rect.
fn rounded_rect_coverage(px: f32, py: f32, rect: Rect, radius: f32) -> f32 {
    let r = radius.min(rect.w / 2.0).min(rect.h / 2.0);
    // Check if outside bounding box
    if px < rect.x || px > rect.x + rect.w || py < rect.y || py > rect.y + rect.h {
        return 0.0;
    }
    // Check corner regions
    let corners = [
        (rect.x + r, rect.y + r),                   // top-left
        (rect.x + rect.w - r, rect.y + r),          // top-right
        (rect.x + r, rect.y + rect.h - r),          // bottom-left
        (rect.x + rect.w - r, rect.y + rect.h - r), // bottom-right
    ];
    for &(cx, cy) in &corners {
        let in_corner_x = (px < rect.x + r && (cx == corners[0].0 || cx == corners[2].0))
            || (px > rect.x + rect.w - r && (cx == corners[1].0 || cx == corners[3].0));
        let in_corner_y = (py < rect.y + r && (cy == corners[0].1 || cy == corners[1].1))
            || (py > rect.y + rect.h - r && (cy == corners[2].1 || cy == corners[3].1));
        if in_corner_x && in_corner_y {
            let dx = px - cx;
            let dy = py - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist > r + 0.5 {
                return 0.0;
            }
            if dist < r - 0.5 {
                return 1.0;
            }
            return (r + 0.5 - dist).clamp(0.0, 1.0);
        }
    }
    1.0
}

fn fill_rounded_rect(
    frame: &mut [u8],
    dims: SurfaceDimensions,
    rect: Rect,
    radius: f32,
    color: [u8; 4],
) {
    let base_alpha = u16::from(color[3]);
    let x0 = (rect.x.floor() as i32).max(0);
    let y0 = (rect.y.floor() as i32).max(0);
    let x1 = ((rect.x + rect.w).ceil() as i32).min(dims.width as i32);
    let y1 = ((rect.y + rect.h).ceil() as i32).min(dims.height as i32);

    for py in y0..y1 {
        for px in x0..x1 {
            let cov = rounded_rect_coverage(px as f32 + 0.5, py as f32 + 0.5, rect, radius);
            if cov <= 0.0 {
                continue;
            }
            let a = ((base_alpha * (cov * 255.0) as u16) / 255).min(255) as u8;
            blend_pixel(
                frame,
                dims,
                px,
                py,
                argb_pixel_premul(color[2], color[1], color[0], a),
            );
        }
    }
}

fn stroke_rounded_rect(
    frame: &mut [u8],
    dims: SurfaceDimensions,
    rect: Rect,
    radius: f32,
    thickness: f32,
    color: [u8; 4],
) {
    let inner = Rect {
        x: rect.x + thickness,
        y: rect.y + thickness,
        w: rect.w - thickness * 2.0,
        h: rect.h - thickness * 2.0,
    };
    let inner_r = (radius - thickness).max(0.0);
    let base_alpha = u16::from(color[3]);
    let x0 = (rect.x.floor() as i32).max(0);
    let y0 = (rect.y.floor() as i32).max(0);
    let x1 = ((rect.x + rect.w).ceil() as i32).min(dims.width as i32);
    let y1 = ((rect.y + rect.h).ceil() as i32).min(dims.height as i32);

    for py in y0..y1 {
        for px in x0..x1 {
            let fp = (px as f32 + 0.5, py as f32 + 0.5);
            let outer_cov = rounded_rect_coverage(fp.0, fp.1, rect, radius);
            if outer_cov <= 0.0 {
                continue;
            }
            let inner_cov = rounded_rect_coverage(fp.0, fp.1, inner, inner_r);
            let border_cov = (outer_cov - inner_cov).clamp(0.0, 1.0);
            if border_cov <= 0.0 {
                continue;
            }
            let a = ((base_alpha * (border_cov * 255.0) as u16) / 255).min(255) as u8;
            blend_pixel(
                frame,
                dims,
                px,
                py,
                argb_pixel_premul(color[2], color[1], color[0], a),
            );
        }
    }
}

fn draw_shadow(
    frame: &mut [u8],
    dims: SurfaceDimensions,
    content: ContentArea,
    radius: f32,
    shadow_radius: u32,
    shadow_alpha: u8,
) {
    let sr = shadow_radius as f32;
    let content_rect = Rect {
        x: content.x as f32,
        y: content.y as f32,
        w: content.width as f32,
        h: content.height as f32,
    };
    let expand = sr;
    let shadow_rect = Rect {
        x: content_rect.x - expand,
        y: content_rect.y - expand,
        w: content_rect.w + expand * 2.0,
        h: content_rect.h + expand * 2.0,
    };
    let x0 = (shadow_rect.x.floor() as i32).max(0);
    let y0 = (shadow_rect.y.floor() as i32).max(0);
    let x1 = ((shadow_rect.x + shadow_rect.w).ceil() as i32).min(dims.width as i32);
    let y1 = ((shadow_rect.y + shadow_rect.h).ceil() as i32).min(dims.height as i32);

    for py in y0..y1 {
        for px in x0..x1 {
            let fp = (px as f32 + 0.5, py as f32 + 0.5);
            // If inside the content rect, skip (will be painted by background fill)
            let inside = rounded_rect_coverage(fp.0, fp.1, content_rect, radius);
            if inside >= 1.0 {
                continue;
            }
            // Compute distance to the content rect edge
            let dist = distance_to_rounded_rect(fp.0, fp.1, content_rect, radius);
            if dist >= sr {
                continue;
            }
            let falloff = 1.0 - dist / sr;
            let alpha = (f32::from(shadow_alpha) * falloff * falloff).round() as u8;
            if alpha == 0 {
                continue;
            }
            blend_pixel(frame, dims, px, py, argb_pixel_premul(0, 0, 0, alpha));
        }
    }
}

fn distance_to_rounded_rect(px: f32, py: f32, rect: Rect, radius: f32) -> f32 {
    let r = radius.min(rect.w / 2.0).min(rect.h / 2.0);
    // Clamp to the inner rectangle (inset by radius)
    let inner_x0 = rect.x + r;
    let inner_x1 = rect.x + rect.w - r;
    let inner_y0 = rect.y + r;
    let inner_y1 = rect.y + rect.h - r;

    let cx = px.clamp(inner_x0, inner_x1);
    let cy = py.clamp(inner_y0, inner_y1);

    let dx = px - cx;
    let dy = py - cy;
    let corner_dist = (dx * dx + dy * dy).sqrt();

    // If we're in a corner zone
    if (px < inner_x0 || px > inner_x1) && (py < inner_y0 || py > inner_y1) {
        (corner_dist - r).max(0.0)
    } else {
        // Straight edge — distance to nearest edge
        let dist_left = (px - rect.x).abs();
        let dist_right = (px - (rect.x + rect.w)).abs();
        let dist_top = (py - rect.y).abs();
        let dist_bottom = (py - (rect.y + rect.h)).abs();
        let min_edge = dist_left.min(dist_right).min(dist_top).min(dist_bottom);
        // If inside, return 0
        if px >= rect.x && px <= rect.x + rect.w && py >= rect.y && py <= rect.y + rect.h {
            0.0
        } else {
            min_edge
        }
    }
}

fn draw_accent_stripe(
    frame: &mut [u8],
    dims: SurfaceDimensions,
    content: ContentArea,
    stripe_width: f32,
    margin: f32,
    color: [u8; 4],
    fade_alpha: f32,
) {
    let sw = stripe_width;
    let half = sw / 2.0;
    let x_center = content.x as f32 + PADDING_LEFT as f32 - sw - margin;
    let y_top = content.y as f32 + PADDING_V as f32 + half;
    let y_bottom = (content.y + content.height) as f32 - PADDING_V as f32 - half;
    if y_bottom <= y_top {
        return;
    }

    let base_alpha = (u16::from(color[3]) as f32 * fade_alpha).round() as u16;
    let x0 = ((x_center - half).floor() as i32).max(0);
    let x1 = ((x_center + half).ceil() as i32).min(dims.width as i32);
    let y0 = ((y_top - half).floor() as i32).max(0);
    let y1 = ((y_bottom + half).ceil() as i32).min(dims.height as i32);

    for py in y0..y1 {
        for px in x0..x1 {
            let fp = (px as f32 + 0.5, py as f32 + 0.5);
            let dx = (fp.0 - x_center).abs();
            if dx > half + 0.5 {
                continue;
            }
            // Pill caps
            let cov = if fp.1 < y_top {
                let dy = y_top - fp.1;
                let dist = (dx * dx + dy * dy).sqrt();
                if dist > half + 0.5 {
                    continue;
                }
                (half + 0.5 - dist).clamp(0.0, 1.0)
            } else if fp.1 > y_bottom {
                let dy = fp.1 - y_bottom;
                let dist = (dx * dx + dy * dy).sqrt();
                if dist > half + 0.5 {
                    continue;
                }
                (half + 0.5 - dist).clamp(0.0, 1.0)
            } else {
                // Side edges AA
                (half + 0.5 - dx).clamp(0.0, 1.0)
            };
            if cov <= 0.0 {
                continue;
            }
            let a = ((base_alpha * (cov * 255.0) as u16) / 255).min(255) as u8;
            blend_pixel(
                frame,
                dims,
                px,
                py,
                argb_pixel_premul(color[2], color[1], color[0], a),
            );
        }
    }
}

fn accent_color_for_phase(phase: OverlayRenderPhase) -> Option<[u8; 4]> {
    match phase {
        OverlayRenderPhase::Hidden => None,
        OverlayRenderPhase::Listening => Some([244, 133, 66, 255]), // ARGB stored as [B, G, R, A] via argb_pixel
        OverlayRenderPhase::Interim => Some([137, 199, 52, 255]),
        OverlayRenderPhase::Finalizing => Some([64, 179, 255, 255]),
    }
}

fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let inv = 1.0 - t;
    1.0 - inv * inv * inv
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FadeDirection {
    In,
    Out,
}

#[derive(Debug, Clone, Copy)]
struct FadeState {
    direction: FadeDirection,
    started_ms: u64,
    duration_ms: u64,
}

impl FadeState {
    fn progress(&self, now_ms: u64) -> f32 {
        let elapsed = now_ms.saturating_sub(self.started_ms) as f32;
        let t = (elapsed / self.duration_ms.max(1) as f32).clamp(0.0, 1.0);
        match self.direction {
            FadeDirection::In => ease_out_cubic(t),
            FadeDirection::Out => 1.0 - ease_out_cubic(t),
        }
    }

    fn is_complete(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.started_ms) >= self.duration_ms
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendKind {
    LayerShell,
    FallbackWindow,
    Noop,
}

#[derive(Debug, Clone)]
struct OverlayUiConfig {
    opacity: f32,
    font: String,
    anchor: CliAnchor,
    margin_x: u32,
    margin_y: u32,
    max_width: u32,
    max_lines: u32,
}

impl OverlayUiConfig {
    fn surface_dimensions(&self) -> SurfaceDimensions {
        let content_width = self.max_width.clamp(320, 3840);
        let clamped_lines = self.max_lines.clamp(1, 10);
        let line_h = (DEFAULT_FONT_SIZE_PX * LINE_HEIGHT_FACTOR).ceil() as u32;
        let content_height = (PADDING_V * 2 + clamped_lines * line_h).clamp(72, 720);
        // Expand for shadow
        let shadow_pad = SHADOW_RADIUS * 2;
        SurfaceDimensions {
            width: content_width + shadow_pad,
            height: content_height + shadow_pad,
        }
    }

    fn content_area(&self) -> ContentArea {
        let content_width = self.max_width.clamp(320, 3840);
        let clamped_lines = self.max_lines.clamp(1, 10);
        let line_h = (DEFAULT_FONT_SIZE_PX * LINE_HEIGHT_FACTOR).ceil() as u32;
        let content_height = (PADDING_V * 2 + clamped_lines * line_h).clamp(72, 720);
        ContentArea {
            x: SHADOW_RADIUS,
            y: SHADOW_RADIUS,
            width: content_width,
            height: content_height,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct SurfaceDimensions {
    width: u32,
    height: u32,
}

#[derive(Debug, Clone, PartialEq)]
struct ParsedFontDescriptor {
    family: String,
    size_px: f32,
    fallback_reason: Option<String>,
}

#[derive(Debug, Clone)]
struct FontResolutionSummary {
    requested: String,
    family: String,
    size_px: f32,
    fallback_reason: Option<String>,
}

struct TextRenderer {
    summary: FontResolutionSummary,
    font: Option<Font>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GenericFamilyKind {
    SansSerif,
    Serif,
    Monospace,
    Cursive,
    Fantasy,
}

impl GenericFamilyKind {
    fn label(self) -> &'static str {
        match self {
            Self::SansSerif => "sans-serif",
            Self::Serif => "serif",
            Self::Monospace => "monospace",
            Self::Cursive => "cursive",
            Self::Fantasy => "fantasy",
        }
    }

    fn as_fontdb_family(self) -> Family<'static> {
        match self {
            Self::SansSerif => Family::SansSerif,
            Self::Serif => Family::Serif,
            Self::Monospace => Family::Monospace,
            Self::Cursive => Family::Cursive,
            Self::Fantasy => Family::Fantasy,
        }
    }
}

const GENERIC_FALLBACK_ORDER: [GenericFamilyKind; 3] = [
    GenericFamilyKind::SansSerif,
    GenericFamilyKind::Serif,
    GenericFamilyKind::Monospace,
];

impl TextRenderer {
    fn new(raw_descriptor: &str) -> Self {
        let parsed = parse_font_descriptor(raw_descriptor);
        let user_specified_font = parsed.fallback_reason.is_none();
        let mut db = Database::new();
        db.load_system_fonts();
        let mut fallback_reasons = Vec::new();
        if let Some(reason) = parsed.fallback_reason.clone() {
            fallback_reasons.push(reason);
        }

        let mut selected_family = parsed.family.clone();
        let mut font = load_font_from_database(&db, &selected_family);

        // Try preferred font cascade only when user hasn't overridden --font
        if font.is_none() && !user_specified_font {
            for preferred in PREFERRED_FONTS {
                font = load_font_from_database(&db, preferred);
                if font.is_some() {
                    selected_family = (*preferred).to_string();
                    fallback_reasons.push(format!("using_preferred_font:{preferred}"));
                    break;
                }
            }
        }

        if font.is_none() {
            fallback_reasons.push(format!("font_unavailable:{selected_family}"));

            if let Some(requested_generic) = parse_generic_family_kind(&selected_family) {
                selected_family = requested_generic.label().to_string();
                font = load_font_from_generic(&db, requested_generic);
                if font.is_some() {
                    fallback_reasons.push(format!(
                        "using_requested_generic_family:{}",
                        requested_generic.label()
                    ));
                }
            }
        }

        if font.is_none() {
            for fallback_family in GENERIC_FALLBACK_ORDER {
                font = load_font_from_generic(&db, fallback_family);
                if font.is_some() {
                    selected_family = fallback_family.label().to_string();
                    fallback_reasons.push(format!(
                        "using_fallback_generic_family:{}",
                        fallback_family.label()
                    ));
                    break;
                }
            }
        }

        if font.is_none() {
            font = load_any_font_from_database(&db);
            if font.is_some() {
                selected_family = "system-default".to_string();
                fallback_reasons.push("using_first_available_system_font".to_string());
            }
        }

        if font.is_none() {
            fallback_reasons.push("font_resolution_failed:text_render_disabled".to_string());
        }

        let fallback_reason = if fallback_reasons.is_empty() {
            None
        } else {
            Some(fallback_reasons.join(";"))
        };
        let summary = FontResolutionSummary {
            requested: raw_descriptor.to_string(),
            family: selected_family,
            size_px: parsed.size_px,
            fallback_reason,
        };

        info!(
            requested_font = %summary.requested,
            resolved_font_family = %summary.family,
            resolved_font_size_px = summary.size_px,
            fallback_reason = summary.fallback_reason.as_deref().unwrap_or("none"),
            "overlay font resolved"
        );

        Self { summary, font }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_headline(
        &self,
        frame: &mut [u8],
        dimensions: SurfaceDimensions,
        content: ContentArea,
        max_width_px: u32,
        max_lines: u32,
        text: &str,
        fade_alpha: f32,
    ) {
        let Some(font) = &self.font else {
            return;
        };

        let text_area_width = max_width_px
            .clamp(64, content.width)
            .saturating_sub(PADDING_LEFT + PADDING_H);
        let line_limit = max_lines.clamp(1, 10);
        let lines = layout_text_lines(text, text_area_width, line_limit, |character| {
            font.metrics(character, self.summary.size_px).advance_width
        });
        if lines.is_empty() {
            return;
        }

        let line_height = (self.summary.size_px * LINE_HEIGHT_FACTOR).ceil() as i32;
        let baseline_start =
            content.y as i32 + PADDING_V as i32 + self.summary.size_px.ceil() as i32;
        let text_x_start = content.x as i32 + PADDING_LEFT as i32;
        let text_alpha = (fade_alpha * 255.0).round() as u8;
        let text_color_premul = argb_pixel_premul(
            TEXT_COLOR_RGB.0,
            TEXT_COLOR_RGB.1,
            TEXT_COLOR_RGB.2,
            text_alpha,
        );
        let shadow_alpha = ((TEXT_SHADOW_COLOR[3] as f32) * fade_alpha).round() as u8;
        let text_shadow_premul = argb_pixel_premul(0, 0, 0, shadow_alpha);

        let mut baseline = baseline_start;
        for line in lines {
            let mut cursor_x = text_x_start as f32;
            for character in line.chars() {
                let (metrics, bitmap) = font.rasterize(character, self.summary.size_px);
                let glyph_x = cursor_x.floor() as i32 + metrics.xmin;
                let glyph_y = baseline - metrics.height as i32 - metrics.ymin;
                // Shadow pass (+0, +1)
                blend_bitmap(
                    frame,
                    dimensions,
                    (glyph_x, glyph_y + 1),
                    (metrics.width, metrics.height),
                    &bitmap,
                    text_shadow_premul,
                );
                // Main pass
                blend_bitmap(
                    frame,
                    dimensions,
                    (glyph_x, glyph_y),
                    (metrics.width, metrics.height),
                    &bitmap,
                    text_color_premul,
                );
                cursor_x += metrics.advance_width;
            }

            baseline += line_height;
            if baseline > (content.y + content.height) as i32 {
                break;
            }
        }
    }
}

trait OverlayBackend {
    fn render(&mut self, state: &OverlayVisibility) -> Result<()>;
    fn is_fading(&self) -> bool {
        false
    }
}

#[derive(Debug)]
struct NoopBackend {
    reason: String,
}

impl OverlayBackend for NoopBackend {
    fn render(&mut self, state: &OverlayVisibility) -> Result<()> {
        debug!(reason = %self.reason, ?state, "overlay renderer running in noop mode");
        Ok(())
    }
}

struct WaylandOverlayBackend {
    kind: BackendKind,
    ui: OverlayUiConfig,
    runtime: WaylandRuntime,
    last_visible: bool,
    fade: Option<FadeState>,
    started: Instant,
}

impl WaylandOverlayBackend {
    fn new(kind: BackendKind, ui: OverlayUiConfig, runtime: WaylandRuntime) -> Self {
        Self {
            kind,
            ui,
            runtime,
            last_visible: false,
            fade: None,
            started: Instant::now(),
        }
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn fade_alpha(&self) -> f32 {
        match &self.fade {
            Some(fade) => fade.progress(self.now_ms()),
            None => {
                if self.last_visible {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }

    fn is_fading(&self) -> bool {
        self.fade
            .as_ref()
            .is_some_and(|f| !f.is_complete(self.now_ms()))
    }
}

impl OverlayBackend for WaylandOverlayBackend {
    fn render(&mut self, state: &OverlayVisibility) -> Result<()> {
        let intent = state.to_render_intent();
        let now = self.now_ms();

        // Detect visibility transitions
        if intent.visible != self.last_visible {
            self.fade = Some(FadeState {
                direction: if intent.visible {
                    FadeDirection::In
                } else {
                    FadeDirection::Out
                },
                started_ms: now,
                duration_ms: FADE_DURATION_MS,
            });
            self.last_visible = intent.visible;
        }

        // Clean up completed fades
        if let Some(fade) = &self.fade {
            if fade.is_complete(now) {
                self.fade = None;
            }
        }

        let fade_alpha = self.fade_alpha();
        self.runtime
            .render_with_fade(&intent, &self.ui, fade_alpha)
            .with_context(|| format!("overlay renderer backend failed for {:?}", self.kind))
    }

    fn is_fading(&self) -> bool {
        WaylandOverlayBackend::is_fading(self)
    }
}

struct BuiltBackend {
    kind: BackendKind,
    reason: String,
    backend: Box<dyn OverlayBackend + Send>,
}

#[derive(Debug, Clone, Copy, Default)]
struct BackendSignals {
    has_layer_shell: bool,
    has_wl_compositor: bool,
    has_xdg_wm_base: bool,
    has_wl_shm: bool,
}

impl BackendSignals {
    fn supports_layer_shell(self) -> bool {
        self.has_layer_shell && self.has_wl_compositor && self.has_wl_shm
    }

    fn supports_fallback_window(self) -> bool {
        self.has_wl_compositor && self.has_xdg_wm_base && self.has_wl_shm
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BackendSelection {
    LayerShell,
    FallbackWindow,
    Noop { reason: String },
}

fn resolve_backend_selection(
    mode: CliBackendMode,
    probe: std::result::Result<BackendSignals, String>,
) -> BackendSelection {
    let signals = match probe {
        Ok(signals) => signals,
        Err(err) => {
            return BackendSelection::Noop {
                reason: format!("wayland_probe_failed:{err}"),
            };
        }
    };

    match mode {
        CliBackendMode::Auto => {
            if signals.supports_layer_shell() {
                BackendSelection::LayerShell
            } else if signals.supports_fallback_window() {
                BackendSelection::FallbackWindow
            } else {
                BackendSelection::Noop {
                    reason: "unsupported_wayland_backend:auto".to_string(),
                }
            }
        }
        CliBackendMode::LayerShell => {
            if signals.supports_layer_shell() {
                BackendSelection::LayerShell
            } else {
                BackendSelection::Noop {
                    reason: "unsupported_wayland_backend:layer_shell".to_string(),
                }
            }
        }
        CliBackendMode::FallbackWindow => {
            if signals.supports_fallback_window() {
                BackendSelection::FallbackWindow
            } else {
                BackendSelection::Noop {
                    reason: "unsupported_wayland_backend:fallback_window".to_string(),
                }
            }
        }
    }
}

fn build_backend(mode: CliBackendMode, ui: OverlayUiConfig) -> BuiltBackend {
    let probe_result = probe_backend_signals().map_err(|err| err.to_string());
    let selection = resolve_backend_selection(mode, probe_result);

    match selection {
        BackendSelection::LayerShell => match WaylandRuntime::new(BackendKind::LayerShell, &ui) {
            Ok(runtime) => BuiltBackend {
                kind: BackendKind::LayerShell,
                reason: "layer_shell".to_string(),
                backend: Box::new(WaylandOverlayBackend::new(
                    BackendKind::LayerShell,
                    ui,
                    runtime,
                )),
            },
            Err(layer_err) => {
                if matches!(mode, CliBackendMode::Auto) {
                    match WaylandRuntime::new(BackendKind::FallbackWindow, &ui) {
                            Ok(runtime) => BuiltBackend {
                                kind: BackendKind::FallbackWindow,
                                reason: format!(
                                    "layer_shell_init_failed:{layer_err};using_fallback_window"
                                ),
                                backend: Box::new(WaylandOverlayBackend::new(
                                    BackendKind::FallbackWindow,
                                    ui,
                                    runtime,
                                )),
                            },
                            Err(fallback_err) => BuiltBackend {
                                kind: BackendKind::Noop,
                                reason: format!(
                                    "layer_shell_init_failed:{layer_err};fallback_init_failed:{fallback_err}"
                                ),
                                backend: Box::new(NoopBackend {
                                    reason: "runtime_backend_init_failed".to_string(),
                                }),
                            },
                        }
                } else {
                    BuiltBackend {
                        kind: BackendKind::Noop,
                        reason: format!("layer_shell_init_failed:{layer_err}"),
                        backend: Box::new(NoopBackend {
                            reason: "runtime_backend_init_failed".to_string(),
                        }),
                    }
                }
            }
        },
        BackendSelection::FallbackWindow => {
            match WaylandRuntime::new(BackendKind::FallbackWindow, &ui) {
                Ok(runtime) => BuiltBackend {
                    kind: BackendKind::FallbackWindow,
                    reason: "fallback_window".to_string(),
                    backend: Box::new(WaylandOverlayBackend::new(
                        BackendKind::FallbackWindow,
                        ui,
                        runtime,
                    )),
                },
                Err(err) => BuiltBackend {
                    kind: BackendKind::Noop,
                    reason: format!("fallback_window_init_failed:{err}"),
                    backend: Box::new(NoopBackend {
                        reason: "runtime_backend_init_failed".to_string(),
                    }),
                },
            }
        }
        BackendSelection::Noop { reason } => BuiltBackend {
            kind: BackendKind::Noop,
            reason: reason.clone(),
            backend: Box::new(NoopBackend { reason }),
        },
    }
}

struct WaylandRuntime {
    connection: Connection,
    event_queue: EventQueue<WaylandRuntimeState>,
    state: WaylandRuntimeState,
    surface: wl_surface::WlSurface,
    shell: ShellSurface,
    shm_buffer: ShmBuffer,
    dimensions: SurfaceDimensions,
    text_renderer: TextRenderer,
}

enum ShellSurface {
    Layer {
        _layer_surface: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    },
    Fallback {
        _xdg_surface: xdg_surface::XdgSurface,
        toplevel: xdg_toplevel::XdgToplevel,
    },
}

impl WaylandRuntime {
    fn new(kind: BackendKind, ui: &OverlayUiConfig) -> Result<Self> {
        if kind == BackendKind::Noop {
            return Err(anyhow!(
                "cannot initialize Wayland runtime for noop backend"
            ));
        }

        let connection = Connection::connect_to_env().context("failed to connect to Wayland")?;
        let display = connection.display();
        let mut event_queue = connection.new_event_queue();
        let queue_handle = event_queue.handle();
        let _registry = display.get_registry(&queue_handle, ());

        let mut state = WaylandRuntimeState::default();
        event_queue
            .roundtrip(&mut state)
            .context("failed initial Wayland registry roundtrip")?;
        event_queue
            .roundtrip(&mut state)
            .context("failed secondary Wayland registry roundtrip")?;

        let compositor = state
            .globals
            .compositor
            .clone()
            .ok_or_else(|| anyhow!("wl_compositor unavailable"))?;
        let shm = state
            .globals
            .shm
            .clone()
            .ok_or_else(|| anyhow!("wl_shm unavailable"))?;

        let surface = compositor.create_surface(&queue_handle, ());
        let dimensions = ui.surface_dimensions();
        let mut shm_buffer = ShmBuffer::new(&shm, &queue_handle, dimensions)?;
        shm_buffer.paint(argb_pixel(0, 0, 0, 0))?;
        let text_renderer = TextRenderer::new(&ui.font);

        let shell = match kind {
            BackendKind::LayerShell => {
                let layer_shell = state
                    .globals
                    .layer_shell
                    .clone()
                    .ok_or_else(|| anyhow!("zwlr_layer_shell_v1 unavailable"))?;
                let layer_surface = layer_shell.get_layer_surface(
                    &surface,
                    None,
                    zwlr_layer_shell_v1::Layer::Overlay,
                    LAYER_NAMESPACE.to_string(),
                    &queue_handle,
                    (),
                );
                layer_surface.set_anchor(layer_anchor(ui.anchor));
                // Compensate layer-shell margins for shadow padding so content stays
                // in the same visual position.
                let shadow_offset = SHADOW_RADIUS;
                let (top, right, bottom, left) = layer_margins(
                    ui.anchor,
                    ui.margin_x.saturating_sub(shadow_offset),
                    ui.margin_y.saturating_sub(shadow_offset),
                );
                layer_surface.set_margin(top, right, bottom, left);
                layer_surface.set_exclusive_zone(0);
                layer_surface
                    .set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
                layer_surface.set_size(dimensions.width, dimensions.height);
                ShellSurface::Layer {
                    _layer_surface: layer_surface,
                }
            }
            BackendKind::FallbackWindow => {
                let xdg_wm_base = state
                    .globals
                    .xdg_wm_base
                    .clone()
                    .ok_or_else(|| anyhow!("xdg_wm_base unavailable"))?;
                let xdg_surface = xdg_wm_base.get_xdg_surface(&surface, &queue_handle, ());
                let toplevel = xdg_surface.get_toplevel(&queue_handle, ());
                toplevel.set_app_id("dev.parakeet.overlay".to_string());
                toplevel.set_title(FALLBACK_WINDOW_TITLE.to_string());
                xdg_surface.set_window_geometry(
                    0,
                    0,
                    dimensions.width as i32,
                    dimensions.height as i32,
                );
                ShellSurface::Fallback {
                    _xdg_surface: xdg_surface,
                    toplevel,
                }
            }
            BackendKind::Noop => return Err(anyhow!("unexpected noop backend kind")),
        };

        surface.commit();
        connection
            .flush()
            .context("failed to flush Wayland setup commit")?;
        event_queue
            .roundtrip(&mut state)
            .context("failed waiting for initial configure")?;

        Ok(Self {
            connection,
            event_queue,
            state,
            surface,
            shell,
            shm_buffer,
            dimensions,
            text_renderer,
        })
    }

    fn render_with_fade(
        &mut self,
        intent: &OverlayRenderIntent,
        ui: &OverlayUiConfig,
        fade_alpha: f32,
    ) -> Result<()> {
        self.dispatch_pending("failed pre-render event dispatch")?;

        if self.state.closed {
            return Err(anyhow!("overlay surface closed by compositor"));
        }

        if !self.state.configured {
            self.event_queue
                .roundtrip(&mut self.state)
                .context("failed waiting for compositor configure")?;
        }

        let keep_surface_mapped_when_hidden = matches!(self.shell, ShellSurface::Layer { .. });
        let should_render = intent.visible || fade_alpha > 0.0 || keep_surface_mapped_when_hidden;
        if should_render {
            let content = ui.content_area();
            render_frame(
                self.shm_buffer.bytes_mut(),
                self.dimensions,
                intent,
                ui,
                &self.text_renderer,
                fade_alpha,
                content,
            );
            self.shm_buffer.sync_to_file()?;
            self.surface.attach(Some(&self.shm_buffer.buffer), 0, 0);
            self.surface.damage_buffer(
                0,
                0,
                self.dimensions.width as i32,
                self.dimensions.height as i32,
            );
            if let ShellSurface::Fallback { toplevel, .. } = &self.shell {
                if intent.visible {
                    toplevel.set_title(format!(
                        "{FALLBACK_WINDOW_TITLE}: {}",
                        truncate_for_title(&intent.headline)
                    ));
                } else {
                    toplevel.set_title(FALLBACK_WINDOW_TITLE.to_string());
                }
            }
        } else {
            self.surface.attach(None, 0, 0);
            if let ShellSurface::Fallback { toplevel, .. } = &self.shell {
                toplevel.set_title(FALLBACK_WINDOW_TITLE.to_string());
            }
        }

        self.surface.commit();
        self.connection
            .flush()
            .context("failed flushing Wayland render updates")?;
        self.dispatch_pending("failed post-render event dispatch")?;

        if self.state.closed {
            return Err(anyhow!("overlay surface closed by compositor"));
        }

        Ok(())
    }

    fn dispatch_pending(&mut self, context: &'static str) -> Result<()> {
        self.event_queue
            .dispatch_pending(&mut self.state)
            .context(context)?;
        Ok(())
    }
}

struct ShmBuffer {
    file: File,
    _pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
    bytes: Vec<u8>,
}

impl ShmBuffer {
    fn new(
        shm: &wl_shm::WlShm,
        queue_handle: &QueueHandle<WaylandRuntimeState>,
        dimensions: SurfaceDimensions,
    ) -> Result<Self> {
        let stride = dimensions
            .width
            .checked_mul(4)
            .ok_or_else(|| anyhow!("overlay stride overflow"))?;
        let size_bytes = stride
            .checked_mul(dimensions.height)
            .ok_or_else(|| anyhow!("overlay buffer size overflow"))?;
        let size_bytes_i32 = i32::try_from(size_bytes).context("overlay buffer too large")?;
        let width_i32 = i32::try_from(dimensions.width).context("overlay width too large")?;
        let height_i32 = i32::try_from(dimensions.height).context("overlay height too large")?;
        let stride_i32 = i32::try_from(stride).context("overlay stride too large")?;

        let file = tempfile::tempfile().context("failed to create overlay shm tempfile")?;
        file.set_len(u64::from(size_bytes))
            .context("failed to size overlay shm tempfile")?;

        let pool = shm.create_pool(file.as_fd(), size_bytes_i32, queue_handle, ());
        let buffer = pool.create_buffer(
            0,
            width_i32,
            height_i32,
            stride_i32,
            wl_shm::Format::Argb8888,
            queue_handle,
            (),
        );

        Ok(Self {
            file,
            _pool: pool,
            buffer,
            bytes: vec![0; usize::try_from(size_bytes).unwrap_or(0)],
        })
    }

    fn paint(&mut self, pixel: [u8; 4]) -> Result<()> {
        for chunk in self.bytes.chunks_exact_mut(4) {
            chunk.copy_from_slice(&pixel);
        }
        self.sync_to_file()
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    fn sync_to_file(&mut self) -> Result<()> {
        self.file
            .seek(SeekFrom::Start(0))
            .context("failed to seek overlay shm file")?;
        self.file
            .write_all(&self.bytes)
            .context("failed to write overlay shm pixel data")?;
        Ok(())
    }
}

#[derive(Default)]
struct WaylandRuntimeState {
    globals: RuntimeGlobals,
    configured: bool,
    closed: bool,
}

#[derive(Default)]
struct RuntimeGlobals {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    xdg_wm_base: Option<xdg_wm_base::XdgWmBase>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for WaylandRuntimeState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        queue_handle: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    state.globals.compositor =
                        Some(registry.bind::<wl_compositor::WlCompositor, _, _>(
                            name,
                            version.min(6),
                            queue_handle,
                            (),
                        ));
                }
                "wl_shm" => {
                    state.globals.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(
                        name,
                        version.min(1),
                        queue_handle,
                        (),
                    ));
                }
                "xdg_wm_base" => {
                    state.globals.xdg_wm_base =
                        Some(registry.bind::<xdg_wm_base::XdgWmBase, _, _>(
                            name,
                            version.min(1),
                            queue_handle,
                            (),
                        ));
                }
                "zwlr_layer_shell_v1" => {
                    state.globals.layer_shell = Some(
                        registry.bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1, _, _>(
                            name,
                            version.min(4),
                            queue_handle,
                            (),
                        ),
                    );
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for WaylandRuntimeState {
    fn event(
        _: &mut Self,
        _: &wl_compositor::WlCompositor,
        _: wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for WaylandRuntimeState {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm::WlShm, ()> for WaylandRuntimeState {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for WaylandRuntimeState {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for WaylandRuntimeState {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for WaylandRuntimeState {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for WaylandRuntimeState {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            state.configured = true;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for WaylandRuntimeState {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Close = event {
            state.closed = true;
        }
    }
}

impl Dispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for WaylandRuntimeState {
    fn event(
        _: &mut Self,
        _: &zwlr_layer_shell_v1::ZwlrLayerShellV1,
        _: zwlr_layer_shell_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for WaylandRuntimeState {
    fn event(
        state: &mut Self,
        layer_surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width: _,
                height: _,
            } => {
                layer_surface.ack_configure(serial);
                state.configured = true;
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.closed = true;
            }
            _ => {}
        }
    }
}

fn probe_backend_signals() -> Result<BackendSignals> {
    let connection = Connection::connect_to_env().context("failed to connect to Wayland")?;
    let display = connection.display();
    let mut event_queue = connection.new_event_queue();
    let queue_handle = event_queue.handle();
    let _registry = display.get_registry(&queue_handle, ());

    let mut state = ProbeState::default();
    event_queue
        .roundtrip(&mut state)
        .context("failed initial probe roundtrip")?;
    event_queue
        .roundtrip(&mut state)
        .context("failed secondary probe roundtrip")?;
    Ok(state.signals)
}

#[derive(Default)]
struct ProbeState {
    signals: BackendSignals,
}

impl Dispatch<wl_registry::WlRegistry, ()> for ProbeState {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { interface, .. } = event {
            match interface.as_str() {
                "zwlr_layer_shell_v1" => state.signals.has_layer_shell = true,
                "wl_compositor" => state.signals.has_wl_compositor = true,
                "xdg_wm_base" => state.signals.has_xdg_wm_base = true,
                "wl_shm" => state.signals.has_wl_shm = true,
                _ => {}
            }
        }
    }
}

fn layer_anchor(anchor: CliAnchor) -> zwlr_layer_surface_v1::Anchor {
    use zwlr_layer_surface_v1::Anchor;

    match anchor {
        CliAnchor::TopLeft => Anchor::Top | Anchor::Left,
        CliAnchor::TopCenter => Anchor::Top,
        CliAnchor::TopRight => Anchor::Top | Anchor::Right,
        CliAnchor::BottomLeft => Anchor::Bottom | Anchor::Left,
        CliAnchor::BottomCenter => Anchor::Bottom,
        CliAnchor::BottomRight => Anchor::Bottom | Anchor::Right,
    }
}

fn layer_margins(anchor: CliAnchor, margin_x: u32, margin_y: u32) -> (i32, i32, i32, i32) {
    let x = margin_x as i32;
    let y = margin_y as i32;

    match anchor {
        CliAnchor::TopLeft => (y, 0, 0, x),
        CliAnchor::TopCenter => (y, 0, 0, 0),
        CliAnchor::TopRight => (y, x, 0, 0),
        CliAnchor::BottomLeft => (0, 0, y, x),
        CliAnchor::BottomCenter => (0, 0, y, 0),
        CliAnchor::BottomRight => (0, x, y, 0),
    }
}

fn parse_font_descriptor(raw: &str) -> ParsedFontDescriptor {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return ParsedFontDescriptor {
            family: DEFAULT_FONT_FAMILY.to_string(),
            size_px: DEFAULT_FONT_SIZE_PX,
            fallback_reason: Some("font_descriptor_empty".to_string()),
        };
    }

    let mut tokens = trimmed.split_whitespace().collect::<Vec<_>>();
    if tokens.len() >= 2 {
        let maybe_size = tokens.last().and_then(|value| value.parse::<f32>().ok());
        if let Some(size) = maybe_size {
            let _ = tokens.pop();
            let family = tokens.join(" ");
            if family.trim().is_empty() {
                return ParsedFontDescriptor {
                    family: DEFAULT_FONT_FAMILY.to_string(),
                    size_px: size.clamp(*FONT_SIZE_RANGE.start(), *FONT_SIZE_RANGE.end()),
                    fallback_reason: Some("font_family_missing_using_default".to_string()),
                };
            }
            return ParsedFontDescriptor {
                family,
                size_px: size.clamp(*FONT_SIZE_RANGE.start(), *FONT_SIZE_RANGE.end()),
                fallback_reason: None,
            };
        }
    }

    ParsedFontDescriptor {
        family: trimmed.to_string(),
        size_px: DEFAULT_FONT_SIZE_PX,
        fallback_reason: Some("font_size_missing_using_default".to_string()),
    }
}

fn parse_generic_family_kind(input: &str) -> Option<GenericFamilyKind> {
    let normalized = input.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "sans" | "sans-serif" | "sans_serif" => Some(GenericFamilyKind::SansSerif),
        "serif" => Some(GenericFamilyKind::Serif),
        "mono" | "monospace" | "monospace-ui" => Some(GenericFamilyKind::Monospace),
        "cursive" => Some(GenericFamilyKind::Cursive),
        "fantasy" => Some(GenericFamilyKind::Fantasy),
        _ => None,
    }
}

fn load_font_from_database(db: &Database, family: &str) -> Option<Font> {
    let query = Query {
        families: &[Family::Name(family)],
        ..Query::default()
    };
    let face_id = db.query(&query)?;
    load_font_from_face(db, face_id)
}

fn load_font_from_generic(db: &Database, family: GenericFamilyKind) -> Option<Font> {
    let binding = [family.as_fontdb_family()];
    let query = Query {
        families: &binding,
        ..Query::default()
    };
    let face_id = db.query(&query)?;
    load_font_from_face(db, face_id)
}

fn load_any_font_from_database(db: &Database) -> Option<Font> {
    let face_id = db.faces().next()?.id;
    load_font_from_face(db, face_id)
}

fn load_font_from_face(db: &Database, face_id: fontdb::ID) -> Option<Font> {
    if let Some(font) = db.with_face_data(face_id, |data, face_index| {
        Font::from_bytes(
            data,
            FontSettings {
                collection_index: face_index,
                ..FontSettings::default()
            },
        )
        .ok()
    }) {
        return font;
    }

    let face = db.face(face_id)?;
    let bytes = match &face.source {
        Source::Binary(bytes) => bytes.as_ref().as_ref().to_vec(),
        Source::File(path) => std::fs::read(path).ok()?,
        Source::SharedFile(path, _) => std::fs::read(path).ok()?,
    };
    Font::from_bytes(
        bytes,
        FontSettings {
            collection_index: face.index,
            ..FontSettings::default()
        },
    )
    .ok()
}

fn argb_pixel(r: u8, g: u8, b: u8, a: u8) -> [u8; 4] {
    [b, g, r, a]
}

fn render_frame(
    frame: &mut [u8],
    dimensions: SurfaceDimensions,
    intent: &OverlayRenderIntent,
    ui: &OverlayUiConfig,
    text_renderer: &TextRenderer,
    fade_alpha: f32,
    content: ContentArea,
) {
    // 1. Clear to transparent
    fill_frame(frame, [0, 0, 0, 0]);

    if fade_alpha <= 0.0 {
        return;
    }

    let content_rect = Rect {
        x: content.x as f32,
        y: content.y as f32,
        w: content.width as f32,
        h: content.height as f32,
    };

    // 2. Draw shadow
    let shadow_a = (SHADOW_ALPHA as f32 * fade_alpha).round() as u8;
    draw_shadow(
        frame,
        dimensions,
        content,
        CORNER_RADIUS,
        SHADOW_RADIUS,
        shadow_a,
    );

    // 3. Fill rounded rect (dark background)
    let bg_a = (BG_ALPHA as f32 * fade_alpha).round() as u8;
    fill_rounded_rect(
        frame,
        dimensions,
        content_rect,
        CORNER_RADIUS,
        argb_pixel(BG_COLOR.0, BG_COLOR.1, BG_COLOR.2, bg_a),
    );

    // 4. Stroke rounded rect (thin border)
    let border_a = (BORDER_ALPHA as f32 * fade_alpha).round() as u8;
    stroke_rounded_rect(
        frame,
        dimensions,
        content_rect,
        CORNER_RADIUS,
        BORDER_THICKNESS,
        argb_pixel(BORDER_COLOR.0, BORDER_COLOR.1, BORDER_COLOR.2, border_a),
    );

    // 5. Draw accent stripe
    if let Some(accent) = accent_color_for_phase(intent.phase) {
        draw_accent_stripe(
            frame,
            dimensions,
            content,
            ACCENT_STRIPE_WIDTH,
            ACCENT_STRIPE_MARGIN,
            accent,
            fade_alpha,
        );
    }

    // 6. Draw text
    let headline = intent.headline.trim();
    let detail = intent
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let text = if let Some(detail) = detail {
        format!("{headline} {detail}")
    } else {
        headline.to_string()
    };

    text_renderer.draw_headline(
        frame,
        dimensions,
        content,
        ui.max_width,
        ui.max_lines,
        &text,
        fade_alpha,
    );
}

fn fill_frame(frame: &mut [u8], pixel: [u8; 4]) {
    for chunk in frame.chunks_exact_mut(4) {
        chunk.copy_from_slice(&pixel);
    }
}

fn blend_bitmap(
    frame: &mut [u8],
    dimensions: SurfaceDimensions,
    origin: (i32, i32),
    size: (usize, usize),
    alpha_bitmap: &[u8],
    color: [u8; 4],
) {
    let (origin_x, origin_y) = origin;
    let (width, height) = size;
    let frame_width = dimensions.width as i32;
    let frame_height = dimensions.height as i32;
    if origin_x >= frame_width || origin_y >= frame_height {
        return;
    }

    // color is already premultiplied: [B*a/255, G*a/255, R*a/255, a]
    for y in 0..height {
        let draw_y = origin_y + y as i32;
        if !(0..frame_height).contains(&draw_y) {
            continue;
        }
        for x in 0..width {
            let draw_x = origin_x + x as i32;
            if !(0..frame_width).contains(&draw_x) {
                continue;
            }
            let coverage = alpha_bitmap[y * width + x];
            if coverage == 0 {
                continue;
            }

            let cov16 = u16::from(coverage);
            // Scale premultiplied color channels by coverage
            let sa = ((cov16 * u16::from(color[3])) / 255) as u8;
            if sa == 0 {
                continue;
            }

            let idx = ((draw_y as u32 * dimensions.width + draw_x as u32) * 4) as usize;
            let inv = 255 - u16::from(sa);
            for ch in 0..3 {
                let src_premul = ((cov16 * u16::from(color[ch])) / 255) as u8;
                frame[idx + ch] =
                    (u16::from(src_premul) + (u16::from(frame[idx + ch]) * inv) / 255) as u8;
            }
            frame[idx + 3] = (u16::from(sa) + (u16::from(frame[idx + 3]) * inv) / 255) as u8;
        }
    }
}

fn layout_text_lines<F>(
    text: &str,
    max_width_px: u32,
    max_lines: u32,
    mut measure: F,
) -> Vec<String>
where
    F: FnMut(char) -> f32,
{
    let max_width = max_width_px.max(48) as f32;
    let max_lines = max_lines.clamp(1, 10) as usize;
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return Vec::new();
    }

    let mut lines = VecDeque::with_capacity(max_lines);
    let mut current = String::new();
    let mut push_line = |line: String| {
        if line.is_empty() {
            return;
        }
        if lines.len() >= max_lines {
            let _ = lines.pop_front();
        }
        lines.push_back(line);
    };

    for word in normalized.split(' ') {
        let candidate = if current.is_empty() {
            word.to_string()
        } else {
            format!("{current} {word}")
        };
        if measure_width(&candidate, &mut measure) <= max_width {
            current = candidate;
            continue;
        }

        if !current.is_empty() {
            push_line(std::mem::take(&mut current));
        }

        let fitted_word = fit_text_to_width(word, max_width, &mut measure);
        if fitted_word.is_empty() {
            continue;
        }
        if fitted_word != word {
            push_line(fitted_word);
            continue;
        }
        current = fitted_word;
    }

    if !current.is_empty() {
        push_line(current);
    }

    lines.into_iter().collect()
}

fn fit_text_to_width<F>(text: &str, max_width: f32, mut measure: F) -> String
where
    F: FnMut(char) -> f32,
{
    if measure_width(text, &mut measure) <= max_width {
        return text.to_string();
    }

    let suffix = "...";
    let mut output = String::new();
    for character in text.chars() {
        let candidate = format!("{output}{character}");
        let candidate_with_suffix = format!("{candidate}{suffix}");
        if measure_width(&candidate_with_suffix, &mut measure) > max_width {
            break;
        }
        output.push(character);
    }

    if output.is_empty() {
        String::new()
    } else {
        format!("{output}{suffix}")
    }
}

fn measure_width<F>(text: &str, measure: &mut F) -> f32
where
    F: FnMut(char) -> f32,
{
    text.chars()
        .fold(0.0, |width, character| width + measure(character))
}

fn truncate_for_title(input: &str) -> String {
    let trimmed = input.trim();
    let mut output = String::new();
    for character in trimmed.chars().take(96) {
        output.push(character);
    }
    if output.is_empty() {
        "(active)".to_string()
    } else {
        output
    }
}

fn is_probably_cosmic_session() -> bool {
    [
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_DESKTOP",
        "DESKTOP_SESSION",
    ]
    .iter()
    .filter_map(|key| std::env::var(key).ok())
    .any(|value| value.to_ascii_lowercase().contains("cosmic"))
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let ui = OverlayUiConfig {
        opacity: cli.opacity.clamp(0.0, 1.0),
        font: cli.font,
        anchor: cli.anchor,
        margin_x: cli.margin_x,
        margin_y: cli.margin_y,
        max_width: cli.max_width,
        max_lines: cli.max_lines,
    };

    let mut built_backend = build_backend(cli.backend, ui.clone());
    info!(
        backend = ?built_backend.kind,
        reason = %built_backend.reason,
        opacity = ui.opacity,
        font = %ui.font,
        anchor = ?ui.anchor,
        margin_x = ui.margin_x,
        margin_y = ui.margin_y,
        max_width = ui.max_width,
        max_lines = ui.max_lines,
        "overlay process started"
    );
    if built_backend.kind == BackendKind::FallbackWindow && is_probably_cosmic_session() {
        warn!(
            backend = "fallback_window",
            desktop = "cosmic",
            "overlay fallback-window is degraded on COSMIC (tiling/focus behavior is compositor-managed); prefer --backend layer-shell"
        );
    }

    let mut machine = OverlayStateMachine::new(Duration::from_millis(cli.auto_hide_ms.max(1)));
    built_backend
        .backend
        .render(machine.visibility())
        .context("initial overlay render failed")?;

    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let started = Instant::now();

    loop {
        tokio::select! {
            line = lines.next_line() => {
                match line {
                    Ok(Some(raw)) => {
                        if raw.trim().is_empty() {
                            continue;
                        }
                        let now_ms = started.elapsed().as_millis() as u64;
                        match serde_json::from_str::<OverlayIpcMessage>(&raw) {
                            Ok(message) => {
                                match machine.apply_event(message, now_ms) {
                                    ApplyOutcome::Applied => {
                                        built_backend
                                            .backend
                                            .render(machine.visibility())
                                            .context("overlay render failed while applying event")?;
                                    }
                                    ApplyOutcome::DroppedStaleSeq => {
                                        debug!("overlay process dropped stale sequence event");
                                    }
                                    ApplyOutcome::DroppedSessionMismatch => {
                                        debug!("overlay process dropped session mismatch event");
                                    }
                                }
                            }
                            Err(err) => {
                                warn!(error = %err, payload = %raw, "failed to decode overlay IPC event");
                            }
                        }
                    }
                    Ok(None) => {
                        info!("overlay stdin closed; shutting down");
                        break;
                    }
                    Err(err) => {
                        warn!(error = %err, "overlay stdin read error; shutting down");
                        break;
                    }
                }
            }
            _ = tick.tick() => {
                let now_ms = started.elapsed().as_millis() as u64;
                let time_advanced = machine.advance_time(now_ms);
                let fading = built_backend.backend.is_fading();
                if time_advanced || fading {
                    built_backend
                        .backend
                        .render(machine.visibility())
                        .context("overlay render failed while advancing auto-hide timer")?;
                }
            }
        }
    }

    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::{
        accent_color_for_phase, ease_out_cubic, layout_text_lines, parse_font_descriptor,
        parse_generic_family_kind, render_frame, resolve_backend_selection, rounded_rect_coverage,
        BackendSelection, BackendSignals, CliBackendMode, FadeDirection, FadeState,
        FontResolutionSummary, OverlayUiConfig, ParsedFontDescriptor, Rect, TextRenderer,
        SHADOW_RADIUS,
    };
    use parakeet_ptt::overlay_state::{OverlayRenderIntent, OverlayRenderPhase};

    #[test]
    fn auto_prefers_layer_shell_when_available() {
        assert_eq!(
            resolve_backend_selection(
                CliBackendMode::Auto,
                Ok(BackendSignals {
                    has_layer_shell: true,
                    has_wl_compositor: true,
                    has_xdg_wm_base: true,
                    has_wl_shm: true,
                })
            ),
            BackendSelection::LayerShell
        );
    }

    #[test]
    fn auto_uses_fallback_when_layer_shell_missing() {
        assert_eq!(
            resolve_backend_selection(
                CliBackendMode::Auto,
                Ok(BackendSignals {
                    has_layer_shell: false,
                    has_wl_compositor: true,
                    has_xdg_wm_base: true,
                    has_wl_shm: true,
                })
            ),
            BackendSelection::FallbackWindow
        );
    }

    #[test]
    fn explicit_layer_shell_disables_when_unsupported() {
        assert_eq!(
            resolve_backend_selection(
                CliBackendMode::LayerShell,
                Ok(BackendSignals {
                    has_layer_shell: false,
                    has_wl_compositor: true,
                    has_xdg_wm_base: true,
                    has_wl_shm: true,
                })
            ),
            BackendSelection::Noop {
                reason: "unsupported_wayland_backend:layer_shell".to_string(),
            }
        );
    }

    #[test]
    fn probe_failure_degrades_to_noop() {
        assert_eq!(
            resolve_backend_selection(CliBackendMode::Auto, Err("no_display".to_string())),
            BackendSelection::Noop {
                reason: "wayland_probe_failed:no_display".to_string(),
            }
        );
    }

    #[test]
    fn hidden_phase_accent_is_none() {
        assert!(accent_color_for_phase(OverlayRenderPhase::Hidden).is_none());
    }

    #[test]
    fn visible_phase_accents_are_some() {
        assert!(accent_color_for_phase(OverlayRenderPhase::Listening).is_some());
        assert!(accent_color_for_phase(OverlayRenderPhase::Interim).is_some());
        assert!(accent_color_for_phase(OverlayRenderPhase::Finalizing).is_some());
    }

    #[test]
    fn font_descriptor_parses_family_and_size() {
        assert_eq!(
            parse_font_descriptor("Sans 16"),
            ParsedFontDescriptor {
                family: "Sans".to_string(),
                size_px: 16.0,
                fallback_reason: None,
            }
        );
    }

    #[test]
    fn malformed_font_descriptor_falls_back_to_default_size() {
        let parsed = parse_font_descriptor("InvalidDescriptor");
        assert_eq!(parsed.family, "InvalidDescriptor".to_string());
        assert_eq!(parsed.size_px, 18.0);
        assert_eq!(
            parsed.fallback_reason.as_deref(),
            Some("font_size_missing_using_default")
        );
    }

    #[test]
    fn generic_family_parsing_supports_common_aliases() {
        assert_eq!(
            parse_generic_family_kind("Sans"),
            Some(super::GenericFamilyKind::SansSerif)
        );
        assert_eq!(
            parse_generic_family_kind("monospace"),
            Some(super::GenericFamilyKind::Monospace)
        );
        assert_eq!(parse_generic_family_kind("unknown"), None);
    }

    #[test]
    fn text_layout_keeps_recent_tail_lines_when_clamped() {
        let lines = layout_text_lines("one two three four five six", 100, 2, |_| 10.0);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "three four");
        assert_eq!(lines[1], "five six");
    }

    #[test]
    fn text_layout_truncates_single_long_word_to_width() {
        let lines = layout_text_lines("alpha", 40, 2, |_| 10.0);
        assert_eq!(lines, vec!["a...".to_string()]);
    }

    #[test]
    fn render_intent_mapping_visible_vs_hidden_alpha() {
        let ui = OverlayUiConfig {
            opacity: 0.92,
            font: "Sans 18".to_string(),
            anchor: super::CliAnchor::TopCenter,
            margin_x: 24,
            margin_y: 24,
            max_width: 320,
            max_lines: 3,
        };
        let dimensions = ui.surface_dimensions();
        let content = ui.content_area();
        let text_renderer = TextRenderer {
            summary: FontResolutionSummary {
                requested: "Sans 18".to_string(),
                family: "Sans".to_string(),
                size_px: 18.0,
                fallback_reason: None,
            },
            font: None,
        };

        let mut hidden_frame = vec![255u8; (dimensions.width * dimensions.height * 4) as usize];
        render_frame(
            &mut hidden_frame,
            dimensions,
            &OverlayRenderIntent {
                phase: OverlayRenderPhase::Hidden,
                visible: false,
                headline: String::new(),
                detail: None,
            },
            &ui,
            &text_renderer,
            0.0,
            content,
        );
        assert!(hidden_frame.chunks_exact(4).all(|pixel| pixel[3] == 0));

        let mut visible_frame = vec![0u8; (dimensions.width * dimensions.height * 4) as usize];
        render_frame(
            &mut visible_frame,
            dimensions,
            &OverlayRenderIntent {
                phase: OverlayRenderPhase::Listening,
                visible: true,
                headline: "listening".to_string(),
                detail: None,
            },
            &ui,
            &text_renderer,
            1.0,
            content,
        );
        assert!(visible_frame.chunks_exact(4).any(|pixel| pixel[3] > 0));
    }

    #[test]
    fn surface_dimensions_include_shadow() {
        let ui = OverlayUiConfig {
            opacity: 0.92,
            font: "Sans 18".to_string(),
            anchor: super::CliAnchor::TopCenter,
            margin_x: 24,
            margin_y: 24,
            max_width: 320,
            max_lines: 3,
        };
        let dims = ui.surface_dimensions();
        let content = ui.content_area();
        assert_eq!(dims.width, content.width + SHADOW_RADIUS * 2);
        assert_eq!(dims.height, content.height + SHADOW_RADIUS * 2);
        assert_eq!(content.x, SHADOW_RADIUS);
        assert_eq!(content.y, SHADOW_RADIUS);
    }

    #[test]
    fn fill_rounded_rect_corner_coverage() {
        let rect = Rect {
            x: 10.0,
            y: 10.0,
            w: 100.0,
            h: 50.0,
        };
        // Center of rect: full coverage
        assert_eq!(rounded_rect_coverage(60.5, 35.5, rect, 12.0), 1.0);
        // Well outside rect: zero coverage
        assert_eq!(rounded_rect_coverage(0.5, 0.5, rect, 12.0), 0.0);
        // Right at the corner: partial coverage (anti-aliased)
        let corner_cov = rounded_rect_coverage(10.5, 10.5, rect, 12.0);
        assert!(
            (0.0..=1.0).contains(&corner_cov),
            "corner coverage should be 0..1, got {corner_cov}"
        );
        // Along the top edge but outside corner arcs: full coverage.
        let top_edge_cov = rounded_rect_coverage(95.5, 10.5, rect, 12.0);
        assert!(
            (top_edge_cov - 1.0).abs() < f32::EPSILON,
            "top edge coverage should stay full outside corners, got {top_edge_cov}"
        );
    }

    #[test]
    fn fade_progress_interpolation() {
        let fade_in = FadeState {
            direction: FadeDirection::In,
            started_ms: 100,
            duration_ms: 250,
        };
        // At start
        assert!((fade_in.progress(100) - 0.0).abs() < 0.01);
        // At end
        assert!((fade_in.progress(350) - 1.0).abs() < 0.01);
        // Midway should be between 0 and 1
        let mid = fade_in.progress(225);
        assert!(mid > 0.0 && mid < 1.0, "midway progress = {mid}");

        let fade_out = FadeState {
            direction: FadeDirection::Out,
            started_ms: 100,
            duration_ms: 250,
        };
        assert!((fade_out.progress(100) - 1.0).abs() < 0.01);
        assert!((fade_out.progress(350) - 0.0).abs() < 0.01);
    }

    #[test]
    fn ease_out_cubic_boundaries() {
        assert!((ease_out_cubic(0.0) - 0.0).abs() < f32::EPSILON);
        assert!((ease_out_cubic(1.0) - 1.0).abs() < f32::EPSILON);
        // Ease-out should be > linear at midpoint
        assert!(ease_out_cubic(0.5) > 0.5);
        // Monotonically increasing
        let mut prev = 0.0f32;
        for i in 0..=100 {
            let t = i as f32 / 100.0;
            let v = ease_out_cubic(t);
            assert!(v >= prev, "not monotonic at t={t}");
            prev = v;
        }
    }
}
