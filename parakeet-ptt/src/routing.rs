use crate::config::{ClipboardOptions, PasteRoutingMode, PasteShortcut};
use crate::surface_focus::FocusSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceClass {
    Terminal,
    General,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct RouteDecision {
    pub class: SurfaceClass,
    pub primary: PasteShortcut,
    pub adaptive_fallback: Option<PasteShortcut>,
    pub low_confidence: bool,
    pub reason: &'static str,
}

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

pub fn decide_route(options: &ClipboardOptions, focus: Option<&FocusSnapshot>) -> RouteDecision {
    if matches!(options.routing_mode, PasteRoutingMode::Static) {
        return RouteDecision {
            class: SurfaceClass::Unknown,
            primary: options.paste_shortcut,
            adaptive_fallback: None,
            low_confidence: false,
            reason: "routing_mode=static",
        };
    }

    if let Some(snapshot) = focus {
        if !snapshot.focused {
            return unknown_route(
                options,
                "adaptive low-confidence focus snapshot (focused=false)",
                true,
            );
        }
    }

    let (class, class_reason) = classify_surface_with_reason(focus);
    match class {
        SurfaceClass::Terminal => RouteDecision {
            class,
            primary: options.adaptive_terminal_shortcut,
            adaptive_fallback: dedup_fallback(
                options.adaptive_terminal_shortcut,
                options.adaptive_general_shortcut,
            ),
            low_confidence: false,
            reason: class_reason,
        },
        SurfaceClass::General => RouteDecision {
            class,
            primary: options.adaptive_general_shortcut,
            adaptive_fallback: dedup_fallback(
                options.adaptive_general_shortcut,
                options.adaptive_terminal_shortcut,
            ),
            low_confidence: false,
            reason: class_reason,
        },
        SurfaceClass::Unknown => unknown_route(options, class_reason, false),
    }
}

fn unknown_route(
    options: &ClipboardOptions,
    reason: &'static str,
    low_confidence: bool,
) -> RouteDecision {
    let alternate = if options.adaptive_unknown_shortcut != options.adaptive_general_shortcut {
        options.adaptive_general_shortcut
    } else {
        options.adaptive_terminal_shortcut
    };

    RouteDecision {
        class: SurfaceClass::Unknown,
        primary: options.adaptive_unknown_shortcut,
        adaptive_fallback: dedup_fallback(options.adaptive_unknown_shortcut, alternate),
        low_confidence,
        reason,
    }
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
    use super::{classify_surface_with_reason, decide_route, SurfaceClass};
    use crate::config::{
        ClipboardOptions, PasteBackendFailurePolicy, PasteKeyBackend, PasteRestorePolicy,
        PasteRoutingMode, PasteShortcut, PasteStrategy,
    };
    use crate::surface_focus::FocusSnapshot;

    fn options() -> ClipboardOptions {
        ClipboardOptions {
            paste_shortcut: PasteShortcut::CtrlShiftV,
            shortcut_fallback: None,
            paste_strategy: PasteStrategy::Single,
            chain_delay_ms: 45,
            restore_policy: PasteRestorePolicy::Never,
            restore_delay_ms: 250,
            post_chord_hold_ms: 700,
            copy_foreground: true,
            mime_type: "text/plain;charset=utf-8".to_string(),
            key_backend: PasteKeyBackend::Auto,
            backend_failure_policy: PasteBackendFailurePolicy::CopyOnly,
            routing_mode: PasteRoutingMode::Adaptive,
            adaptive_terminal_shortcut: PasteShortcut::CtrlShiftV,
            adaptive_general_shortcut: PasteShortcut::CtrlV,
            adaptive_unknown_shortcut: PasteShortcut::CtrlShiftV,
            seat: None,
            write_primary: false,
        }
    }

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
            focused,
            active: true,
            resolver: "test",
        }
    }

    #[test]
    fn classifies_terminal_surface() {
        let focus = snapshot("Unnamed", "shell", "/com/mitchellh/ghostty/a11y/abc", false);
        assert_eq!(classify_surface_with_reason(Some(&focus)).0, SurfaceClass::Terminal);
    }

    #[test]
    fn classifies_general_surface() {
        let focus = snapshot(
            "Brave Browser",
            "Codex - Brave",
            "/org/a11y/atspi/accessible/1",
            false,
        );
        assert_eq!(classify_surface_with_reason(Some(&focus)).0, SurfaceClass::General);
    }

    #[test]
    fn classifies_cosmic_edit_app_id_as_general() {
        let focus = snapshot(
            "com.system76.CosmicEdit",
            "Untitled",
            "/org/a11y/atspi/accessible/2",
            true,
        );
        assert_eq!(classify_surface_with_reason(Some(&focus)).0, SurfaceClass::General);
    }

    #[test]
    fn classifies_cosmic_text_editor_title_as_general() {
        let focus = snapshot(
            "Some app",
            "New document - COSMIC Text Editor",
            "/org/a11y/atspi/accessible/3",
            true,
        );
        assert_eq!(classify_surface_with_reason(Some(&focus)).0, SurfaceClass::General);
    }

    #[test]
    fn normalization_handles_dot_dash_underscore_forms() {
        let focus = snapshot(
            "com.system76.cosmic_edit",
            "cosmic-edit",
            "/org/a11y/atspi/accessible/4",
            true,
        );
        assert_eq!(classify_surface_with_reason(Some(&focus)).0, SurfaceClass::General);
    }

    #[test]
    fn unknown_surface_defaults_to_unknown() {
        let focus = snapshot("SomeApp", "random", "/org/example", false);
        assert_eq!(classify_surface_with_reason(Some(&focus)).0, SurfaceClass::Unknown);
    }

    #[test]
    fn adaptive_route_prefers_terminal_shortcut_for_terminals() {
        let opts = options();
        let focus = snapshot("Unnamed", "shell", "/com/mitchellh/ghostty/a11y/abc", true);
        let decision = decide_route(&opts, Some(&focus));
        assert_eq!(decision.class, SurfaceClass::Terminal);
        assert_eq!(decision.primary, PasteShortcut::CtrlShiftV);
        assert_eq!(decision.adaptive_fallback, Some(PasteShortcut::CtrlV));
        assert!(!decision.low_confidence);
    }

    #[test]
    fn adaptive_route_uses_unknown_when_snapshot_is_not_focused() {
        let opts = options();
        let focus = snapshot(
            "Brave Browser",
            "Codex - Brave",
            "/org/a11y/atspi/accessible/1",
            false,
        );
        let decision = decide_route(&opts, Some(&focus));
        assert_eq!(decision.class, SurfaceClass::Unknown);
        assert_eq!(decision.primary, PasteShortcut::CtrlShiftV);
        assert_eq!(decision.adaptive_fallback, Some(PasteShortcut::CtrlV));
        assert!(decision.low_confidence);
    }

    #[test]
    fn unknown_route_remains_terminal_first() {
        let opts = options();
        let focus = snapshot("mystery", "floating-tool", "/org/example/unknown", true);
        let decision = decide_route(&opts, Some(&focus));
        assert_eq!(decision.class, SurfaceClass::Unknown);
        assert_eq!(decision.primary, PasteShortcut::CtrlShiftV);
        assert_eq!(decision.adaptive_fallback, Some(PasteShortcut::CtrlV));
    }

    #[test]
    fn static_route_uses_legacy_shortcut() {
        let mut opts = options();
        opts.routing_mode = PasteRoutingMode::Static;
        opts.paste_shortcut = PasteShortcut::ShiftInsert;
        let decision = decide_route(&opts, None);
        assert_eq!(decision.primary, PasteShortcut::ShiftInsert);
        assert!(decision.adaptive_fallback.is_none());
    }
}
