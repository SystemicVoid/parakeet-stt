use std::process::Command;
use std::sync::{Arc, LazyLock, Mutex};

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
pub struct AtspiFocusResolver {
    cached_address: Arc<Mutex<Option<String>>>,
}

impl AtspiFocusResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn resolve(&self) -> Result<Option<FocusSnapshot>> {
        let address = self.atspi_address()?;
        let app_roots = self.get_children(&address, "org.a11y.atspi.Registry", ROOT_OBJECT_PATH)?;
        if app_roots.is_empty() {
            return Ok(None);
        }

        let mut best: Option<(u8, FocusSnapshot)> = None;
        for app in app_roots {
            let app_name = self
                .get_name(&address, &app.service_name, &app.object_path)
                .ok()
                .flatten();

            // Some apps expose windows as children; others only mark the root object.
            let mut candidates = self
                .get_children(&address, &app.service_name, &app.object_path)
                .unwrap_or_default();
            candidates.push(app.clone());

            for candidate in candidates {
                let state_chunks = match self.get_state_chunks(
                    &address,
                    &candidate.service_name,
                    &candidate.object_path,
                ) {
                    Ok(chunks) => chunks,
                    Err(_) => continue,
                };

                let focused = has_state_bit(&state_chunks, ATSPI_STATE_FOCUSED);
                let active = has_state_bit(&state_chunks, ATSPI_STATE_ACTIVE);
                if !(focused || active) {
                    continue;
                }

                let object_name = self
                    .get_name(&address, &candidate.service_name, &candidate.object_path)
                    .ok()
                    .flatten();

                let snapshot = FocusSnapshot {
                    app_name: app_name.clone(),
                    object_name,
                    object_path: Some(candidate.object_path.clone()),
                    service_name: Some(candidate.service_name.clone()),
                    focused,
                    active,
                    resolver: "atspi",
                };

                // Prefer focused objects, then active ones.
                let score = (u8::from(focused) * 2) + u8::from(active);
                match &best {
                    Some((best_score, _)) if *best_score >= score => {}
                    _ => best = Some((score, snapshot)),
                }
            }
        }

        Ok(best.map(|(_, snapshot)| snapshot))
    }

    fn atspi_address(&self) -> Result<String> {
        if let Some(address) = self
            .cached_address
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
        {
            return Ok(address);
        }

        let output = Self::gdbus_call([
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
        ])?;
        let address = parse_first_single_quoted(&output)
            .context("failed to parse org.a11y.Bus.GetAddress output")?;

        if let Ok(mut guard) = self.cached_address.lock() {
            *guard = Some(address.clone());
        }
        Ok(address)
    }

    fn get_children(&self, address: &str, dest: &str, path: &str) -> Result<Vec<ObjectRef>> {
        let output = Self::gdbus_call([
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
        ])?;

        Ok(parse_object_refs(&output))
    }

    fn get_name(&self, address: &str, dest: &str, path: &str) -> Result<Option<String>> {
        let output = Self::gdbus_call([
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
        ])?;

        Ok(parse_variant_string(&output))
    }

    fn get_state_chunks(&self, address: &str, dest: &str, path: &str) -> Result<Vec<u32>> {
        let output = Self::gdbus_call([
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
        ])?;

        Ok(parse_uint32_list(&output))
    }

    fn gdbus_call<const N: usize>(args: [&str; N]) -> Result<String> {
        let output = Command::new("gdbus")
            .args(args)
            .output()
            .context("failed to spawn gdbus command")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gdbus call failed: {}", stderr.trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

const ROOT_OBJECT_PATH: &str = "/org/a11y/atspi/accessible/root";
const ATSPI_STATE_ACTIVE: u32 = 1;
const ATSPI_STATE_FOCUSED: u32 = 12;
const GDBUS_TIMEOUT_SECONDS: &str = "2";

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
    use super::{has_state_bit, parse_object_refs, parse_uint32_list};

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
}
