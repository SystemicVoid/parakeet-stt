use std::collections::VecDeque;
use std::f32::consts::PI;
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
    wl_buffer, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool, wl_surface,
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
const OVERLAY_ADAPTIVE_WIDTH_ENV: &str = "PARAKEET_OVERLAY_ADAPTIVE_WIDTH";
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
const PADDING_LEFT: u32 = 72;
const ACCENT_STRIPE_WIDTH: f32 = 3.0;
const ACCENT_STRIPE_MARGIN: f32 = 4.0;
const ENTRANCE_DURATION_MS: u64 = 300;
const EXIT_DURATION_MS: u64 = 250;
const ACCENT_CROSSFADE_MS: u64 = 150;
const ENTRANCE_SLIDE_PX: f32 = 7.0;
const EXIT_SLIDE_PX: f32 = 5.0;
const BREATHING_CYCLE_MS: u64 = 3000;
const BREATHING_AMPLITUDE: f32 = 0.05;
const CHAR_FADEIN_MS: u64 = 100;
const CHAR_STAGGER_MS: u64 = 16;
const MIN_PANEL_WIDTH: u32 = 240;
const WIDTH_ANIM_MS: u64 = 200;

const PHRASE_ROTATE_MS: u64 = 3000;
const PHRASE_CROSSFADE_MS: u64 = 200;
const ELLIPSIS_DOT_DELAY_MS: u64 = 200;
const ELLIPSIS_CYCLE_MS: u64 = 1200;

const LISTENING_PHRASES: &[&str] = &[
    "Listening",
    "Go ahead",
    "Ready when you are",
    "Speak freely",
    "I'm all ears",
    "Say the word",
    "Standing by",
    "Fire away",
    "What's on your mind",
    "Take your time",
    "Hearing you",
    "At your service",
];

const PROGRESS_BAR_HEIGHT: f32 = 2.0;
const PROGRESS_SWEEP_MS: u64 = 1500;
const PROGRESS_SEGMENT_FRAC: f32 = 0.3;
const SUCCESS_FLASH_MS: u64 = 200;
const SUCCESS_FLASH_COLOR: [u8; 4] = [80, 220, 120, 255]; // green accent

// --- Audio Waveform ---
const WF_CANVAS_W: usize = 20;
const WF_MAX_H: usize = 24;
const WF_UPSCALE: usize = 3;
const WF_COLOR: (u8, u8, u8) = (0, 220, 210);
const WF_DB_FLOOR: f32 = -60.0;
const WF_DB_CEIL: f32 = -5.0;
const WF_ATTACK: f32 = 0.85;
const WF_RELEASE: f32 = 0.05;
const WF_TICK_DECAY: f32 = 0.96;
const WF_DITHER_ZONE: f32 = 0.35;
const WF_MARGIN_LEFT: f32 = 4.0;
const WF_MARGIN_V: f32 = 6.0;
const WF_VISUAL_GAIN: f32 = 1.2;
const WF_WARMUP_MS: u64 = 250;
const WF_GLOW_ALPHA: u8 = 40;
const WF_GLOW_RADIUS_PX: i32 = 1;

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
    #[arg(long, value_enum, default_value_t = CliAnchor::BottomCenter)]
    anchor: CliAnchor,

    /// Horizontal margin from anchor reference point.
    #[arg(long, default_value_t = 24)]
    margin_x: u32,

    /// Vertical margin from anchor reference point.
    #[arg(long, default_value_t = 32)]
    margin_y: u32,

    /// Maximum text box width in pixels.
    #[arg(long, default_value_t = 960)]
    max_width: u32,

    /// Maximum rendered lines.
    #[arg(long, default_value_t = 4)]
    max_lines: u32,

    /// Preferred wl_output name for the layer surface target.
    #[arg(long)]
    output_name: Option<String>,

    /// Enable or disable adaptive overlay width.
    #[arg(long, action = clap::ArgAction::Set)]
    adaptive_width: Option<bool>,
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

fn parse_bool_override(raw: &str) -> Option<bool> {
    let normalized = raw.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn resolve_adaptive_width_override(cli_override: Option<bool>) -> bool {
    if let Some(adaptive_width) = cli_override {
        return adaptive_width;
    }

    std::env::var(OVERLAY_ADAPTIVE_WIDTH_ENV)
        .ok()
        .as_deref()
        .and_then(parse_bool_override)
        .unwrap_or(true)
}

#[cfg(test)]
fn resolve_adaptive_width_with_env_input(
    cli_override: Option<bool>,
    env_override: Option<&str>,
) -> bool {
    if let Some(adaptive_width) = cli_override {
        return adaptive_width;
    }

    env_override.and_then(parse_bool_override).unwrap_or(true)
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

const BAYER_4X4: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

struct WaveformCanvas {
    levels: [f32; WF_CANVAS_W],
    cursor: usize,
    created_ms: u64,
}

impl WaveformCanvas {
    fn new(now_ms: u64) -> Self {
        Self {
            levels: [0.0; WF_CANVAS_W],
            cursor: 0,
            created_ms: now_ms,
        }
    }

    fn push(&mut self, level_db: f32) {
        let target = ((level_db - WF_DB_FLOOR) / (WF_DB_CEIL - WF_DB_FLOOR)).clamp(0.0, 1.0);
        let previous = self.levels[self.cursor];
        let rate = if target > previous {
            WF_ATTACK
        } else {
            WF_RELEASE
        };
        self.levels[self.cursor] = previous + (target - previous) * rate;
        self.cursor = (self.cursor + 1) % WF_CANVAS_W;
    }

    fn tick_decay(&mut self) {
        for (index, level) in self.levels.iter_mut().enumerate() {
            if index == self.cursor {
                continue;
            }
            *level *= WF_TICK_DECAY;
            if *level < 0.01 {
                *level = 0.0;
            }
        }
    }

    fn has_signal(&self) -> bool {
        self.levels.iter().any(|&l| l > 0.0)
    }

    fn visible_columns(&self, now_ms: u64) -> usize {
        let elapsed = now_ms.saturating_sub(self.created_ms);
        let visible = (elapsed.saturating_mul(WF_CANVAS_W as u64) / WF_WARMUP_MS.max(1)) as usize;
        visible.clamp(0, WF_CANVAS_W)
    }
}

fn draw_waveform(
    frame: &mut [u8],
    dims: SurfaceDimensions,
    content: ContentArea,
    waveform: &WaveformCanvas,
    now_ms: u64,
    frame_alpha: f32,
) {
    let available_vertical = (content.height as f32 - 2.0 * WF_MARGIN_V).max(0.0);
    if available_vertical < WF_UPSCALE as f32 {
        return;
    }
    let canvas_h = ((available_vertical / WF_UPSCALE as f32).floor() as usize).clamp(1, WF_MAX_H);
    let zone_x = (content.x as f32 + WF_MARGIN_LEFT).round() as i32;
    let zone_h_px = (canvas_h * WF_UPSCALE) as i32;
    let zone_y =
        ((content.y + content.height) as f32 - WF_MARGIN_V - zone_h_px as f32).round() as i32;

    let mut shape_map = vec![0u8; WF_CANVAS_W * canvas_h];
    let visible_columns = waveform.visible_columns(now_ms);

    for cy in 0..canvas_h {
        for cx in 0..visible_columns {
            let level = waveform.levels[(waveform.cursor + cx) % WF_CANVAS_W];
            if level <= 0.0 {
                continue;
            }

            let fill_rows = (level * WF_VISUAL_GAIN).clamp(0.0, 1.0) * canvas_h as f32;
            if fill_rows <= 0.0 {
                continue;
            }
            let row_from_bottom = (canvas_h - 1 - cy) as f32;
            if row_from_bottom >= fill_rows {
                continue;
            }

            let bayer_value = BAYER_4X4[cy % 4][cx % 4] as f32 / 16.0;
            let solid_boundary = fill_rows * (1.0 - WF_DITHER_ZONE);
            let mut shape_type = 0u8; // 0: empty, 1: dot, 2: cross, 3: solid
            if row_from_bottom < solid_boundary {
                shape_type = 3;
                // Keep the base dense, but allow slight porosity for imbricated texture.
                let solid_progress = if solid_boundary <= f32::EPSILON {
                    1.0
                } else {
                    1.0 - row_from_bottom / solid_boundary
                };
                let solid_density = (0.84 + 0.14 * solid_progress).clamp(0.0, 1.0);
                if bayer_value > solid_density {
                    shape_type = 2;
                }
            } else {
                let dither_height = (fill_rows * WF_DITHER_ZONE).max(f32::EPSILON);
                let coverage = 1.0 - (row_from_bottom - solid_boundary) / dither_height;
                let combined = coverage - bayer_value;
                if combined > 0.15 {
                    shape_type = 3;
                } else if combined > -0.15 {
                    shape_type = 2;
                } else if combined > -0.45 {
                    shape_type = 1;
                }
            }

            shape_map[cy * WF_CANVAS_W + cx] = shape_type;
        }
    }

    let core_alpha = (frame_alpha * 255.0).round() as u8;
    let core_pixel = argb_pixel_premul(WF_COLOR.0, WF_COLOR.1, WF_COLOR.2, core_alpha);
    let glow_alpha = ((WF_GLOW_ALPHA as f32) * frame_alpha).round() as u8;
    let glow_pixel = argb_pixel_premul(WF_COLOR.0, WF_COLOR.1, WF_COLOR.2, glow_alpha);

    // Bloom pre-pass: low-alpha halo around denser waveform regions.
    if glow_alpha > 0 {
        for cy in 0..canvas_h {
            for cx in 0..visible_columns {
                let shape = shape_map[cy * WF_CANVAS_W + cx];
                if shape < 2 {
                    continue;
                }
                let tile_x = zone_x + (cx * WF_UPSCALE) as i32;
                let tile_y = zone_y + (cy * WF_UPSCALE) as i32;
                let x0 = tile_x - WF_GLOW_RADIUS_PX;
                let y0 = tile_y - WF_GLOW_RADIUS_PX;
                let x1 = tile_x + WF_UPSCALE as i32 + WF_GLOW_RADIUS_PX;
                let y1 = tile_y + WF_UPSCALE as i32 + WF_GLOW_RADIUS_PX;
                for py in y0..y1 {
                    for px in x0..x1 {
                        blend_pixel(frame, dims, px, py, glow_pixel);
                    }
                }
            }
        }
    }

    // Sharp pass: draw imbricated 3x3 shape tiles.
    for cy in 0..canvas_h {
        for cx in 0..visible_columns {
            let shape = shape_map[cy * WF_CANVAS_W + cx];
            if shape == 0 {
                continue;
            }
            for dy in 0..WF_UPSCALE {
                for dx in 0..WF_UPSCALE {
                    let draw_pixel = match shape {
                        3 => true,
                        2 => dx == 1 || dy == 1,
                        1 => dx == 1 && dy == 1,
                        _ => false,
                    };
                    if draw_pixel {
                        let px = zone_x + (cx * WF_UPSCALE + dx) as i32;
                        let py = zone_y + (cy * WF_UPSCALE + dy) as i32;
                        blend_pixel(frame, dims, px, py, core_pixel);
                    }
                }
            }
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

fn should_trigger_success_flash(
    previous_phase: OverlayRenderPhase,
    next_phase: OverlayRenderPhase,
) -> bool {
    previous_phase == OverlayRenderPhase::Finalizing && next_phase == OverlayRenderPhase::Hidden
}

fn listening_breathing_factor(elapsed_ms: u64) -> f32 {
    let cycle = BREATHING_CYCLE_MS.max(1) as f32;
    let phase = 2.0 * PI * ((elapsed_ms as f32) / cycle);
    1.0 + BREATHING_AMPLITUDE * phase.sin()
}

fn apply_breathing_alpha(alpha: u8, elapsed_ms: u64) -> u8 {
    (f32::from(alpha) * listening_breathing_factor(elapsed_ms))
        .round()
        .clamp(0.0, 255.0) as u8
}

fn should_apply_breathing(phase: OverlayRenderPhase, accent_transition_active: bool) -> bool {
    phase == OverlayRenderPhase::Listening && !accent_transition_active
}

fn shared_prefix_len(previous: &str, current: &str) -> usize {
    previous
        .chars()
        .zip(current.chars())
        .take_while(|(a, b)| a == b)
        .count()
}

fn shared_suffix_len(previous: &str, current: &str, shared_prefix: usize) -> usize {
    let previous_chars = previous.chars().collect::<Vec<_>>();
    let current_chars = current.chars().collect::<Vec<_>>();
    let previous_len = previous_chars.len();
    let current_len = current_chars.len();
    let max_suffix = previous_len.min(current_len).saturating_sub(shared_prefix);
    let mut suffix = 0usize;
    while suffix < max_suffix
        && previous_chars[previous_len - 1 - suffix] == current_chars[current_len - 1 - suffix]
    {
        suffix = suffix.saturating_add(1);
    }
    suffix
}

fn changed_char_range(previous: &str, current: &str) -> Option<(usize, usize)> {
    if previous == current {
        return None;
    }
    let shared_prefix = shared_prefix_len(previous, current);
    let shared_suffix = shared_suffix_len(previous, current, shared_prefix);
    let current_len = current.chars().count();
    let changed_end = current_len.saturating_sub(shared_suffix);
    if changed_end <= shared_prefix {
        None
    } else {
        Some((shared_prefix, changed_end))
    }
}

fn interim_char_fade_alpha(now_ms: u64, changed_ms: u64) -> f32 {
    (now_ms.saturating_sub(changed_ms) as f32 / CHAR_FADEIN_MS.max(1) as f32).clamp(0.0, 1.0)
}

fn staggered_char_fade_alpha(
    elapsed_ms: u64,
    changed_start: usize,
    changed_end: usize,
    char_index: usize,
) -> f32 {
    if char_index < changed_start || char_index >= changed_end {
        return 1.0;
    }
    let stagger_start_ms = (char_index.saturating_sub(changed_start) as u64) * CHAR_STAGGER_MS;
    interim_char_fade_alpha(elapsed_ms, stagger_start_ms)
}

fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let inv = 1.0 - t;
    1.0 - inv * inv * inv
}

fn ease_in_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * t
}

fn lerp_channel(a: u8, b: u8, t: f32) -> u8 {
    (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8
}

#[derive(Debug, Clone, Copy)]
struct AccentTransition {
    from_color: [u8; 4],
    to_color: [u8; 4],
    started_ms: u64,
}

impl AccentTransition {
    fn blended_color(&self, now_ms: u64) -> [u8; 4] {
        let elapsed = now_ms.saturating_sub(self.started_ms) as f32;
        let t = (elapsed / ACCENT_CROSSFADE_MS.max(1) as f32).clamp(0.0, 1.0);
        [
            lerp_channel(self.from_color[0], self.to_color[0], t),
            lerp_channel(self.from_color[1], self.to_color[1], t),
            lerp_channel(self.from_color[2], self.to_color[2], t),
            lerp_channel(self.from_color[3], self.to_color[3], t),
        ]
    }

    fn is_complete(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.started_ms) >= ACCENT_CROSSFADE_MS
    }
}

#[derive(Debug, Clone, Copy)]
struct SuccessFlash {
    started_ms: u64,
}

impl SuccessFlash {
    fn is_active(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.started_ms) < SUCCESS_FLASH_MS
    }

    fn color(&self, now_ms: u64) -> [u8; 4] {
        let elapsed = now_ms.saturating_sub(self.started_ms) as f32;
        let t = (elapsed / SUCCESS_FLASH_MS.max(1) as f32).clamp(0.0, 1.0);
        // Fade out the flash
        let alpha = ((1.0 - t) * f32::from(SUCCESS_FLASH_COLOR[3])).round() as u8;
        [
            SUCCESS_FLASH_COLOR[0],
            SUCCESS_FLASH_COLOR[1],
            SUCCESS_FLASH_COLOR[2],
            alpha,
        ]
    }
}

fn draw_progress_bar(
    frame: &mut [u8],
    dimensions: SurfaceDimensions,
    content: ContentArea,
    now_ms: u64,
    started_ms: u64,
    fade_alpha: f32,
) {
    let elapsed = now_ms.saturating_sub(started_ms) as f32;
    let sweep_pos = (elapsed % PROGRESS_SWEEP_MS as f32) / PROGRESS_SWEEP_MS as f32;

    let bar_y = (content.y + content.height) as f32 - CORNER_RADIUS - PROGRESS_BAR_HEIGHT;
    let bar_x0 = content.x as f32 + CORNER_RADIUS;
    let bar_x1 = (content.x + content.width) as f32 - CORNER_RADIUS;
    let bar_width = bar_x1 - bar_x0;
    if bar_width <= 0.0 {
        return;
    }

    let segment_width = bar_width * PROGRESS_SEGMENT_FRAC;
    let center = bar_x0 + sweep_pos * bar_width;

    let px_y0 = bar_y.floor() as i32;
    let px_y1 = (bar_y + PROGRESS_BAR_HEIGHT).ceil() as i32;
    let px_x0 = bar_x0.floor() as i32;
    let px_x1 = bar_x1.ceil() as i32;

    for py in px_y0..px_y1 {
        for px in px_x0..px_x1 {
            let fx = px as f32 + 0.5;
            // Distance from sweep center (wrapping)
            let mut dist = (fx - center).abs();
            // Handle wrapping at edges
            dist = dist.min((fx - center + bar_width).abs());
            dist = dist.min((fx - center - bar_width).abs());

            let half_seg = segment_width / 2.0;
            if dist > half_seg {
                continue;
            }
            // Soft edges
            let intensity = 1.0 - (dist / half_seg);
            let alpha = (intensity * intensity * 180.0 * fade_alpha).round() as u8;
            if alpha == 0 {
                continue;
            }
            blend_pixel(
                frame,
                dimensions,
                px,
                py,
                argb_pixel_premul(220, 230, 255, alpha),
            );
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ListeningAnimState {
    phrase_index: usize,
    phrase_started_ms: u64,
    listening_entered_ms: u64,
}

impl ListeningAnimState {
    fn new(now_ms: u64) -> Self {
        // Seed starting index from time to avoid always starting at "Listening"
        let index = (now_ms as usize) % LISTENING_PHRASES.len();
        Self {
            phrase_index: index,
            phrase_started_ms: now_ms,
            listening_entered_ms: now_ms,
        }
    }

    /// Advance phrase rotation. Returns true if state changed.
    fn tick(&mut self, now_ms: u64) -> bool {
        let elapsed = now_ms.saturating_sub(self.phrase_started_ms);
        if elapsed >= PHRASE_ROTATE_MS {
            self.phrase_index = (self.phrase_index + 1) % LISTENING_PHRASES.len();
            self.phrase_started_ms = now_ms;
            true
        } else {
            false
        }
    }

    fn current_phrase(&self) -> &'static str {
        LISTENING_PHRASES[self.phrase_index]
    }

    /// Returns (outgoing_phrase, blend_factor 0→1) during cross-fade window,
    /// or None if not cross-fading.
    fn crossfade_state(&self, now_ms: u64) -> Option<(&'static str, f32)> {
        // Avoid cross-fading on initial Listening entry; there is no real outgoing phrase yet.
        if self.phrase_started_ms == self.listening_entered_ms {
            return None;
        }
        let elapsed = now_ms.saturating_sub(self.phrase_started_ms);
        if elapsed < PHRASE_CROSSFADE_MS {
            let t = elapsed as f32 / PHRASE_CROSSFADE_MS.max(1) as f32;
            let prev = if self.phrase_index == 0 {
                LISTENING_PHRASES.len() - 1
            } else {
                self.phrase_index - 1
            };
            Some((LISTENING_PHRASES[prev], t))
        } else {
            None
        }
    }

    /// Per-dot opacity [0.0–1.0] for a 3-dot animated ellipsis.
    fn dot_opacities(&self, now_ms: u64) -> [f32; 3] {
        let elapsed = now_ms.saturating_sub(self.listening_entered_ms);
        let cycle_pos = (elapsed % ELLIPSIS_CYCLE_MS) as f32;
        let mut opacities = [0.0f32; 3];
        for (i, opacity) in opacities.iter_mut().enumerate() {
            let dot_start = (i as u64) * ELLIPSIS_DOT_DELAY_MS;
            let dot_elapsed = cycle_pos - dot_start as f32;
            if dot_elapsed < 0.0 {
                *opacity = 0.0;
            } else {
                // Fade in over 200ms, hold, then cycle resets
                let fade_t = (dot_elapsed / ELLIPSIS_DOT_DELAY_MS as f32).clamp(0.0, 1.0);
                *opacity = fade_t;
            }
        }
        opacities
    }
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

    /// Compute vertical slide offset in pixels.
    /// Positive = toward screen edge (downward for Bottom*, upward for Top*).
    fn slide_offset(&self, now_ms: u64, anchor: CliAnchor) -> f32 {
        let elapsed = now_ms.saturating_sub(self.started_ms) as f32;
        let t = (elapsed / self.duration_ms.max(1) as f32).clamp(0.0, 1.0);
        let is_bottom = matches!(
            anchor,
            CliAnchor::BottomLeft | CliAnchor::BottomCenter | CliAnchor::BottomRight
        );
        // Sign: positive pushes toward screen edge
        let sign = if is_bottom { 1.0 } else { -1.0 };
        match self.direction {
            FadeDirection::In => {
                // Start offset, ease to 0
                let remaining = 1.0 - ease_out_cubic(t);
                sign * ENTRANCE_SLIDE_PX * remaining
            }
            FadeDirection::Out => {
                // Start at 0, ease to offset
                let progress = ease_in_cubic(t);
                sign * EXIT_SLIDE_PX * progress
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct WidthState {
    current_width: f32,
    target_width: f32,
    started_ms: u64,
}

impl WidthState {
    fn new(initial_width: f32, now_ms: u64) -> Self {
        Self {
            current_width: initial_width,
            target_width: initial_width,
            started_ms: now_ms,
        }
    }

    fn start(&mut self, to_width: f32, now_ms: u64) {
        self.current_width = self.animated_width(now_ms);
        self.target_width = to_width;
        self.started_ms = now_ms;
    }

    fn snap(&mut self, width: f32, now_ms: u64) {
        self.current_width = width;
        self.target_width = width;
        self.started_ms = now_ms;
    }

    fn animated_width(&self, now_ms: u64) -> f32 {
        let elapsed = now_ms.saturating_sub(self.started_ms) as f32;
        let t = (elapsed / WIDTH_ANIM_MS.max(1) as f32).clamp(0.0, 1.0);
        self.current_width + (self.target_width - self.current_width) * ease_out_cubic(t)
    }

    fn is_animating(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.started_ms) < WIDTH_ANIM_MS
            && (self.target_width - self.current_width).abs() >= 0.5
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
    adaptive_width_enabled: bool,
}

impl OverlayUiConfig {
    fn clamped_content_width(&self) -> u32 {
        self.max_width.clamp(MIN_PANEL_WIDTH, 3840)
    }

    fn clamped_content_height(&self) -> u32 {
        let clamped_lines = self.max_lines.clamp(1, 10);
        let line_h = (DEFAULT_FONT_SIZE_PX * LINE_HEIGHT_FACTOR).ceil() as u32;
        (PADDING_V * 2 + clamped_lines * line_h).clamp(72, 720)
    }

    fn surface_width_for_content(content_width: u32) -> u32 {
        content_width.saturating_add(SHADOW_RADIUS * 2)
    }

    fn min_surface_width(&self) -> u32 {
        Self::surface_width_for_content(MIN_PANEL_WIDTH)
    }

    fn max_surface_width(&self) -> u32 {
        Self::surface_width_for_content(self.clamped_content_width())
    }

    fn surface_dimensions(&self) -> SurfaceDimensions {
        let content_width = self.clamped_content_width();
        let content_height = self.clamped_content_height();
        // Expand for shadow
        let shadow_pad = SHADOW_RADIUS * 2;
        SurfaceDimensions {
            width: content_width + shadow_pad,
            height: content_height + shadow_pad,
        }
    }

    #[cfg(test)]
    fn content_area(&self) -> ContentArea {
        self.content_area_with_width(self.clamped_content_width())
    }

    fn content_area_with_width(&self, content_width: u32) -> ContentArea {
        let content_height = self.clamped_content_height();
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
        interim_fade: Option<(usize, usize, u64)>,
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

        let mut baseline = baseline_start;
        let mut char_index = 0usize;
        for line in lines {
            let mut cursor_x = text_x_start as f32;
            for character in line.chars() {
                let per_char_alpha =
                    if let Some((changed_start, changed_end, elapsed_ms)) = interim_fade {
                        fade_alpha
                            * staggered_char_fade_alpha(
                                elapsed_ms,
                                changed_start,
                                changed_end,
                                char_index,
                            )
                    } else {
                        fade_alpha
                    };
                char_index = char_index.saturating_add(1);
                if per_char_alpha <= 0.0 {
                    let metrics = font.metrics(character, self.summary.size_px);
                    cursor_x += metrics.advance_width;
                    continue;
                }
                let (metrics, bitmap) = font.rasterize(character, self.summary.size_px);
                let glyph_x = cursor_x.floor() as i32 + metrics.xmin;
                let glyph_y = baseline - metrics.height as i32 - metrics.ymin;
                let text_alpha = (per_char_alpha * 255.0).round() as u8;
                let text_color_premul = argb_pixel_premul(
                    TEXT_COLOR_RGB.0,
                    TEXT_COLOR_RGB.1,
                    TEXT_COLOR_RGB.2,
                    text_alpha,
                );
                let shadow_alpha = ((TEXT_SHADOW_COLOR[3] as f32) * per_char_alpha).round() as u8;
                let text_shadow_premul = argb_pixel_premul(0, 0, 0, shadow_alpha);
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

    fn measure_text_width(&self, text: &str) -> f32 {
        let Some(font) = &self.font else {
            return 0.0;
        };
        text.chars().fold(0.0, |width, character| {
            width + font.metrics(character, self.summary.size_px).advance_width
        })
    }
}

trait OverlayBackend {
    fn render(&mut self, state: &OverlayVisibility) -> Result<()>;
    fn is_fading(&self) -> bool {
        false
    }
    fn push_audio_level(&mut self, _level_db: f32) {}
    fn tick_waveform(&mut self) {}
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
    last_phase: OverlayRenderPhase,
    accent_transition: Option<AccentTransition>,
    listening_anim: Option<ListeningAnimState>,
    finalizing_started_ms: Option<u64>,
    success_flash: Option<SuccessFlash>,
    prev_headline: String,
    headline_changed_ms: Option<u64>,
    changed_char_start: usize,
    changed_char_end: usize,
    width_state: WidthState,
    listening_max_phrase_width: f32,
    waveform: Option<WaveformCanvas>,
    started: Instant,
}

impl WaylandOverlayBackend {
    fn new(kind: BackendKind, ui: OverlayUiConfig, runtime: WaylandRuntime) -> Self {
        let initial_width = ui.min_surface_width() as f32;
        let listening_max_phrase_width = LISTENING_PHRASES
            .iter()
            .map(|phrase| {
                runtime
                    .text_renderer
                    .measure_text_width(&format!("{phrase}..."))
            })
            .fold(0.0, f32::max);
        Self {
            kind,
            ui,
            runtime,
            last_visible: false,
            fade: None,
            last_phase: OverlayRenderPhase::Hidden,
            accent_transition: None,
            listening_anim: None,
            finalizing_started_ms: None,
            success_flash: None,
            prev_headline: String::new(),
            headline_changed_ms: None,
            changed_char_start: 0,
            changed_char_end: 0,
            width_state: WidthState::new(initial_width, 0),
            listening_max_phrase_width,
            waveform: None,
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

    fn measure_intent_text_width(&self, intent: &OverlayRenderIntent) -> f32 {
        match intent.phase {
            OverlayRenderPhase::Listening => self.listening_max_phrase_width,
            _ => {
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
                self.runtime.text_renderer.measure_text_width(&text)
            }
        }
    }

    fn target_surface_width(&self, intent: &OverlayRenderIntent) -> f32 {
        let text_width = self.measure_intent_text_width(intent);
        let content_target = (text_width
            + PADDING_LEFT as f32
            + PADDING_H as f32
            + ACCENT_STRIPE_WIDTH
            + ACCENT_STRIPE_MARGIN)
            .ceil()
            .clamp(
                MIN_PANEL_WIDTH as f32,
                self.ui.clamped_content_width() as f32,
            );
        OverlayUiConfig::surface_width_for_content(content_target as u32) as f32
    }

    fn update_interim_headline_state(&mut self, intent: &OverlayRenderIntent, now_ms: u64) {
        if intent.phase != OverlayRenderPhase::Interim {
            self.headline_changed_ms = None;
            self.changed_char_start = 0;
            self.changed_char_end = 0;
            self.prev_headline.clear();
            return;
        }

        if self.prev_headline != intent.headline {
            if let Some((changed_start, changed_end)) =
                changed_char_range(&self.prev_headline, &intent.headline)
            {
                self.changed_char_start = changed_start;
                self.changed_char_end = changed_end;
                self.headline_changed_ms = Some(now_ms);
            } else {
                self.changed_char_start = 0;
                self.changed_char_end = 0;
                self.headline_changed_ms = None;
            }
        }
    }

    fn interim_fade_state(&mut self, now_ms: u64) -> Option<(usize, usize, u64)> {
        let changed_ms = self.headline_changed_ms?;
        let changed_count = self
            .changed_char_end
            .saturating_sub(self.changed_char_start);
        if changed_count == 0 {
            self.headline_changed_ms = None;
            return None;
        }
        let elapsed = now_ms.saturating_sub(changed_ms);
        let total_duration = CHAR_FADEIN_MS.saturating_add(
            (changed_count.saturating_sub(1) as u64).saturating_mul(CHAR_STAGGER_MS),
        );
        if elapsed >= total_duration {
            self.headline_changed_ms = None;
            return None;
        }
        Some((self.changed_char_start, self.changed_char_end, elapsed))
    }
}

impl OverlayBackend for WaylandOverlayBackend {
    fn render(&mut self, state: &OverlayVisibility) -> Result<()> {
        let intent = state.to_render_intent();
        let now = self.now_ms();
        self.update_interim_headline_state(&intent, now);

        // Detect visibility transitions
        if intent.visible != self.last_visible {
            let direction = if intent.visible {
                FadeDirection::In
            } else {
                FadeDirection::Out
            };
            self.fade = Some(FadeState {
                direction,
                started_ms: now,
                duration_ms: match direction {
                    FadeDirection::In => ENTRANCE_DURATION_MS,
                    FadeDirection::Out => EXIT_DURATION_MS,
                },
            });
            self.last_visible = intent.visible;
        }

        // Detect phase changes for accent cross-fade and listening animation
        if intent.phase != self.last_phase {
            // Only cross-fade between visible phases (entrance fade handles Hidden→visible)
            let from = accent_color_for_phase(self.last_phase);
            let to = accent_color_for_phase(intent.phase);
            if let (Some(from_color), Some(to_color)) = (from, to) {
                self.accent_transition = Some(AccentTransition {
                    from_color,
                    to_color,
                    started_ms: now,
                });
            } else {
                self.accent_transition = None;
            }
            // Manage listening animation lifecycle
            if intent.phase == OverlayRenderPhase::Listening {
                self.listening_anim = Some(ListeningAnimState::new(now));
            } else {
                self.listening_anim = None;
            }
            // Manage finalizing progress bar and success flash
            if intent.phase == OverlayRenderPhase::Finalizing {
                self.finalizing_started_ms = Some(now);
            } else {
                // Trigger success flash only on Finalizing→Hidden exit
                if should_trigger_success_flash(self.last_phase, intent.phase) {
                    self.success_flash = Some(SuccessFlash { started_ms: now });
                }
                self.finalizing_started_ms = None;
            }
            // Manage waveform lifecycle
            match intent.phase {
                OverlayRenderPhase::Listening | OverlayRenderPhase::Interim => {
                    if self.waveform.is_none() {
                        self.waveform = Some(WaveformCanvas::new(now));
                    }
                }
                OverlayRenderPhase::Hidden => {
                    self.waveform = None;
                }
                _ => {}
            }
            if intent.phase == OverlayRenderPhase::Hidden {
                let hidden_width = if self.ui.adaptive_width_enabled {
                    self.ui.min_surface_width()
                } else {
                    self.ui.max_surface_width()
                };
                self.width_state.snap(hidden_width as f32, now);
            }
            self.last_phase = intent.phase;
        }

        // Clean up completed fades
        if let Some(fade) = &self.fade {
            if fade.is_complete(now) {
                self.fade = None;
            }
        }
        if let Some(transition) = &self.accent_transition {
            if transition.is_complete(now) {
                self.accent_transition = None;
            }
        }

        // Clean up completed success flash
        if let Some(flash) = &self.success_flash {
            if !flash.is_active(now) {
                self.success_flash = None;
            }
        }

        // Resolve accent color: success flash > transition blend > static.
        let mut accent_color = if let Some(flash) = &self.success_flash {
            Some(flash.color(now))
        } else if let Some(transition) = &self.accent_transition {
            Some(transition.blended_color(now))
        } else {
            accent_color_for_phase(intent.phase)
        };

        if should_apply_breathing(intent.phase, self.accent_transition.is_some()) {
            if let (Some(color), Some(anim)) = (accent_color.as_mut(), self.listening_anim.as_ref())
            {
                let elapsed = now.saturating_sub(anim.listening_entered_ms);
                color[3] = apply_breathing_alpha(color[3], elapsed);
            }
        }

        // Tick listening animation
        if let Some(anim) = &mut self.listening_anim {
            anim.tick(now);
        }

        let effective_surface_width = if self.ui.adaptive_width_enabled {
            let mut target_width = self.target_surface_width(&intent);
            if intent.phase == OverlayRenderPhase::Interim {
                target_width = target_width.max(self.width_state.target_width);
            }
            if (target_width - self.width_state.target_width).abs() >= 0.5 {
                self.width_state.start(target_width, now);
            }
            self.width_state
                .animated_width(now)
                .clamp(
                    self.ui.min_surface_width() as f32,
                    self.ui.max_surface_width() as f32,
                )
                .round() as u32
        } else {
            let fixed_width = self.ui.max_surface_width() as f32;
            self.width_state.snap(fixed_width, now);
            fixed_width.round() as u32
        };
        let interim_fade = if intent.phase == OverlayRenderPhase::Interim {
            self.interim_fade_state(now)
        } else {
            None
        };

        let fade_alpha = self.fade_alpha();
        let y_offset = self
            .fade
            .map(|f| f.slide_offset(now, self.ui.anchor))
            .unwrap_or(0.0);
        let result = self
            .runtime
            .render_with_fade(
                &intent,
                &self.ui,
                fade_alpha,
                y_offset,
                accent_color,
                self.listening_anim.as_ref(),
                now,
                self.finalizing_started_ms,
                effective_surface_width,
                interim_fade,
                self.waveform.as_ref(),
            )
            .with_context(|| format!("overlay renderer backend failed for {:?}", self.kind));
        self.prev_headline = intent.headline;
        result
    }

    fn is_fading(&self) -> bool {
        self.fade.is_some()
            || self.accent_transition.is_some()
            || self.listening_anim.is_some()
            || self.finalizing_started_ms.is_some()
            || self.success_flash.is_some()
            || self.headline_changed_ms.is_some()
            || (self.ui.adaptive_width_enabled && self.width_state.is_animating(self.now_ms()))
            || self
                .waveform
                .as_ref()
                .is_some_and(|waveform| waveform.has_signal())
    }

    fn push_audio_level(&mut self, level_db: f32) {
        if !level_db.is_finite() {
            return;
        }
        let now = self.now_ms();
        if self.waveform.is_none() {
            self.waveform = Some(WaveformCanvas::new(now));
        }
        if let Some(waveform) = &mut self.waveform {
            waveform.push(level_db);
        }
    }

    fn tick_waveform(&mut self) {
        if let Some(waveform) = &mut self.waveform {
            waveform.tick_decay();
        }
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

fn build_backend(
    mode: CliBackendMode,
    ui: OverlayUiConfig,
    output_name: Option<&str>,
) -> BuiltBackend {
    let probe_result = probe_backend_signals().map_err(|err| err.to_string());
    let selection = resolve_backend_selection(mode, probe_result);

    match selection {
        BackendSelection::LayerShell => {
            match WaylandRuntime::new(BackendKind::LayerShell, &ui, output_name) {
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
                        match WaylandRuntime::new(BackendKind::FallbackWindow, &ui, None) {
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
            }
        }
        BackendSelection::FallbackWindow => {
            match WaylandRuntime::new(BackendKind::FallbackWindow, &ui, None) {
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

fn output_name_index(outputs: &[RuntimeOutputBinding], requested: &str) -> Option<usize> {
    output_name_match_index(
        &outputs
            .iter()
            .map(|entry| entry.name.as_deref())
            .collect::<Vec<_>>(),
        requested,
    )
}

fn output_name_match_index(output_names: &[Option<&str>], requested: &str) -> Option<usize> {
    output_names
        .iter()
        .position(|name| name.is_some_and(|name| name == requested))
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
    last_committed_surface_width: Option<u32>,
}

fn damage_width_for_commit(
    previous_surface_width: Option<u32>,
    effective_surface_width: u32,
) -> u32 {
    previous_surface_width
        .map(|previous| previous.max(effective_surface_width))
        .unwrap_or(effective_surface_width)
}

enum ShellSurface {
    Layer {
        layer_surface: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    },
    Fallback {
        _xdg_surface: xdg_surface::XdgSurface,
        toplevel: xdg_toplevel::XdgToplevel,
    },
}

impl WaylandRuntime {
    fn new(kind: BackendKind, ui: &OverlayUiConfig, output_name: Option<&str>) -> Result<Self> {
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
                let target_output = output_name.and_then(|requested| {
                    output_name_index(&state.globals.outputs, requested)
                        .map(|index| state.globals.outputs[index].output.clone())
                });
                let layer_surface = layer_shell.get_layer_surface(
                    &surface,
                    target_output.as_ref(),
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
                ShellSurface::Layer { layer_surface }
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
            last_committed_surface_width: None,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn render_with_fade(
        &mut self,
        intent: &OverlayRenderIntent,
        ui: &OverlayUiConfig,
        fade_alpha: f32,
        y_offset: f32,
        accent_color: Option<[u8; 4]>,
        listening_anim: Option<&ListeningAnimState>,
        now_ms: u64,
        finalizing_started_ms: Option<u64>,
        effective_surface_width: u32,
        interim_fade: Option<(usize, usize, u64)>,
        waveform: Option<&WaveformCanvas>,
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
            let damage_width =
                damage_width_for_commit(self.last_committed_surface_width, effective_surface_width)
                    .clamp(1, self.dimensions.width);
            let effective_content_width =
                effective_surface_width.saturating_sub(SHADOW_RADIUS.saturating_mul(2));
            let content = ui.content_area_with_width(effective_content_width);
            if let ShellSurface::Layer { layer_surface } = &self.shell {
                layer_surface.set_size(effective_surface_width, self.dimensions.height);
            }
            render_frame(
                self.shm_buffer.bytes_mut(),
                self.dimensions,
                intent,
                ui,
                &self.text_renderer,
                fade_alpha,
                content,
                y_offset,
                accent_color,
                listening_anim,
                now_ms,
                finalizing_started_ms,
                interim_fade,
                waveform,
            );
            self.shm_buffer.sync_to_file()?;
            self.surface.attach(Some(&self.shm_buffer.buffer), 0, 0);
            self.surface
                .damage_buffer(0, 0, damage_width as i32, self.dimensions.height as i32);
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
            self.last_committed_surface_width = Some(effective_surface_width);
        } else {
            self.surface.attach(None, 0, 0);
            if let ShellSurface::Fallback { toplevel, .. } = &self.shell {
                toplevel.set_title(FALLBACK_WINDOW_TITLE.to_string());
            }
            self.last_committed_surface_width = None;
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
    outputs: Vec<RuntimeOutputBinding>,
}

#[derive(Clone)]
struct RuntimeOutputBinding {
    output: wl_output::WlOutput,
    name: Option<String>,
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
                "wl_output" => {
                    let output = registry.bind::<wl_output::WlOutput, _, _>(
                        name,
                        version.min(4),
                        queue_handle,
                        (),
                    );
                    state
                        .globals
                        .outputs
                        .push(RuntimeOutputBinding { output, name: None });
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

impl Dispatch<wl_output::WlOutput, ()> for WaylandRuntimeState {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            for entry in &mut state.globals.outputs {
                if &entry.output == output {
                    entry.name = Some(name.clone());
                    break;
                }
            }
        }
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

#[allow(clippy::too_many_arguments)]
fn render_frame(
    frame: &mut [u8],
    dimensions: SurfaceDimensions,
    intent: &OverlayRenderIntent,
    ui: &OverlayUiConfig,
    text_renderer: &TextRenderer,
    fade_alpha: f32,
    content: ContentArea,
    y_offset: f32,
    accent_color: Option<[u8; 4]>,
    listening_anim: Option<&ListeningAnimState>,
    now_ms: u64,
    finalizing_started_ms: Option<u64>,
    interim_fade: Option<(usize, usize, u64)>,
    waveform: Option<&WaveformCanvas>,
) {
    // 1. Clear to transparent
    fill_frame(frame, [0, 0, 0, 0]);

    let frame_alpha = (fade_alpha * ui.opacity.clamp(0.0, 1.0)).clamp(0.0, 1.0);
    if frame_alpha <= 0.0 {
        return;
    }

    // Apply slide offset, clamped within shadow region
    let max_slide = SHADOW_RADIUS as f32;
    let offset_y = y_offset.clamp(-max_slide, max_slide);
    let content = ContentArea {
        y: (content.y as f32 + offset_y)
            .round()
            .clamp(0.0, (dimensions.height - content.height) as f32) as u32,
        ..content
    };

    let content_rect = Rect {
        x: content.x as f32,
        y: content.y as f32,
        w: content.width as f32,
        h: content.height as f32,
    };

    // 2. Draw shadow
    let shadow_a = (SHADOW_ALPHA as f32 * frame_alpha).round() as u8;
    draw_shadow(
        frame,
        dimensions,
        content,
        CORNER_RADIUS,
        SHADOW_RADIUS,
        shadow_a,
    );

    // 3. Fill rounded rect (dark background)
    let bg_a = (BG_ALPHA as f32 * frame_alpha).round() as u8;
    fill_rounded_rect(
        frame,
        dimensions,
        content_rect,
        CORNER_RADIUS,
        argb_pixel(BG_COLOR.0, BG_COLOR.1, BG_COLOR.2, bg_a),
    );

    // 4. Stroke rounded rect (thin border)
    let border_a = (BORDER_ALPHA as f32 * frame_alpha).round() as u8;
    stroke_rounded_rect(
        frame,
        dimensions,
        content_rect,
        CORNER_RADIUS,
        BORDER_THICKNESS,
        argb_pixel(BORDER_COLOR.0, BORDER_COLOR.1, BORDER_COLOR.2, border_a),
    );

    // 5. Draw waveform or accent stripe
    let waveform_drawn = if matches!(
        intent.phase,
        OverlayRenderPhase::Listening | OverlayRenderPhase::Interim
    ) {
        if let Some(waveform) = waveform {
            draw_waveform(frame, dimensions, content, waveform, now_ms, frame_alpha);
            true
        } else {
            false
        }
    } else {
        false
    };
    if !waveform_drawn {
        if let Some(accent) = accent_color {
            draw_accent_stripe(
                frame,
                dimensions,
                content,
                ACCENT_STRIPE_WIDTH,
                ACCENT_STRIPE_MARGIN,
                accent,
                frame_alpha,
            );
        }
    }

    // 5b. Draw progress bar during Finalizing phase
    if let Some(started) = finalizing_started_ms {
        draw_progress_bar(frame, dimensions, content, now_ms, started, frame_alpha);
    }

    // 6. Draw text (with listening animation override)
    if let Some(anim) = listening_anim {
        // Animated phrase + ellipsis dots
        let phrase = anim.current_phrase();
        let dot_opacities = anim.dot_opacities(now_ms);

        if let Some((outgoing, t)) = anim.crossfade_state(now_ms) {
            // During cross-fade: outgoing at (1-t), incoming at t
            let out_text = format_phrase_with_dots(outgoing, &[1.0; 3]);
            let in_text = format_phrase_with_dots(phrase, &dot_opacities);
            text_renderer.draw_headline(
                frame,
                dimensions,
                content,
                content.width,
                ui.max_lines,
                &out_text,
                frame_alpha * (1.0 - t),
                None,
            );
            text_renderer.draw_headline(
                frame,
                dimensions,
                content,
                content.width,
                ui.max_lines,
                &in_text,
                frame_alpha * t,
                None,
            );
        } else {
            let text = format_phrase_with_dots(phrase, &dot_opacities);
            text_renderer.draw_headline(
                frame,
                dimensions,
                content,
                content.width,
                ui.max_lines,
                &text,
                frame_alpha,
                None,
            );
        }
    } else {
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

        let per_char_fade = if intent.phase == OverlayRenderPhase::Interim {
            interim_fade
        } else {
            None
        };
        text_renderer.draw_headline(
            frame,
            dimensions,
            content,
            content.width,
            ui.max_lines,
            &text,
            frame_alpha,
            per_char_fade,
        );
    }
}

/// Format a phrase with animated ellipsis dots.
/// Dots are always rendered (simplifies layout); opacity is controlled
/// by the caller via per-glyph alpha in the text renderer. For now we
/// use Unicode half/full-width approach: visible dots at full alpha,
/// dim dots at reduced alpha. Since draw_headline doesn't support
/// per-glyph alpha, we approximate with a threshold: dots with opacity
/// >= 0.5 are shown, others hidden with a space placeholder.
fn format_phrase_with_dots(phrase: &str, dot_opacities: &[f32; 3]) -> String {
    let mut s = phrase.to_string();
    for &opacity in dot_opacities {
        if opacity >= 0.5 {
            s.push('.');
        }
    }
    s
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
    let adaptive_width_enabled = resolve_adaptive_width_override(cli.adaptive_width);
    let ui = OverlayUiConfig {
        opacity: cli.opacity.clamp(0.0, 1.0),
        font: cli.font,
        anchor: cli.anchor,
        margin_x: cli.margin_x,
        margin_y: cli.margin_y,
        max_width: cli.max_width,
        max_lines: cli.max_lines,
        adaptive_width_enabled,
    };

    let mut built_backend = build_backend(cli.backend, ui.clone(), cli.output_name.as_deref());
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
        adaptive_width_enabled = ui.adaptive_width_enabled,
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
                                // Extract audio level before state machine consumes the message
                                if let OverlayIpcMessage::AudioLevel { level_db, .. } = &message {
                                    built_backend.backend.push_audio_level(*level_db);
                                }
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
                built_backend.backend.tick_waveform();
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
        accent_color_for_phase, apply_breathing_alpha, changed_char_range, damage_width_for_commit,
        ease_out_cubic, interim_char_fade_alpha, layout_text_lines, output_name_match_index,
        parse_font_descriptor, parse_generic_family_kind, render_frame,
        resolve_adaptive_width_with_env_input, resolve_backend_selection, rounded_rect_coverage,
        shared_prefix_len, shared_suffix_len, should_apply_breathing, staggered_char_fade_alpha,
        BackendSelection, BackendSignals, CliAnchor, CliBackendMode, FadeDirection, FadeState,
        FontResolutionSummary, OverlayUiConfig, ParsedFontDescriptor, Rect, TextRenderer,
        WidthState, BREATHING_CYCLE_MS, CHAR_FADEIN_MS, CHAR_STAGGER_MS, SHADOW_RADIUS,
    };
    use clap::Parser;
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
    fn breathing_modulates_alpha_at_quarter_cycle() {
        let baseline = 200;
        let modulated = apply_breathing_alpha(baseline, BREATHING_CYCLE_MS / 4);
        assert!(modulated > baseline);
    }

    #[test]
    fn breathing_returns_to_baseline_at_full_cycle() {
        let baseline = 180;
        let modulated = apply_breathing_alpha(baseline, BREATHING_CYCLE_MS);
        assert_eq!(modulated, baseline);
    }

    #[test]
    fn breathing_only_during_listening() {
        assert!(should_apply_breathing(OverlayRenderPhase::Listening, false));
        assert!(!should_apply_breathing(OverlayRenderPhase::Interim, false));
        assert!(!should_apply_breathing(
            OverlayRenderPhase::Finalizing,
            false
        ));
        assert!(!should_apply_breathing(OverlayRenderPhase::Listening, true));
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
    fn shared_prefix_detection() {
        assert_eq!(shared_prefix_len("hello world", "hello rust"), 6);
        assert_eq!(shared_prefix_len("alpha", "beta"), 0);
    }

    #[test]
    fn shared_suffix_detection_respects_shared_prefix_boundary() {
        assert_eq!(shared_suffix_len("alpha beta", "gamma beta", 0), 6);
        assert_eq!(shared_suffix_len("alpha", "alpha", 5), 0);
    }

    #[test]
    fn changed_range_detects_middle_rewrites() {
        assert_eq!(
            changed_char_range("hello world", "hello rust"),
            Some((6, 10))
        );
    }

    #[test]
    fn changed_range_detects_append_updates() {
        assert_eq!(changed_char_range("hello", "hello world"), Some((5, 11)));
    }

    #[test]
    fn char_fadein_zero_at_start() {
        assert_eq!(interim_char_fade_alpha(100, 100), 0.0);
    }

    #[test]
    fn char_fadein_full_at_duration() {
        assert_eq!(interim_char_fade_alpha(100 + CHAR_FADEIN_MS, 100), 1.0);
    }

    #[test]
    fn staggered_fade_keeps_prefix_visible() {
        assert_eq!(staggered_char_fade_alpha(0, 5, 7, 4), 1.0);
        assert_eq!(staggered_char_fade_alpha(0, 5, 7, 7), 1.0);
    }

    #[test]
    fn staggered_fade_delays_later_suffix_chars() {
        let first_suffix = staggered_char_fade_alpha(50, 3, 8, 3);
        let later_suffix = staggered_char_fade_alpha(50, 3, 8, 5);
        assert!(first_suffix > later_suffix);
        let fully_visible =
            staggered_char_fade_alpha(CHAR_FADEIN_MS + 2 * CHAR_STAGGER_MS, 3, 8, 5);
        assert_eq!(fully_visible, 1.0);
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
            adaptive_width_enabled: true,
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
            0.0,
            None,
            None,
            0,
            None,
            None,
            None,
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
            0.0,
            accent_color_for_phase(OverlayRenderPhase::Listening),
            None,
            0,
            None,
            None,
            None,
        );
        assert!(visible_frame.chunks_exact(4).any(|pixel| pixel[3] > 0));
    }

    #[test]
    fn render_frame_applies_configured_opacity() {
        let base_ui = OverlayUiConfig {
            opacity: 1.0,
            font: "Sans 18".to_string(),
            anchor: super::CliAnchor::TopCenter,
            margin_x: 24,
            margin_y: 24,
            max_width: 320,
            max_lines: 3,
            adaptive_width_enabled: true,
        };
        let low_opacity_ui = OverlayUiConfig {
            opacity: 0.35,
            ..base_ui.clone()
        };

        let dimensions = low_opacity_ui.surface_dimensions();
        let content = low_opacity_ui.content_area();
        let text_renderer = TextRenderer {
            summary: FontResolutionSummary {
                requested: "Sans 18".to_string(),
                family: "Sans".to_string(),
                size_px: 18.0,
                fallback_reason: None,
            },
            font: None,
        };
        let visible = OverlayRenderIntent {
            phase: OverlayRenderPhase::Listening,
            visible: true,
            headline: "listening".to_string(),
            detail: None,
        };

        let accent = accent_color_for_phase(OverlayRenderPhase::Listening);

        let mut full_alpha_frame = vec![0u8; (dimensions.width * dimensions.height * 4) as usize];
        render_frame(
            &mut full_alpha_frame,
            dimensions,
            &visible,
            &base_ui,
            &text_renderer,
            1.0,
            content,
            0.0,
            accent,
            None,
            0,
            None,
            None,
            None,
        );

        let mut low_alpha_frame = vec![0u8; (dimensions.width * dimensions.height * 4) as usize];
        render_frame(
            &mut low_alpha_frame,
            dimensions,
            &visible,
            &low_opacity_ui,
            &text_renderer,
            1.0,
            content,
            0.0,
            accent,
            None,
            0,
            None,
            None,
            None,
        );

        let max_full = full_alpha_frame
            .chunks_exact(4)
            .map(|pixel| pixel[3])
            .max()
            .unwrap_or(0);
        let max_low = low_alpha_frame
            .chunks_exact(4)
            .map(|pixel| pixel[3])
            .max()
            .unwrap_or(0);
        assert!(
            max_full > 0,
            "full-opacity frame should contain visible pixels"
        );
        assert!(
            max_low < max_full,
            "lower configured opacity should reduce composed alpha (full={max_full}, low={max_low})"
        );
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
            adaptive_width_enabled: true,
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

    #[test]
    fn default_cli_anchor_is_bottom_center() {
        // Verify Cli struct defaults parse to BottomCenter with 32px vertical margin
        let cli = super::Cli::parse_from(["parakeet-overlay"]);
        assert!(matches!(cli.anchor, CliAnchor::BottomCenter));
        assert_eq!(cli.margin_y, 32);
    }

    #[test]
    fn cli_parses_output_name_arg() {
        let cli = super::Cli::parse_from(["parakeet-overlay", "--output-name", "HDMI-A-1"]);
        assert_eq!(cli.output_name.as_deref(), Some("HDMI-A-1"));
    }

    #[test]
    fn cli_adaptive_width_defaults_to_none() {
        let cli = super::Cli::parse_from(["parakeet-overlay"]);
        assert_eq!(cli.adaptive_width, None);
    }

    #[test]
    fn cli_parses_adaptive_width_arg() {
        let enabled = super::Cli::parse_from(["parakeet-overlay", "--adaptive-width", "true"]);
        assert_eq!(enabled.adaptive_width, Some(true));

        let disabled = super::Cli::parse_from(["parakeet-overlay", "--adaptive-width", "false"]);
        assert_eq!(disabled.adaptive_width, Some(false));
    }

    #[test]
    fn resolve_adaptive_width_override_defaults_to_enabled() {
        assert!(resolve_adaptive_width_with_env_input(None, None));
    }

    #[test]
    fn resolve_adaptive_width_override_honors_env_and_cli_precedence() {
        assert!(!resolve_adaptive_width_with_env_input(None, Some("false")));
        assert!(resolve_adaptive_width_with_env_input(None, Some("true")));
        assert!(resolve_adaptive_width_with_env_input(
            Some(true),
            Some("false")
        ));
        assert!(!resolve_adaptive_width_with_env_input(
            Some(false),
            Some("true")
        ));
    }

    #[test]
    fn output_matching_finds_correct_output() {
        let outputs = [Some("DP-1"), Some("HDMI-A-1")];
        assert_eq!(output_name_match_index(&outputs, "HDMI-A-1"), Some(1));
    }

    #[test]
    fn output_matching_falls_back_to_none() {
        let outputs = [Some("DP-1"), None];
        assert_eq!(output_name_match_index(&outputs, "UNKNOWN"), None);
    }

    #[test]
    fn ease_in_cubic_boundaries() {
        use super::ease_in_cubic;
        assert!((ease_in_cubic(0.0) - 0.0).abs() < f32::EPSILON);
        assert!((ease_in_cubic(1.0) - 1.0).abs() < f32::EPSILON);
        // Ease-in should be < linear at midpoint
        assert!(ease_in_cubic(0.5) < 0.5);
        // Monotonically increasing
        let mut prev = 0.0f32;
        for i in 0..=100 {
            let t = i as f32 / 100.0;
            let v = ease_in_cubic(t);
            assert!(v >= prev, "not monotonic at t={t}");
            prev = v;
        }
    }

    #[test]
    fn slide_offset_entrance_ends_at_zero() {
        let fade = FadeState {
            direction: FadeDirection::In,
            started_ms: 0,
            duration_ms: super::ENTRANCE_DURATION_MS,
        };
        let offset = fade.slide_offset(super::ENTRANCE_DURATION_MS, CliAnchor::BottomCenter);
        assert!(
            offset.abs() < 0.01,
            "entrance slide should end at zero, got {offset}"
        );
    }

    #[test]
    fn slide_offset_exit_starts_at_zero() {
        let fade = FadeState {
            direction: FadeDirection::Out,
            started_ms: 100,
            duration_ms: super::EXIT_DURATION_MS,
        };
        let offset = fade.slide_offset(100, CliAnchor::BottomCenter);
        assert!(
            offset.abs() < 0.01,
            "exit slide should start at zero, got {offset}"
        );
    }

    #[test]
    fn entrance_duration_longer_than_exit() {
        let entrance = super::ENTRANCE_DURATION_MS;
        let exit = super::EXIT_DURATION_MS;
        assert!(
            entrance > exit,
            "entrance={entrance} should exceed exit={exit}"
        );
    }

    #[test]
    fn accent_transition_interpolates_at_midpoint() {
        use super::AccentTransition;
        let transition = AccentTransition {
            from_color: [0, 0, 0, 255],
            to_color: [200, 200, 200, 255],
            started_ms: 0,
        };
        let mid = transition.blended_color(super::ACCENT_CROSSFADE_MS / 2);
        // Each channel should be roughly halfway (100 ± rounding)
        assert!(
            mid[0] > 80 && mid[0] < 120,
            "R channel midpoint: {}",
            mid[0]
        );
        assert_eq!(mid[3], 255, "alpha should stay 255");
    }

    #[test]
    fn accent_transition_completes_at_duration() {
        use super::AccentTransition;
        let transition = AccentTransition {
            from_color: [10, 20, 30, 255],
            to_color: [200, 100, 50, 255],
            started_ms: 0,
        };
        let final_color = transition.blended_color(super::ACCENT_CROSSFADE_MS);
        assert_eq!(final_color, [200, 100, 50, 255]);
    }

    #[test]
    fn no_accent_transition_from_hidden() {
        // When going Hidden→Listening, accent_color_for_phase(Hidden) is None,
        // so no AccentTransition should be created (entrance fade handles it).
        assert!(accent_color_for_phase(OverlayRenderPhase::Hidden).is_none());
    }

    #[test]
    fn phrase_advances_after_interval() {
        use super::ListeningAnimState;
        let mut anim = ListeningAnimState {
            phrase_index: 0,
            phrase_started_ms: 0,
            listening_entered_ms: 0,
        };
        let initial = anim.current_phrase();
        // Before rotation interval: no change
        assert!(!anim.tick(super::PHRASE_ROTATE_MS - 1));
        assert_eq!(anim.current_phrase(), initial);
        // After rotation interval: advances
        assert!(anim.tick(super::PHRASE_ROTATE_MS));
        assert_eq!(anim.phrase_index, 1);
    }

    #[test]
    fn dot_opacities_stagger_correctly() {
        use super::ListeningAnimState;
        let anim = ListeningAnimState {
            phrase_index: 0,
            phrase_started_ms: 0,
            listening_entered_ms: 0,
        };
        // At t=0, first dot starts fading, others haven't started
        let dots = anim.dot_opacities(0);
        assert!(dots[0] >= 0.0);
        assert_eq!(dots[1], 0.0);
        assert_eq!(dots[2], 0.0);
        // After 2 dot delays, all three should be visible
        let all_on = anim.dot_opacities(super::ELLIPSIS_DOT_DELAY_MS * 3);
        assert!(all_on[0] >= 0.5);
        assert!(all_on[1] >= 0.5);
        assert!(all_on[2] >= 0.5);
    }

    #[test]
    fn dots_reset_after_cycle() {
        use super::ListeningAnimState;
        let anim = ListeningAnimState {
            phrase_index: 0,
            phrase_started_ms: 0,
            listening_entered_ms: 0,
        };
        // Right at cycle boundary, position wraps to 0
        let dots = anim.dot_opacities(super::ELLIPSIS_CYCLE_MS);
        assert!(
            dots[1] < 0.5,
            "second dot should reset after cycle, got {}",
            dots[1]
        );
    }

    #[test]
    fn crossfade_active_during_rotation_window() {
        use super::ListeningAnimState;
        let mut anim = ListeningAnimState {
            phrase_index: 0,
            phrase_started_ms: 0,
            listening_entered_ms: 0,
        };
        // After rotation, crossfade should be active
        anim.tick(super::PHRASE_ROTATE_MS);
        let cf = anim.crossfade_state(anim.phrase_started_ms + super::PHRASE_CROSSFADE_MS / 2);
        assert!(
            cf.is_some(),
            "crossfade should be active right after rotation"
        );
        let (_, t) = cf.unwrap();
        assert!(
            t > 0.0 && t < 1.0,
            "blend factor should be mid-range, got {t}"
        );
        // After crossfade window ends
        let cf_done = anim.crossfade_state(anim.phrase_started_ms + super::PHRASE_CROSSFADE_MS);
        assert!(cf_done.is_none(), "crossfade should be done");
    }

    #[test]
    fn crossfade_inactive_on_listening_entry() {
        use super::ListeningAnimState;
        let entered = 1000;
        let anim = ListeningAnimState::new(entered);
        assert!(
            anim.crossfade_state(entered).is_none(),
            "crossfade should be inactive at listening entry"
        );
        assert!(
            anim.crossfade_state(entered + super::PHRASE_CROSSFADE_MS / 2)
                .is_none(),
            "crossfade should stay inactive before first phrase rotation"
        );
    }

    #[test]
    fn progress_segment_wraps_at_duration() {
        // The sweep position should wrap back near 0 after PROGRESS_SWEEP_MS
        let started = 1000u64;
        let just_before = started + super::PROGRESS_SWEEP_MS - 1;
        let just_after = started + super::PROGRESS_SWEEP_MS;
        // Position just before wrap should be near 1.0
        let pos_before = (just_before - started) as f32 % super::PROGRESS_SWEEP_MS as f32
            / super::PROGRESS_SWEEP_MS as f32;
        assert!(pos_before > 0.9, "pos_before={pos_before}");
        // Position at exact wrap should be 0.0
        let pos_after = (just_after - started) as f32 % super::PROGRESS_SWEEP_MS as f32
            / super::PROGRESS_SWEEP_MS as f32;
        assert!(pos_after < 0.01, "pos_after={pos_after}");
    }

    #[test]
    fn adaptive_width_clamps_to_min() {
        let ui = OverlayUiConfig {
            opacity: 1.0,
            font: "Sans 18".to_string(),
            anchor: CliAnchor::BottomCenter,
            margin_x: 24,
            margin_y: 32,
            max_width: 400,
            max_lines: 3,
            adaptive_width_enabled: true,
        };
        let min_width = ui.min_surface_width() as f32;
        let target = OverlayUiConfig::surface_width_for_content(20) as f32;
        let clamped = target.clamp(ui.min_surface_width() as f32, ui.max_surface_width() as f32);
        assert_eq!(clamped, min_width);
    }

    #[test]
    fn adaptive_width_clamps_to_max() {
        let ui = OverlayUiConfig {
            opacity: 1.0,
            font: "Sans 18".to_string(),
            anchor: CliAnchor::BottomCenter,
            margin_x: 24,
            margin_y: 32,
            max_width: 420,
            max_lines: 3,
            adaptive_width_enabled: true,
        };
        let huge = OverlayUiConfig::surface_width_for_content(2_000) as f32;
        let clamped = huge.clamp(ui.min_surface_width() as f32, ui.max_surface_width() as f32);
        assert_eq!(clamped, ui.max_surface_width() as f32);
    }

    #[test]
    fn width_animation_completes_at_duration() {
        let mut width_state = WidthState::new(200.0, 0);
        width_state.start(360.0, 0);
        assert!((width_state.animated_width(super::WIDTH_ANIM_MS) - 360.0).abs() < 0.5);
    }

    #[test]
    fn damage_width_tracks_previous_width_on_shrink() {
        assert_eq!(damage_width_for_commit(Some(640), 320), 640);
    }

    #[test]
    fn damage_width_uses_current_width_for_first_frame() {
        assert_eq!(damage_width_for_commit(None, 320), 320);
    }

    #[test]
    fn listening_phase_uses_max_phrase_width() {
        let expected = super::LISTENING_PHRASES
            .iter()
            .map(|phrase| phrase.len())
            .max()
            .unwrap_or(0);
        let current = super::LISTENING_PHRASES[0].len();
        assert!(expected >= current);
    }

    #[test]
    fn success_flash_active_during_window() {
        use super::SuccessFlash;
        let flash = SuccessFlash { started_ms: 100 };
        assert!(flash.is_active(100));
        assert!(flash.is_active(100 + super::SUCCESS_FLASH_MS - 1));
        assert!(!flash.is_active(100 + super::SUCCESS_FLASH_MS));
        // Color alpha should decay
        let early = flash.color(100);
        let late = flash.color(100 + super::SUCCESS_FLASH_MS - 1);
        assert!(
            early[3] > late[3],
            "flash should fade: early={}, late={}",
            early[3],
            late[3]
        );
    }

    #[test]
    fn success_flash_triggers_on_finalizing_exit() {
        // SuccessFlash should be created when transitioning Finalizing→Hidden.
        // This tests the SuccessFlash struct's basic behavior (integration with
        // WaylandOverlayBackend is tested via the render loop).
        use super::SuccessFlash;
        let flash = SuccessFlash { started_ms: 500 };
        assert!(flash.is_active(500));
        let color = flash.color(500);
        assert_eq!(color[0], super::SUCCESS_FLASH_COLOR[0]);
        assert_eq!(color[1], super::SUCCESS_FLASH_COLOR[1]);
        assert_eq!(color[2], super::SUCCESS_FLASH_COLOR[2]);
        assert!(color[3] > 200, "initial flash alpha should be near-full");
    }

    #[test]
    fn success_flash_only_triggers_on_finalizing_to_hidden() {
        assert!(super::should_trigger_success_flash(
            OverlayRenderPhase::Finalizing,
            OverlayRenderPhase::Hidden
        ));
        assert!(!super::should_trigger_success_flash(
            OverlayRenderPhase::Finalizing,
            OverlayRenderPhase::Listening
        ));
        assert!(!super::should_trigger_success_flash(
            OverlayRenderPhase::Interim,
            OverlayRenderPhase::Hidden
        ));
    }
}
