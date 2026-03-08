use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use cosmic_protocols::toplevel_info::v1::client::{
    zcosmic_toplevel_handle_v1, zcosmic_toplevel_info_v1,
};
use serde::{Deserialize, Serialize};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{event_created_child, Connection, Dispatch, QueueHandle};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1, ext_foreign_toplevel_list_v1,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusSnapshot {
    pub app_name: Option<String>,
    pub object_name: Option<String>,
    pub object_path: Option<String>,
    pub service_name: Option<String>,
    pub output_name: Option<String>,
    pub focused: bool,
    pub active: bool,
    #[serde(
        skip_serializing,
        skip_deserializing,
        default = "default_focus_snapshot_resolver"
    )]
    pub resolver: &'static str,
}

fn default_focus_snapshot_resolver() -> &'static str {
    "serde"
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

#[derive(Debug, Clone)]
pub enum WaylandFocusObservation {
    Fresh {
        snapshot: FocusSnapshot,
        cache_age_ms: u64,
    },
    LowConfidence {
        snapshot: FocusSnapshot,
        cache_age_ms: u64,
        reason: &'static str,
    },
    Unavailable {
        reason: &'static str,
        cache_age_ms: Option<u64>,
    },
}

#[derive(Debug, Clone)]
pub struct WaylandFocusCache {
    shared: Arc<Mutex<WaylandFocusSharedState>>,
}

impl WaylandFocusCache {
    pub fn new() -> Self {
        let shared = Arc::new(Mutex::new(WaylandFocusSharedState::default()));
        let shared_for_worker = Arc::clone(&shared);
        if thread::Builder::new()
            .name("parakeet-wayland-focus".to_string())
            .spawn(move || run_wayland_focus_worker(shared_for_worker))
            .is_err()
        {
            if let Ok(mut state) = shared.lock() {
                state.connected = false;
                state.protocols_supported = false;
            }
        }
        Self { shared }
    }

    pub fn observe(&self, stale_ms: u64, transition_grace_ms: u64) -> WaylandFocusObservation {
        let snapshot = self
            .shared
            .lock()
            .ok()
            .map(|guard| guard.clone())
            .unwrap_or_default();

        if !snapshot.connected {
            return WaylandFocusObservation::Unavailable {
                reason: "wayland_cache_disconnected",
                cache_age_ms: snapshot
                    .last_commit_at
                    .map(|ts| ts.elapsed().as_millis() as u64),
            };
        }
        if !snapshot.protocols_supported {
            return WaylandFocusObservation::Unavailable {
                reason: "wayland_protocols_unavailable",
                cache_age_ms: None,
            };
        }

        let Some(last_commit_at) = snapshot.last_commit_at else {
            return WaylandFocusObservation::Unavailable {
                reason: "wayland_cache_uninitialized",
                cache_age_ms: None,
            };
        };

        let cache_age_ms = last_commit_at.elapsed().as_millis() as u64;
        if let Some(active) = snapshot.active.as_ref() {
            if cache_age_ms > stale_ms.max(1) {
                return WaylandFocusObservation::LowConfidence {
                    snapshot: active.to_focus_snapshot(true),
                    cache_age_ms,
                    reason: "wayland_cache_stale",
                };
            }
            return WaylandFocusObservation::Fresh {
                snapshot: active.to_focus_snapshot(true),
                cache_age_ms,
            };
        }

        if cache_age_ms > stale_ms.max(1) {
            return WaylandFocusObservation::Unavailable {
                reason: "wayland_cache_stale",
                cache_age_ms: Some(cache_age_ms),
            };
        }

        if snapshot.activated_count > 1 {
            return WaylandFocusObservation::Unavailable {
                reason: "wayland_ambiguous_activated",
                cache_age_ms: Some(cache_age_ms),
            };
        }

        let grace = transition_grace_ms.max(1);
        if let (Some(last_activated), Some(last_activated_at)) = (
            snapshot.last_activated.as_ref(),
            snapshot.last_activated_at.as_ref(),
        ) {
            if last_activated_at.elapsed().as_millis() as u64 <= grace {
                return WaylandFocusObservation::LowConfidence {
                    snapshot: last_activated.to_focus_snapshot(false),
                    cache_age_ms,
                    reason: "wayland_transition_no_activated",
                };
            }
        }

        WaylandFocusObservation::Unavailable {
            reason: "wayland_no_activated",
            cache_age_ms: Some(cache_age_ms),
        }
    }

    pub fn current_output_name(&self) -> Option<String> {
        match self.observe(1_500, 250) {
            WaylandFocusObservation::Fresh { snapshot, .. } => snapshot.output_name,
            WaylandFocusObservation::LowConfidence {
                snapshot,
                reason: "wayland_transition_no_activated" | "wayland_cache_stale",
                ..
            } => snapshot.output_name,
            WaylandFocusObservation::Unavailable { .. } => None,
            WaylandFocusObservation::LowConfidence { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct WaylandFocusSharedState {
    connected: bool,
    protocols_supported: bool,
    active: Option<CachedToplevel>,
    activated_count: usize,
    last_activated: Option<CachedToplevel>,
    last_commit_at: Option<Instant>,
    last_activated_at: Option<Instant>,
}

#[derive(Debug)]
struct WaylandRuntimeState {
    shared: Arc<Mutex<WaylandFocusSharedState>>,
    foreign_toplevel_list: Option<ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1>,
    cosmic_toplevel_info: Option<zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1>,
    outputs: Vec<RuntimeOutput>,
    toplevels: Vec<RuntimeToplevel>,
}

impl WaylandRuntimeState {
    fn new(shared: Arc<Mutex<WaylandFocusSharedState>>) -> Self {
        Self {
            shared,
            foreign_toplevel_list: None,
            cosmic_toplevel_info: None,
            outputs: Vec::new(),
            toplevels: Vec::new(),
        }
    }

    fn supports_focus_protocols(&self) -> bool {
        self.foreign_toplevel_list.is_some() && self.cosmic_toplevel_info.is_some()
    }

    fn publish(&self) {
        let now = Instant::now();
        let activated: Vec<CachedToplevel> = self
            .toplevels
            .iter()
            .filter(|entry| entry.activated)
            .map(|entry| CachedToplevel {
                identifier: entry.identifier.clone(),
                app_id: entry.app_id.clone(),
                title: entry.title.clone(),
                output_names: entry
                    .outputs
                    .iter()
                    .filter_map(|output| self.output_name_for(output))
                    .collect(),
            })
            .collect();

        if let Ok(mut shared) = self.shared.lock() {
            shared.connected = true;
            shared.protocols_supported = self.supports_focus_protocols();
            shared.activated_count = activated.len();
            shared.active = if activated.len() == 1 {
                activated.first().cloned()
            } else {
                None
            };
            if let Some(active) = shared.active.clone() {
                shared.last_activated = Some(active);
                shared.last_activated_at = Some(now);
            }
            shared.last_commit_at = Some(now);
        }
    }

    fn find_by_foreign_handle_mut(
        &mut self,
        handle: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ) -> Option<&mut RuntimeToplevel> {
        self.toplevels
            .iter_mut()
            .find(|entry| &entry.foreign == handle)
    }

    fn find_by_cosmic_handle_mut(
        &mut self,
        handle: &zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1,
    ) -> Option<&mut RuntimeToplevel> {
        self.toplevels
            .iter_mut()
            .find(|entry| entry.cosmic.as_ref() == Some(handle))
    }

    fn find_output_mut(&mut self, output: &wl_output::WlOutput) -> Option<&mut RuntimeOutput> {
        self.outputs
            .iter_mut()
            .find(|entry| &entry.output == output)
    }

    fn output_name_for(&self, output: &wl_output::WlOutput) -> Option<String> {
        self.outputs
            .iter()
            .find(|entry| &entry.output == output)
            .and_then(|entry| entry.name.clone())
    }
}

#[derive(Debug)]
struct RuntimeOutput {
    output: wl_output::WlOutput,
    name: Option<String>,
}

#[derive(Debug)]
struct RuntimeToplevel {
    foreign: ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    cosmic: Option<zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1>,
    identifier: Option<String>,
    app_id: Option<String>,
    title: Option<String>,
    outputs: Vec<wl_output::WlOutput>,
    activated: bool,
}

#[derive(Debug, Clone)]
struct CachedToplevel {
    identifier: Option<String>,
    app_id: Option<String>,
    title: Option<String>,
    output_names: Vec<String>,
}

impl CachedToplevel {
    fn to_focus_snapshot(&self, focused: bool) -> FocusSnapshot {
        FocusSnapshot {
            app_name: self.app_id.clone(),
            object_name: self.title.clone(),
            object_path: self.identifier.clone(),
            service_name: Some("wayland".to_string()),
            output_name: self.output_names.first().cloned(),
            focused,
            active: true,
            resolver: "wayland",
        }
    }
}

fn run_wayland_focus_worker(shared: Arc<Mutex<WaylandFocusSharedState>>) {
    const RECONNECT_DELAY: Duration = Duration::from_secs(1);

    loop {
        let result = run_wayland_focus_session(Arc::clone(&shared));
        if result.is_err() {
            if let Ok(mut guard) = shared.lock() {
                guard.connected = false;
            }
        }
        thread::sleep(RECONNECT_DELAY);
    }
}

fn run_wayland_focus_session(shared: Arc<Mutex<WaylandFocusSharedState>>) -> Result<()> {
    let connection =
        Connection::connect_to_env().context("failed to connect to Wayland display")?;
    let display = connection.display();
    let mut event_queue = connection.new_event_queue();
    let queue_handle = event_queue.handle();
    let _registry = display.get_registry(&queue_handle, ());
    let mut runtime = WaylandRuntimeState::new(shared);

    event_queue
        .roundtrip(&mut runtime)
        .context("failed initial Wayland roundtrip")?;
    event_queue
        .roundtrip(&mut runtime)
        .context("failed secondary Wayland roundtrip")?;
    runtime.publish();

    loop {
        event_queue
            .blocking_dispatch(&mut runtime)
            .context("Wayland focus event dispatch failed")?;
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for WaylandRuntimeState {
    fn event(
        runtime: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        queue_handle: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                match interface.as_str() {
                    "ext_foreign_toplevel_list_v1" => {
                        let bind_version = version.min(1);
                        runtime.foreign_toplevel_list = Some(registry.bind::<
                        ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1,
                        _,
                        _,
                    >(name, bind_version, queue_handle, ()));
                    }
                    "zcosmic_toplevel_info_v1"
                        if version >= zcosmic_toplevel_info_v1::REQ_GET_COSMIC_TOPLEVEL_SINCE =>
                    {
                        let bind_version = version.min(3);
                        runtime.cosmic_toplevel_info = Some(
                            registry.bind::<zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1, _, _>(
                                name,
                                bind_version,
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
                        runtime.outputs.push(RuntimeOutput { output, name: None });
                    }
                    _ => {}
                }
            }
            wl_registry::Event::GlobalRemove { .. } => {}
            _ => {}
        }
        runtime.publish();
    }
}

impl Dispatch<ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, ()> for WaylandRuntimeState {
    fn event(
        runtime: &mut Self,
        _: &ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        queue_handle: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            let cosmic = runtime
                .cosmic_toplevel_info
                .as_ref()
                .map(|manager| manager.get_cosmic_toplevel(&toplevel, queue_handle, ()));
            runtime.toplevels.push(RuntimeToplevel {
                foreign: toplevel,
                cosmic,
                identifier: None,
                app_id: None,
                title: None,
                outputs: Vec::new(),
                activated: false,
            });
        }
        runtime.publish();
    }

    event_created_child!(
        WaylandRuntimeState,
        ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1,
        [
            ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (
                ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
                ()
            ),
        ]
    );
}

impl Dispatch<ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1, ()>
    for WaylandRuntimeState
{
    fn event(
        runtime: &mut Self,
        handle: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mut publish = false;
        match event {
            ext_foreign_toplevel_handle_v1::Event::Title { title } => {
                if let Some(entry) = runtime.find_by_foreign_handle_mut(handle) {
                    entry.title = Some(title);
                }
            }
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                if let Some(entry) = runtime.find_by_foreign_handle_mut(handle) {
                    entry.app_id = Some(app_id);
                }
            }
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                if let Some(entry) = runtime.find_by_foreign_handle_mut(handle) {
                    entry.identifier = Some(identifier);
                }
            }
            ext_foreign_toplevel_handle_v1::Event::Done => {
                publish = true;
            }
            ext_foreign_toplevel_handle_v1::Event::Closed => {
                if let Some(index) = runtime
                    .toplevels
                    .iter()
                    .position(|entry| &entry.foreign == handle)
                {
                    let removed = runtime.toplevels.swap_remove(index);
                    if let Some(cosmic) = removed.cosmic {
                        cosmic.destroy();
                    }
                    removed.foreign.destroy();
                }
                publish = true;
            }
            _ => {}
        }
        if publish {
            runtime.publish();
        }
    }
}

impl Dispatch<zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1, ()> for WaylandRuntimeState {
    fn event(
        runtime: &mut Self,
        _: &zcosmic_toplevel_info_v1::ZcosmicToplevelInfoV1,
        event: zcosmic_toplevel_info_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, zcosmic_toplevel_info_v1::Event::Done) {
            runtime.publish();
        }
    }
}

impl Dispatch<zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1, ()> for WaylandRuntimeState {
    fn event(
        runtime: &mut Self,
        handle: &zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1,
        event: zcosmic_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mut publish = false;
        match event {
            zcosmic_toplevel_handle_v1::Event::State { state } => {
                if let Some(entry) = runtime.find_by_cosmic_handle_mut(handle) {
                    entry.activated = parse_cosmic_state_has_activated(&state);
                    publish = true;
                }
            }
            zcosmic_toplevel_handle_v1::Event::OutputEnter { output } => {
                if let Some(entry) = runtime.find_by_cosmic_handle_mut(handle) {
                    if !entry.outputs.iter().any(|current| current == &output) {
                        entry.outputs.push(output);
                        publish = true;
                    }
                }
            }
            zcosmic_toplevel_handle_v1::Event::OutputLeave { output } => {
                if let Some(entry) = runtime.find_by_cosmic_handle_mut(handle) {
                    let before = entry.outputs.len();
                    entry.outputs.retain(|current| current != &output);
                    publish = publish || entry.outputs.len() != before;
                }
            }
            _ => {}
        }
        if publish {
            runtime.publish();
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for WaylandRuntimeState {
    fn event(
        runtime: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some(entry) = runtime.find_output_mut(output) {
                entry.name = Some(name);
                runtime.publish();
            }
        }
    }
}

fn parse_cosmic_state_has_activated(state: &[u8]) -> bool {
    const COSMIC_STATE_ACTIVATED: u32 = 2;

    state.chunks_exact(4).any(|chunk| {
        <[u8; 4]>::try_from(chunk)
            .ok()
            .map(u32::from_ne_bytes)
            .is_some_and(|value| value == COSMIC_STATE_ACTIVATED)
    })
}

#[cfg(test)]
mod tests {
    use super::{
        parse_cosmic_state_has_activated, CachedToplevel, WaylandFocusCache,
        WaylandFocusObservation, WaylandFocusSharedState,
    };
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    #[test]
    fn parse_cosmic_state_detects_activated_flag() {
        let encoded = 2u32
            .to_ne_bytes()
            .into_iter()
            .chain(1u32.to_ne_bytes())
            .collect::<Vec<u8>>();
        assert!(parse_cosmic_state_has_activated(&encoded));
    }

    #[test]
    fn wayland_cache_marks_stale_active_snapshot_as_low_confidence() {
        let active = CachedToplevel {
            identifier: Some("abc".to_string()),
            app_id: Some("Brave".to_string()),
            title: Some("Title".to_string()),
            output_names: vec!["HDMI-A-1".to_string()],
        };
        let cache = WaylandFocusCache {
            shared: Arc::new(Mutex::new(WaylandFocusSharedState {
                connected: true,
                protocols_supported: true,
                active: Some(active.clone()),
                activated_count: 1,
                last_activated: Some(active),
                last_commit_at: Some(Instant::now() - Duration::from_millis(1800)),
                last_activated_at: Some(Instant::now() - Duration::from_millis(100)),
            })),
        };

        let observed = cache.observe(1200, 200);
        match observed {
            WaylandFocusObservation::LowConfidence {
                snapshot, reason, ..
            } => {
                assert!(snapshot.focused);
                assert_eq!(snapshot.resolver, "wayland");
                assert_eq!(reason, "wayland_cache_stale");
            }
            _ => panic!("expected stale active cache to be low confidence"),
        }
    }

    #[test]
    fn wayland_cache_uses_transition_grace_low_confidence_snapshot() {
        let recent = CachedToplevel {
            identifier: Some("def".to_string()),
            app_id: Some("Code".to_string()),
            title: Some("Editor".to_string()),
            output_names: vec!["DP-1".to_string()],
        };
        let cache = WaylandFocusCache {
            shared: Arc::new(Mutex::new(WaylandFocusSharedState {
                connected: true,
                protocols_supported: true,
                active: None,
                activated_count: 0,
                last_activated: Some(recent),
                last_commit_at: Some(Instant::now()),
                last_activated_at: Some(Instant::now()),
            })),
        };

        let observed = cache.observe(1200, 200);
        match observed {
            WaylandFocusObservation::LowConfidence {
                snapshot, reason, ..
            } => {
                assert!(!snapshot.focused);
                assert_eq!(snapshot.resolver, "wayland");
                assert_eq!(reason, "wayland_transition_no_activated");
            }
            _ => panic!("expected low-confidence transition snapshot"),
        }
    }

    #[test]
    fn focus_snapshot_includes_output_name() {
        let entry = CachedToplevel {
            identifier: Some("ghi".to_string()),
            app_id: Some("Terminal".to_string()),
            title: Some("shell".to_string()),
            output_names: vec!["DP-1".to_string()],
        };

        let snapshot = entry.to_focus_snapshot(true);
        assert_eq!(snapshot.output_name.as_deref(), Some("DP-1"));
    }

    #[test]
    fn current_output_name_accepts_stale_active_snapshot() {
        let active = CachedToplevel {
            identifier: Some("jkl".to_string()),
            app_id: Some("Ghostty".to_string()),
            title: Some("terminal".to_string()),
            output_names: vec!["HDMI-A-1".to_string()],
        };
        let cache = WaylandFocusCache {
            shared: Arc::new(Mutex::new(WaylandFocusSharedState {
                connected: true,
                protocols_supported: true,
                active: Some(active.clone()),
                activated_count: 1,
                last_activated: Some(active),
                last_commit_at: Some(Instant::now() - Duration::from_secs(5)),
                last_activated_at: Some(Instant::now() - Duration::from_secs(5)),
            })),
        };

        assert_eq!(
            cache.current_output_name().as_deref(),
            Some("HDMI-A-1"),
            "overlay spawn should not deadlock on stale-but-populated active focus snapshots"
        );
    }
}
