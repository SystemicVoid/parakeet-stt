use std::collections::HashSet;
use std::process::Command;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use regex::Regex;

#[derive(Debug, Clone)]
struct ObjectRef {
    service_name: String,
    object_path: String,
}

#[derive(Debug, Clone)]
pub struct FocusSnapshot {
    pub app_name: Option<String>,
    pub object_name: Option<String>,
    pub object_path: Option<String>,
    pub service_name: Option<String>,
    pub focused: bool,
    pub active: bool,
    pub resolver: &'static str,
}

impl FocusSnapshot {
    pub fn haystack(&self) -> String {
        [
            self.app_name.as_deref().unwrap_or_default(),
            self.object_name.as_deref().unwrap_or_default(),
            self.object_path.as_deref().unwrap_or_default(),
            self.service_name.as_deref().unwrap_or_default(),
        ]
        .join(" ")
        .to_lowercase()
    }
}

#[derive(Debug, Clone, Default)]
pub struct FocusResolveStats {
    pub timed_out: bool,
    pub gdbus_calls: u32,
    pub deep_scan_apps: u8,
    pub deep_scan_nodes: u32,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Default)]
pub struct FocusResolveResult {
    pub snapshot: Option<FocusSnapshot>,
    pub stats: FocusResolveStats,
}

#[derive(Debug)]
enum ResolveFailure {
    Timeout,
    Fatal(anyhow::Error),
}

type ResolveAttempt<T> = std::result::Result<T, ResolveFailure>;

#[derive(Debug, Clone, Default)]
pub struct AtspiFocusResolver {
    cached_address: Arc<Mutex<Option<String>>>,
}

impl AtspiFocusResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn resolve_with_limits(
        &self,
        budget_ms: u64,
        deep_scan_max_apps: u8,
    ) -> Result<FocusResolveResult> {
        let started = Instant::now();
        let deadline = started
            .checked_add(std::time::Duration::from_millis(budget_ms.max(1)))
            .unwrap_or(started);
        let mut stats = FocusResolveStats::default();

        let mut best: Option<(u8, usize, FocusSnapshot)> = None;
        let address = match self.atspi_address(deadline, &mut stats) {
            Ok(address) => address,
            Err(ResolveFailure::Timeout) => {
                return Ok(finalize_resolve(started, stats, best));
            }
            Err(ResolveFailure::Fatal(err)) => {
                return Err(err);
            }
        };

        let app_roots = match self.get_children(
            &address,
            "org.a11y.atspi.Registry",
            ROOT_OBJECT_PATH,
            deadline,
            &mut stats,
        ) {
            Ok(app_roots) => app_roots,
            Err(ResolveFailure::Timeout) => {
                return Ok(finalize_resolve(started, stats, best));
            }
            Err(ResolveFailure::Fatal(err)) => {
                return Err(err);
            }
        };
        if app_roots.is_empty() {
            return Ok(finalize_resolve(started, stats, best));
        }

        let mut deep_scan_targets: Vec<(u8, ObjectRef, Option<String>)> = Vec::new();

        'apps: for app in app_roots {
            if Self::deadline_expired(deadline, &mut stats) {
                break;
            }

            let app_name = match self.get_name(
                &address,
                &app.service_name,
                &app.object_path,
                deadline,
                &mut stats,
            ) {
                Ok(value) => value,
                Err(ResolveFailure::Timeout) => break,
                Err(ResolveFailure::Fatal(_)) => None,
            };

            // Some apps expose windows as children; others only mark the root object.
            let mut candidates = match self.get_children(
                &address,
                &app.service_name,
                &app.object_path,
                deadline,
                &mut stats,
            ) {
                Ok(children) => children
                    .into_iter()
                    .map(|candidate| (candidate, 1usize))
                    .collect::<Vec<_>>(),
                Err(ResolveFailure::Timeout) => break,
                Err(ResolveFailure::Fatal(_)) => Vec::new(),
            };
            candidates.push((app.clone(), 0));

            let mut app_best: Option<(u8, usize, FocusSnapshot)> = None;
            let mut app_has_active = false;

            for (candidate, depth) in candidates {
                let state_chunks = match self.get_state_chunks(
                    &address,
                    &candidate.service_name,
                    &candidate.object_path,
                    deadline,
                    &mut stats,
                ) {
                    Ok(chunks) => chunks,
                    Err(ResolveFailure::Timeout) => break 'apps,
                    Err(ResolveFailure::Fatal(_)) => continue,
                };

                let focused = has_state_bit(&state_chunks, ATSPI_STATE_FOCUSED);
                let active = has_state_bit(&state_chunks, ATSPI_STATE_ACTIVE);
                if active {
                    app_has_active = true;
                }
                if !(focused || active) {
                    continue;
                }

                let object_name = match self.get_name(
                    &address,
                    &candidate.service_name,
                    &candidate.object_path,
                    deadline,
                    &mut stats,
                ) {
                    Ok(value) => value,
                    Err(ResolveFailure::Timeout) => break 'apps,
                    Err(ResolveFailure::Fatal(_)) => None,
                };

                let snapshot = FocusSnapshot {
                    app_name: app_name.clone(),
                    object_name,
                    object_path: Some(candidate.object_path.clone()),
                    service_name: Some(candidate.service_name.clone()),
                    focused,
                    active,
                    resolver: "atspi",
                };

                let score = rank_candidate(active, focused);
                let should_replace = match &app_best {
                    Some((best_score, best_depth, best_snapshot)) => should_replace_candidate(
                        *best_score,
                        *best_depth,
                        best_snapshot.focused,
                        score,
                        depth,
                        snapshot.focused,
                    ),
                    None => true,
                };
                if should_replace {
                    app_best = Some((score, depth, snapshot));
                }
            }

            let app_best_rank = app_best.as_ref().map_or(0, |(score, _, _)| *score);
            let app_best_focused = app_best
                .as_ref()
                .is_some_and(|(_, _, snapshot)| snapshot.focused);

            if let Some((score, depth, snapshot)) = app_best {
                if should_replace_global(best.as_ref(), score, depth, &snapshot) {
                    best = Some((score, depth, snapshot));
                }
            }

            if is_high_confidence(best.as_ref()) {
                break;
            }

            if app_has_active && !app_best_focused {
                deep_scan_targets.push((app_best_rank, app, app_name));
            }
        }

        if !is_high_confidence(best.as_ref()) && deep_scan_max_apps > 0 {
            deep_scan_targets.sort_by(|left, right| right.0.cmp(&left.0));
            for (_, app, app_name) in deep_scan_targets
                .into_iter()
                .take(usize::from(deep_scan_max_apps))
            {
                if Self::deadline_expired(deadline, &mut stats) {
                    break;
                }
                stats.deep_scan_apps = stats.deep_scan_apps.saturating_add(1);

                let focused =
                    match self.find_focused_descendant(&address, &app, deadline, &mut stats) {
                        Ok(value) => value,
                        Err(ResolveFailure::Timeout) => break,
                        Err(ResolveFailure::Fatal(_)) => continue,
                    };

                let Some((focused_ref, depth)) = focused else {
                    continue;
                };

                let object_name = match self.get_name(
                    &address,
                    &focused_ref.service_name,
                    &focused_ref.object_path,
                    deadline,
                    &mut stats,
                ) {
                    Ok(value) => value,
                    Err(ResolveFailure::Timeout) => break,
                    Err(ResolveFailure::Fatal(_)) => None,
                };

                let focused_snapshot = FocusSnapshot {
                    app_name: app_name.clone(),
                    object_name,
                    object_path: Some(focused_ref.object_path.clone()),
                    service_name: Some(focused_ref.service_name.clone()),
                    focused: true,
                    active: true,
                    resolver: "atspi",
                };
                let score = rank_candidate(true, true);
                if should_replace_global(best.as_ref(), score, depth, &focused_snapshot) {
                    best = Some((score, depth, focused_snapshot));
                }

                if is_high_confidence(best.as_ref()) {
                    break;
                }
            }
        }

        Ok(finalize_resolve(started, stats, best))
    }

    fn find_focused_descendant(
        &self,
        address: &str,
        root: &ObjectRef,
        deadline: Instant,
        stats: &mut FocusResolveStats,
    ) -> ResolveAttempt<Option<(ObjectRef, usize)>> {
        let mut stack: Vec<(ObjectRef, usize)> = vec![(root.clone(), 0)];
        let mut seen = HashSet::new();
        let mut scanned = 0usize;

        while let Some((candidate, depth)) = stack.pop() {
            if Self::deadline_expired(deadline, stats) {
                return Err(ResolveFailure::Timeout);
            }
            if scanned >= MAX_FOCUS_SCAN_NODES {
                break;
            }

            let key = (
                candidate.service_name.clone(),
                candidate.object_path.clone(),
            );
            if !seen.insert(key) {
                continue;
            }
            scanned += 1;
            stats.deep_scan_nodes = stats.deep_scan_nodes.saturating_add(1);

            let state_chunks = match self.get_state_chunks(
                address,
                &candidate.service_name,
                &candidate.object_path,
                deadline,
                stats,
            ) {
                Ok(chunks) => chunks,
                Err(ResolveFailure::Timeout) => return Err(ResolveFailure::Timeout),
                Err(ResolveFailure::Fatal(_)) => continue,
            };
            if has_state_bit(&state_chunks, ATSPI_STATE_FOCUSED) {
                return Ok(Some((candidate, depth)));
            }

            let children = match self.get_children(
                address,
                &candidate.service_name,
                &candidate.object_path,
                deadline,
                stats,
            ) {
                Ok(children) => children,
                Err(ResolveFailure::Timeout) => return Err(ResolveFailure::Timeout),
                Err(ResolveFailure::Fatal(_)) => continue,
            };
            for child in children {
                stack.push((child, depth + 1));
            }
        }

        Ok(None)
    }

    fn atspi_address(
        &self,
        deadline: Instant,
        stats: &mut FocusResolveStats,
    ) -> ResolveAttempt<String> {
        if let Some(address) = self
            .cached_address
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
        {
            return Ok(address);
        }

        let output = Self::gdbus_call(
            [
                "call",
                "--session",
                "--dest",
                "org.a11y.Bus",
                "--object-path",
                "/org/a11y/bus",
                "--timeout",
                GDBUS_TIMEOUT_SECONDS,
                "--method",
                "org.a11y.Bus.GetAddress",
            ],
            deadline,
            stats,
        )?;
        let address = parse_first_single_quoted(&output)
            .context("failed to parse org.a11y.Bus.GetAddress output")
            .map_err(ResolveFailure::Fatal)?;

        if let Ok(mut guard) = self.cached_address.lock() {
            *guard = Some(address.clone());
        }
        Ok(address)
    }

    fn get_children(
        &self,
        address: &str,
        dest: &str,
        path: &str,
        deadline: Instant,
        stats: &mut FocusResolveStats,
    ) -> ResolveAttempt<Vec<ObjectRef>> {
        let output = Self::gdbus_call(
            [
                "call",
                "--address",
                address,
                "--dest",
                dest,
                "--object-path",
                path,
                "--timeout",
                GDBUS_TIMEOUT_SECONDS,
                "--method",
                "org.a11y.atspi.Accessible.GetChildren",
            ],
            deadline,
            stats,
        )?;

        Ok(parse_object_refs(&output))
    }

    fn get_name(
        &self,
        address: &str,
        dest: &str,
        path: &str,
        deadline: Instant,
        stats: &mut FocusResolveStats,
    ) -> ResolveAttempt<Option<String>> {
        let output = Self::gdbus_call(
            [
                "call",
                "--address",
                address,
                "--dest",
                dest,
                "--object-path",
                path,
                "--timeout",
                GDBUS_TIMEOUT_SECONDS,
                "--method",
                "org.freedesktop.DBus.Properties.Get",
                "org.a11y.atspi.Accessible",
                "Name",
            ],
            deadline,
            stats,
        )?;

        Ok(parse_variant_string(&output))
    }

    fn get_state_chunks(
        &self,
        address: &str,
        dest: &str,
        path: &str,
        deadline: Instant,
        stats: &mut FocusResolveStats,
    ) -> ResolveAttempt<Vec<u32>> {
        let output = Self::gdbus_call(
            [
                "call",
                "--address",
                address,
                "--dest",
                dest,
                "--object-path",
                path,
                "--timeout",
                GDBUS_TIMEOUT_SECONDS,
                "--method",
                "org.a11y.atspi.Accessible.GetState",
            ],
            deadline,
            stats,
        )?;

        Ok(parse_uint32_list(&output))
    }

    fn gdbus_call<const N: usize>(
        args: [&str; N],
        deadline: Instant,
        stats: &mut FocusResolveStats,
    ) -> ResolveAttempt<String> {
        if Self::deadline_expired(deadline, stats) {
            return Err(ResolveFailure::Timeout);
        }

        stats.gdbus_calls = stats.gdbus_calls.saturating_add(1);
        let output = Command::new("gdbus")
            .args(args)
            .output()
            .context("failed to spawn gdbus command")
            .map_err(ResolveFailure::Fatal)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ResolveFailure::Fatal(anyhow::anyhow!(
                "gdbus call failed: {}",
                stderr.trim()
            )));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn deadline_expired(deadline: Instant, stats: &mut FocusResolveStats) -> bool {
        if Instant::now() < deadline {
            return false;
        }
        stats.timed_out = true;
        true
    }
}

const ROOT_OBJECT_PATH: &str = "/org/a11y/atspi/accessible/root";
const ATSPI_STATE_ACTIVE: u32 = 1;
const ATSPI_STATE_FOCUSED: u32 = 12;
const MAX_FOCUS_SCAN_NODES: usize = 256;
const GDBUS_TIMEOUT_SECONDS: &str = "1";

fn finalize_resolve(
    started: Instant,
    mut stats: FocusResolveStats,
    best: Option<(u8, usize, FocusSnapshot)>,
) -> FocusResolveResult {
    stats.duration_ms = started.elapsed().as_millis() as u64;
    FocusResolveResult {
        snapshot: best.map(|(_, _, snapshot)| snapshot),
        stats,
    }
}

fn rank_candidate(active: bool, focused: bool) -> u8 {
    (u8::from(active) * 2) + u8::from(focused)
}

fn should_replace_global(
    current: Option<&(u8, usize, FocusSnapshot)>,
    new_rank: u8,
    new_depth: usize,
    new_snapshot: &FocusSnapshot,
) -> bool {
    match current {
        Some((current_rank, current_depth, current_snapshot)) => should_replace_candidate(
            *current_rank,
            *current_depth,
            current_snapshot.focused,
            new_rank,
            new_depth,
            new_snapshot.focused,
        ),
        None => true,
    }
}

fn is_high_confidence(candidate: Option<&(u8, usize, FocusSnapshot)>) -> bool {
    candidate.is_some_and(|(_, _, snapshot)| snapshot.active && snapshot.focused)
}

fn should_replace_candidate(
    current_rank: u8,
    current_depth: usize,
    current_focused: bool,
    new_rank: u8,
    new_depth: usize,
    new_focused: bool,
) -> bool {
    if new_rank > current_rank {
        return true;
    }
    if new_rank < current_rank {
        return false;
    }

    // For equally-ranked focused candidates, prefer deeper descendants.
    new_focused && current_focused && new_depth > current_depth
}

fn has_state_bit(chunks: &[u32], bit: u32) -> bool {
    let chunk_index = (bit / 32) as usize;
    let bit_in_chunk = bit % 32;
    chunks
        .get(chunk_index)
        .map(|chunk| chunk & (1 << bit_in_chunk) != 0)
        .unwrap_or(false)
}

fn parse_first_single_quoted(value: &str) -> Option<String> {
    SINGLE_QUOTED_RE
        .captures(value)
        .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()))
}

fn parse_variant_string(value: &str) -> Option<String> {
    VARIANT_STRING_RE
        .captures(value)
        .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()))
}

fn parse_object_refs(value: &str) -> Vec<ObjectRef> {
    OBJECT_REF_RE
        .captures_iter(value)
        .filter_map(|caps| {
            let service_name = caps.get(1)?.as_str().to_string();
            let object_path = caps.get(2)?.as_str().to_string();
            Some(ObjectRef {
                service_name,
                object_path,
            })
        })
        .collect()
}

fn parse_uint32_list(value: &str) -> Vec<u32> {
    let mut values: Vec<u32> = UINT32_RE
        .captures_iter(value)
        .filter_map(|caps| caps.get(1).and_then(|m| m.as_str().parse::<u32>().ok()))
        .collect();
    if values.len() > 1 {
        return values;
    }

    if let Some(start) = value.find('[') {
        if let Some(end) = value[start..].find(']') {
            let payload = &value[start + 1..start + end];
            values = payload
                .split(',')
                .filter_map(|item| {
                    let cleaned = item.trim().trim_start_matches("uint32").trim();
                    cleaned.parse::<u32>().ok()
                })
                .collect();
        }
    }
    values
}

static SINGLE_QUOTED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"'([^']+)'").expect("valid regex"));
static VARIANT_STRING_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<\s*'([^']*)'\s*>").expect("valid regex"));
static OBJECT_REF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\('([^']+)',\s*(?:objectpath\s*)?'([^']+)'\)").expect("valid regex")
});
static UINT32_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"uint32\s+(\d+)").expect("valid regex"));

#[cfg(test)]
mod tests {
    use super::{
        has_state_bit, is_high_confidence, parse_object_refs, parse_uint32_list, rank_candidate,
        should_replace_candidate, FocusSnapshot,
    };

    #[test]
    fn parses_object_refs_from_gdbus_output() {
        let input = "([(':1.3', objectpath '/org/a11y/atspi/accessible/root'), (':1.2', '/x')],)";
        let refs = parse_object_refs(input);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].service_name, ":1.3");
        assert_eq!(refs[0].object_path, "/org/a11y/atspi/accessible/root");
        assert_eq!(refs[1].service_name, ":1.2");
        assert_eq!(refs[1].object_path, "/x");
    }

    #[test]
    fn parses_state_chunks_and_checks_bits() {
        let chunks = parse_uint32_list("([uint32 1124075776, 0],)");
        assert_eq!(chunks, vec![1124075776, 0]);
        assert!(has_state_bit(&chunks, 8));
        assert!(!has_state_bit(&chunks, 12));
    }

    #[test]
    fn ranks_active_above_focused_only() {
        assert!(rank_candidate(true, false) > rank_candidate(false, true));
        assert!(rank_candidate(true, true) > rank_candidate(true, false));
    }

    #[test]
    fn tie_break_prefers_deeper_focused_candidate() {
        assert!(should_replace_candidate(3, 3, true, 3, 8, true));
        assert!(!should_replace_candidate(3, 8, true, 3, 3, true));
    }

    #[test]
    fn high_confidence_requires_active_and_focused() {
        let snapshot = FocusSnapshot {
            app_name: Some("App".to_string()),
            object_name: Some("Window".to_string()),
            object_path: Some("/obj".to_string()),
            service_name: Some(":1.7".to_string()),
            focused: true,
            active: true,
            resolver: "test",
        };
        assert!(is_high_confidence(Some(&(3, 1, snapshot.clone()))));

        let mut not_focused = snapshot.clone();
        not_focused.focused = false;
        assert!(!is_high_confidence(Some(&(2, 1, not_focused))));
    }
}
