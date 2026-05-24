use crate::config::PasteShortcut;
use crate::surface_focus::{
    FocusSnapshot, WaylandFocusObservation, WAYLAND_FOCUS_REASON_CACHE_STALE,
    WAYLAND_FOCUS_REASON_TRANSITION_NO_ACTIVATED,
};
use serde::{Deserialize, Serialize};

pub const OVERLAY_OUTPUT_STALE_MS: u64 = 1_500;
pub const OVERLAY_OUTPUT_TRANSITION_GRACE_MS: u64 = 250;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SurfaceClass {
    Terminal,
    General,
    Unknown,
    Forced,
}

impl SurfaceClass {
    pub fn as_report_str(self) -> &'static str {
        match self {
            Self::Terminal => "Terminal",
            Self::General => "General",
            Self::Unknown => "Unknown",
            Self::Forced => "Forced",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FocusConfidence {
    Fresh,
    LowConfidence,
    Unavailable,
}

impl FocusConfidence {
    pub fn as_report_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::LowConfidence => "low_confidence",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FocusRouteInput<'a> {
    pub snapshot: Option<&'a FocusSnapshot>,
    pub source: &'a str,
    pub confidence: FocusConfidence,
}

impl<'a> FocusRouteInput<'a> {
    pub fn new(
        snapshot: Option<&'a FocusSnapshot>,
        source: &'a str,
        confidence: FocusConfidence,
    ) -> Self {
        Self {
            snapshot,
            source,
            confidence,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FocusObservation {
    pub snapshot: Option<FocusSnapshot>,
    pub source_selected: &'static str,
    pub confidence: FocusConfidence,
    pub wayland_cache_age_ms: Option<u64>,
    pub wayland_fallback_reason: Option<&'static str>,
}

impl FocusObservation {
    pub fn from_wayland(observation: WaylandFocusObservation) -> Self {
        match observation {
            WaylandFocusObservation::Fresh {
                snapshot,
                cache_age_ms,
            } => Self {
                snapshot: Some(snapshot),
                source_selected: "wayland_cache",
                confidence: FocusConfidence::Fresh,
                wayland_cache_age_ms: Some(cache_age_ms),
                wayland_fallback_reason: None,
            },
            WaylandFocusObservation::LowConfidence {
                snapshot,
                cache_age_ms,
                reason,
            } => Self {
                snapshot: Some(snapshot),
                source_selected: "wayland_cache_low_confidence",
                confidence: FocusConfidence::LowConfidence,
                wayland_cache_age_ms: Some(cache_age_ms),
                wayland_fallback_reason: Some(reason),
            },
            WaylandFocusObservation::Unavailable {
                reason,
                cache_age_ms,
            } => Self::unavailable(reason, cache_age_ms),
        }
    }

    pub fn unavailable(
        reason: &'static str,
        wayland_cache_age_ms: Option<u64>,
    ) -> FocusObservation {
        Self {
            snapshot: None,
            source_selected: "wayland_unavailable",
            confidence: FocusConfidence::Unavailable,
            wayland_cache_age_ms,
            wayland_fallback_reason: Some(reason),
        }
    }

    pub fn route_input(&self) -> FocusRouteInput<'_> {
        FocusRouteInput::new(
            self.snapshot.as_ref(),
            self.source_selected,
            self.confidence,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShortcutPlan {
    pub primary: PasteShortcut,
    pub adaptive_fallback: Option<PasteShortcut>,
}

impl ShortcutPlan {
    fn new(primary: PasteShortcut, fallback: PasteShortcut) -> Self {
        Self {
            primary,
            adaptive_fallback: dedup_fallback(primary, fallback),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteDecision {
    pub source: String,
    pub focus_confidence: FocusConfidence,
    pub surface_class: SurfaceClass,
    pub shortcut_plan: ShortcutPlan,
    pub output_name: Option<String>,
    pub reason: String,
}

impl RouteDecision {
    pub fn forced(
        source: impl Into<String>,
        focus_confidence: FocusConfidence,
        output_name: Option<String>,
        primary: PasteShortcut,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            source: source.into(),
            focus_confidence,
            surface_class: SurfaceClass::Forced,
            shortcut_plan: ShortcutPlan {
                primary,
                adaptive_fallback: None,
            },
            output_name,
            reason: reason.into(),
        }
    }

    pub fn route_class_name(&self) -> &'static str {
        self.surface_class.as_report_str()
    }
}

const TERMINAL_SHORTCUT: PasteShortcut = PasteShortcut::CtrlShiftV;
const GENERAL_SHORTCUT: PasteShortcut = PasteShortcut::CtrlV;
const UNKNOWN_SHORTCUT: PasteShortcut = PasteShortcut::CtrlShiftV;

const TERMINAL_HINTS: &[&str] = &[
    "ghostty",
    "cosmic term",
    "cosmic terminal",
    "terminal",
    "alacritty",
    "kitty",
    "wezterm",
    "konsole",
    "xterm",
    "tilix",
    "foot",
    "tmux",
    "zellij",
];

const GENERAL_HINTS: &[&str] = &[
    "code",
    "vscode",
    "visual studio code",
    "brave",
    "chromium",
    "chrome",
    "firefox",
    "notion",
    "cosmic edit",
    "gedit",
    "kate",
    "obsidian",
];

const COSMIC_EDIT_HINTS: &[&str] = &[
    "com system76 cosmicedit",
    "cosmicedit",
    "cosmic text editor",
];

pub fn decide_route_for_focus(input: FocusRouteInput<'_>) -> RouteDecision {
    if let Some(snapshot) = input.snapshot {
        if !snapshot.focused {
            return unknown_route(
                input,
                "adaptive low-confidence focus snapshot (focused=false)",
            );
        }
    }

    let (class, class_reason) = classify_surface_with_reason(input.snapshot);
    match class {
        SurfaceClass::Terminal => route_decision(
            input,
            class,
            ShortcutPlan::new(TERMINAL_SHORTCUT, GENERAL_SHORTCUT),
            class_reason,
        ),
        SurfaceClass::General => route_decision(
            input,
            class,
            ShortcutPlan::new(GENERAL_SHORTCUT, TERMINAL_SHORTCUT),
            class_reason,
        ),
        SurfaceClass::Unknown => unknown_route(input, class_reason),
        SurfaceClass::Forced => unreachable!("forced routes are constructed explicitly"),
    }
}

pub fn decide_overlay_output_target(observation: &FocusObservation) -> Option<String> {
    match observation.confidence {
        FocusConfidence::Fresh => observation
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.output_name.clone()),
        FocusConfidence::LowConfidence
            if matches!(
                observation.wayland_fallback_reason,
                Some(
                    WAYLAND_FOCUS_REASON_TRANSITION_NO_ACTIVATED | WAYLAND_FOCUS_REASON_CACHE_STALE
                )
            ) =>
        {
            observation
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.output_name.clone())
        }
        FocusConfidence::LowConfidence | FocusConfidence::Unavailable => None,
    }
}

fn route_decision(
    input: FocusRouteInput<'_>,
    surface_class: SurfaceClass,
    shortcut_plan: ShortcutPlan,
    reason: &'static str,
) -> RouteDecision {
    RouteDecision {
        source: input.source.to_string(),
        focus_confidence: input.confidence,
        surface_class,
        shortcut_plan,
        output_name: input
            .snapshot
            .and_then(|snapshot| snapshot.output_name.as_ref().cloned()),
        reason: reason.to_string(),
    }
}

fn unknown_route(input: FocusRouteInput<'_>, reason: &'static str) -> RouteDecision {
    let alternate = if UNKNOWN_SHORTCUT != GENERAL_SHORTCUT {
        GENERAL_SHORTCUT
    } else {
        TERMINAL_SHORTCUT
    };

    route_decision(
        input,
        SurfaceClass::Unknown,
        ShortcutPlan::new(UNKNOWN_SHORTCUT, alternate),
        reason,
    )
}

fn dedup_fallback(primary: PasteShortcut, fallback: PasteShortcut) -> Option<PasteShortcut> {
    if primary == fallback {
        None
    } else {
        Some(fallback)
    }
}

fn classify_surface_with_reason(focus: Option<&FocusSnapshot>) -> (SurfaceClass, &'static str) {
    let Some(focus) = focus else {
        return (SurfaceClass::Unknown, "adaptive unknown surface");
    };

    let haystack = normalize_for_hint_match(&focus.haystack());
    if contains_any_hint(&haystack, TERMINAL_HINTS) {
        return (SurfaceClass::Terminal, "adaptive terminal-like surface");
    }
    if contains_any_hint(&haystack, COSMIC_EDIT_HINTS) {
        return (SurfaceClass::General, "adaptive cosmic edit surface");
    }
    if contains_any_hint(&haystack, GENERAL_HINTS) {
        return (
            SurfaceClass::General,
            "adaptive editor/browser-like surface",
        );
    }

    (SurfaceClass::Unknown, "adaptive unknown surface")
}

fn contains_any_hint(haystack: &str, hints: &[&str]) -> bool {
    hints.iter().any(|hint| haystack.contains(hint))
}

fn normalize_for_hint_match(raw: &str) -> String {
    let mut normalized = String::with_capacity(raw.len());
    let mut in_gap = false;
    for ch in raw.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            normalized.push(lower);
            in_gap = false;
        } else if !in_gap {
            normalized.push(' ');
            in_gap = true;
        }
    }

    normalized.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        classify_surface_with_reason, decide_overlay_output_target, decide_route_for_focus,
        FocusConfidence, FocusObservation, FocusRouteInput, SurfaceClass,
    };
    use crate::config::PasteShortcut;
    use crate::surface_focus::{
        FocusSnapshot, WaylandFocusObservation, WAYLAND_FOCUS_REASON_CACHE_STALE,
        WAYLAND_FOCUS_REASON_TRANSITION_NO_ACTIVATED,
    };

    fn snapshot(
        app_name: &str,
        object_name: &str,
        object_path: &str,
        focused: bool,
    ) -> FocusSnapshot {
        FocusSnapshot {
            app_name: Some(app_name.to_string()),
            object_name: Some(object_name.to_string()),
            object_path: Some(object_path.to_string()),
            service_name: Some(":1.42".to_string()),
            output_name: None,
            focused,
            active: true,
            resolver: "test".to_string(),
        }
    }

    fn decide_test_route(focus: &FocusSnapshot) -> super::RouteDecision {
        let confidence = if focus.focused {
            FocusConfidence::Fresh
        } else {
            FocusConfidence::LowConfidence
        };
        decide_route_for_focus(FocusRouteInput::new(
            Some(focus),
            "direct_snapshot",
            confidence,
        ))
    }

    #[test]
    fn classifies_terminal_surface() {
        let focus = snapshot("Unnamed", "shell", "/com/mitchellh/ghostty/a11y/abc", false);
        assert_eq!(
            classify_surface_with_reason(Some(&focus)).0,
            SurfaceClass::Terminal
        );
    }

    #[test]
    fn classifies_general_surface() {
        let focus = snapshot(
            "Brave Browser",
            "Codex - Brave",
            "/org/a11y/atspi/accessible/1",
            false,
        );
        assert_eq!(
            classify_surface_with_reason(Some(&focus)).0,
            SurfaceClass::General
        );
    }

    #[test]
    fn classifies_cosmic_edit_app_id_as_general() {
        let focus = snapshot(
            "com.system76.CosmicEdit",
            "Untitled",
            "/org/a11y/atspi/accessible/2",
            true,
        );
        assert_eq!(
            classify_surface_with_reason(Some(&focus)).0,
            SurfaceClass::General
        );
    }

    #[test]
    fn classifies_cosmic_text_editor_title_as_general() {
        let focus = snapshot(
            "Some app",
            "New document - COSMIC Text Editor",
            "/org/a11y/atspi/accessible/3",
            true,
        );
        assert_eq!(
            classify_surface_with_reason(Some(&focus)).0,
            SurfaceClass::General
        );
    }

    #[test]
    fn normalization_handles_dot_dash_underscore_forms() {
        let focus = snapshot(
            "com.system76.cosmic_edit",
            "cosmic-edit",
            "/org/a11y/atspi/accessible/4",
            true,
        );
        assert_eq!(
            classify_surface_with_reason(Some(&focus)).0,
            SurfaceClass::General
        );
    }

    #[test]
    fn unknown_surface_defaults_to_unknown() {
        let focus = snapshot("SomeApp", "random", "/org/example", false);
        assert_eq!(
            classify_surface_with_reason(Some(&focus)).0,
            SurfaceClass::Unknown
        );
    }

    #[test]
    fn adaptive_route_prefers_terminal_shortcut_for_terminals() {
        let focus = snapshot("Unnamed", "shell", "/com/mitchellh/ghostty/a11y/abc", true);
        let decision = decide_test_route(&focus);
        assert_eq!(decision.surface_class, SurfaceClass::Terminal);
        assert_eq!(decision.shortcut_plan.primary, PasteShortcut::CtrlShiftV);
        assert_eq!(
            decision.shortcut_plan.adaptive_fallback,
            Some(PasteShortcut::CtrlV)
        );
        assert_eq!(decision.focus_confidence, FocusConfidence::Fresh);
    }

    #[test]
    fn adaptive_route_uses_unknown_when_snapshot_is_not_focused() {
        let focus = snapshot(
            "Brave Browser",
            "Codex - Brave",
            "/org/a11y/atspi/accessible/1",
            false,
        );
        let decision = decide_test_route(&focus);
        assert_eq!(decision.surface_class, SurfaceClass::Unknown);
        assert_eq!(decision.shortcut_plan.primary, PasteShortcut::CtrlShiftV);
        assert_eq!(
            decision.shortcut_plan.adaptive_fallback,
            Some(PasteShortcut::CtrlV)
        );
        assert_eq!(decision.focus_confidence, FocusConfidence::LowConfidence);
        assert_eq!(
            decision.reason,
            "adaptive low-confidence focus snapshot (focused=false)"
        );
    }

    #[test]
    fn unknown_route_remains_terminal_first() {
        let focus = snapshot("mystery", "floating-tool", "/org/example/unknown", true);
        let decision = decide_test_route(&focus);
        assert_eq!(decision.surface_class, SurfaceClass::Unknown);
        assert_eq!(decision.shortcut_plan.primary, PasteShortcut::CtrlShiftV);
        assert_eq!(
            decision.shortcut_plan.adaptive_fallback,
            Some(PasteShortcut::CtrlV)
        );
    }

    #[test]
    fn route_decision_carries_focus_source_confidence_output_and_reason() {
        let mut focus = snapshot(
            "Ghostty",
            "terminal",
            "/com/mitchellh/ghostty/a11y/abc",
            true,
        );
        focus.output_name = Some("DP-1".to_string());

        let decision = decide_route_for_focus(FocusRouteInput::new(
            Some(&focus),
            "wayland_cache_low_confidence",
            FocusConfidence::LowConfidence,
        ));

        assert_eq!(decision.source, "wayland_cache_low_confidence");
        assert_eq!(decision.focus_confidence, FocusConfidence::LowConfidence);
        assert_eq!(decision.surface_class, SurfaceClass::Terminal);
        assert_eq!(decision.shortcut_plan.primary, PasteShortcut::CtrlShiftV);
        assert_eq!(
            decision.shortcut_plan.adaptive_fallback,
            Some(PasteShortcut::CtrlV)
        );
        assert_eq!(decision.output_name.as_deref(), Some("DP-1"));
        assert_eq!(decision.reason, "adaptive terminal-like surface");
    }

    #[test]
    fn route_decision_reports_unavailable_focus_as_unknown_route() {
        let decision = decide_route_for_focus(FocusRouteInput::new(
            None,
            "wayland_unavailable",
            FocusConfidence::Unavailable,
        ));

        assert_eq!(decision.source, "wayland_unavailable");
        assert_eq!(decision.focus_confidence, FocusConfidence::Unavailable);
        assert_eq!(decision.surface_class, SurfaceClass::Unknown);
        assert_eq!(decision.shortcut_plan.primary, PasteShortcut::CtrlShiftV);
        assert_eq!(
            decision.shortcut_plan.adaptive_fallback,
            Some(PasteShortcut::CtrlV)
        );
        assert_eq!(decision.output_name, None);
        assert_eq!(decision.reason, "adaptive unknown surface");
    }

    #[test]
    fn shared_focus_observation_feeds_injection_route_and_overlay_target() {
        let mut focus = snapshot(
            "Ghostty",
            "terminal",
            "/com/mitchellh/ghostty/a11y/abc",
            true,
        );
        focus.output_name = Some("DP-1".to_string());

        let observation = FocusObservation::from_wayland(WaylandFocusObservation::Fresh {
            snapshot: focus,
            cache_age_ms: 8,
        });
        let route = decide_route_for_focus(observation.route_input());

        assert_eq!(observation.source_selected, "wayland_cache");
        assert_eq!(observation.confidence, FocusConfidence::Fresh);
        assert_eq!(route.source, "wayland_cache");
        assert_eq!(route.focus_confidence, FocusConfidence::Fresh);
        assert_eq!(route.surface_class, SurfaceClass::Terminal);
        assert_eq!(route.output_name.as_deref(), Some("DP-1"));
        assert_eq!(
            decide_overlay_output_target(&observation).as_deref(),
            Some("DP-1")
        );
    }

    #[test]
    fn overlay_output_target_preserves_focus_confidence_rules() {
        let mut fresh = snapshot("Code", "editor", "/org/example/code", true);
        fresh.output_name = Some("DP-1".to_string());
        let fresh_observation = FocusObservation::from_wayland(WaylandFocusObservation::Fresh {
            snapshot: fresh,
            cache_age_ms: 4,
        });
        assert_eq!(
            decide_overlay_output_target(&fresh_observation).as_deref(),
            Some("DP-1")
        );

        let mut stale = snapshot("Ghostty", "terminal", "/org/example/terminal", true);
        stale.output_name = Some("HDMI-A-1".to_string());
        let stale_observation =
            FocusObservation::from_wayland(WaylandFocusObservation::LowConfidence {
                snapshot: stale,
                cache_age_ms: 1_600,
                reason: WAYLAND_FOCUS_REASON_CACHE_STALE,
            });
        assert_eq!(
            decide_overlay_output_target(&stale_observation).as_deref(),
            Some("HDMI-A-1")
        );

        let mut transition = snapshot("Code", "editor", "/org/example/code", false);
        transition.output_name = Some("DP-2".to_string());
        let transition_observation =
            FocusObservation::from_wayland(WaylandFocusObservation::LowConfidence {
                snapshot: transition,
                cache_age_ms: 12,
                reason: WAYLAND_FOCUS_REASON_TRANSITION_NO_ACTIVATED,
            });
        assert_eq!(
            decide_overlay_output_target(&transition_observation).as_deref(),
            Some("DP-2")
        );

        let mut low_confidence_other = snapshot("Code", "editor", "/org/example/code", true);
        low_confidence_other.output_name = Some("DP-3".to_string());
        let low_confidence_other_observation =
            FocusObservation::from_wayland(WaylandFocusObservation::LowConfidence {
                snapshot: low_confidence_other,
                cache_age_ms: 12,
                reason: "other_low_confidence_reason",
            });
        assert_eq!(
            decide_overlay_output_target(&low_confidence_other_observation),
            None
        );

        let unavailable = FocusObservation::from_wayland(WaylandFocusObservation::Unavailable {
            reason: "wayland_cache_uninitialized",
            cache_age_ms: None,
        });
        assert_eq!(decide_overlay_output_target(&unavailable), None);
    }
}
