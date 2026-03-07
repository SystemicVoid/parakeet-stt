use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use evdev::{Device, InputEventKind, Key};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};

const HOTKEY_DEVICE_RESCAN_INTERVAL_MS: u64 = 750;
const HOTKEY_METRICS_LOG_INTERVAL_SECS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyIntent {
    Dictate,
    LlmQuery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    Down { intent: HotkeyIntent },
    Up,
}

pub struct HotkeyTasks {
    handles: Vec<JoinHandle<()>>,
    listener_count: usize,
}

#[derive(Debug)]
enum ListenerExitReason {
    OpenError(String),
    FetchError(String),
    ChannelClosed,
    TaskJoinError(String),
}

#[derive(Debug)]
struct ListenerExit {
    path: PathBuf,
    reason: ListenerExitReason,
}

#[derive(Debug, Default)]
struct HotkeyDiagnostics {
    scan_runs_total: AtomicU64,
    scan_matching_device_total: AtomicU64,
    listener_start_total: AtomicU64,
    listener_exit_total: AtomicU64,
    listener_open_error_total: AtomicU64,
    listener_fetch_error_total: AtomicU64,
    listener_task_join_error_total: AtomicU64,
    talk_down_raw_total: AtomicU64,
    talk_up_raw_total: AtomicU64,
    pre_modifier_down_raw_total: AtomicU64,
    pre_modifier_up_raw_total: AtomicU64,
    talk_down_emitted_total: AtomicU64,
    talk_down_llm_query_emitted_total: AtomicU64,
    talk_up_emitted_total: AtomicU64,
    channel_send_fail_total: AtomicU64,
}

#[derive(Debug, Clone, Copy)]
struct HotkeyDiagnosticsSnapshot {
    scan_runs_total: u64,
    scan_matching_device_total: u64,
    listener_start_total: u64,
    listener_exit_total: u64,
    listener_open_error_total: u64,
    listener_fetch_error_total: u64,
    listener_task_join_error_total: u64,
    talk_down_raw_total: u64,
    talk_up_raw_total: u64,
    pre_modifier_down_raw_total: u64,
    pre_modifier_up_raw_total: u64,
    talk_down_emitted_total: u64,
    talk_down_llm_query_emitted_total: u64,
    talk_up_emitted_total: u64,
    channel_send_fail_total: u64,
}

impl HotkeyDiagnostics {
    fn note_scan(&self, matching_devices: usize) {
        self.scan_runs_total.fetch_add(1, Ordering::Relaxed);
        self.scan_matching_device_total
            .fetch_add(matching_devices as u64, Ordering::Relaxed);
    }

    fn note_listener_started(&self) {
        self.listener_start_total.fetch_add(1, Ordering::Relaxed);
    }

    fn note_listener_exit(&self, reason: &ListenerExitReason) {
        self.listener_exit_total.fetch_add(1, Ordering::Relaxed);
        match reason {
            ListenerExitReason::OpenError(_) => {
                self.listener_open_error_total
                    .fetch_add(1, Ordering::Relaxed);
            }
            ListenerExitReason::FetchError(_) => {
                self.listener_fetch_error_total
                    .fetch_add(1, Ordering::Relaxed);
            }
            ListenerExitReason::TaskJoinError(_) => {
                self.listener_task_join_error_total
                    .fetch_add(1, Ordering::Relaxed);
            }
            ListenerExitReason::ChannelClosed => {}
        }
    }

    fn note_talk_down_raw(&self) {
        self.talk_down_raw_total.fetch_add(1, Ordering::Relaxed);
    }

    fn note_talk_up_raw(&self) {
        self.talk_up_raw_total.fetch_add(1, Ordering::Relaxed);
    }

    fn note_pre_modifier_down_raw(&self) {
        self.pre_modifier_down_raw_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn note_pre_modifier_up_raw(&self) {
        self.pre_modifier_up_raw_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn note_emitted_event(&self, event: HotkeyEvent) {
        match event {
            HotkeyEvent::Down { intent } => {
                self.talk_down_emitted_total.fetch_add(1, Ordering::Relaxed);
                if intent == HotkeyIntent::LlmQuery {
                    self.talk_down_llm_query_emitted_total
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            HotkeyEvent::Up => {
                self.talk_up_emitted_total.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn note_send_failure(&self) {
        self.channel_send_fail_total.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> HotkeyDiagnosticsSnapshot {
        HotkeyDiagnosticsSnapshot {
            scan_runs_total: self.scan_runs_total.load(Ordering::Relaxed),
            scan_matching_device_total: self.scan_matching_device_total.load(Ordering::Relaxed),
            listener_start_total: self.listener_start_total.load(Ordering::Relaxed),
            listener_exit_total: self.listener_exit_total.load(Ordering::Relaxed),
            listener_open_error_total: self.listener_open_error_total.load(Ordering::Relaxed),
            listener_fetch_error_total: self.listener_fetch_error_total.load(Ordering::Relaxed),
            listener_task_join_error_total: self
                .listener_task_join_error_total
                .load(Ordering::Relaxed),
            talk_down_raw_total: self.talk_down_raw_total.load(Ordering::Relaxed),
            talk_up_raw_total: self.talk_up_raw_total.load(Ordering::Relaxed),
            pre_modifier_down_raw_total: self.pre_modifier_down_raw_total.load(Ordering::Relaxed),
            pre_modifier_up_raw_total: self.pre_modifier_up_raw_total.load(Ordering::Relaxed),
            talk_down_emitted_total: self.talk_down_emitted_total.load(Ordering::Relaxed),
            talk_down_llm_query_emitted_total: self
                .talk_down_llm_query_emitted_total
                .load(Ordering::Relaxed),
            talk_up_emitted_total: self.talk_up_emitted_total.load(Ordering::Relaxed),
            channel_send_fail_total: self.channel_send_fail_total.load(Ordering::Relaxed),
        }
    }

    fn log_summary(&self, active_listeners: usize) {
        let snapshot = self.snapshot();
        info!(
            active_listeners,
            scan_runs_total = snapshot.scan_runs_total,
            scan_matching_device_total = snapshot.scan_matching_device_total,
            listener_start_total = snapshot.listener_start_total,
            listener_exit_total = snapshot.listener_exit_total,
            listener_open_error_total = snapshot.listener_open_error_total,
            listener_fetch_error_total = snapshot.listener_fetch_error_total,
            listener_task_join_error_total = snapshot.listener_task_join_error_total,
            talk_down_raw_total = snapshot.talk_down_raw_total,
            talk_up_raw_total = snapshot.talk_up_raw_total,
            pre_modifier_down_raw_total = snapshot.pre_modifier_down_raw_total,
            pre_modifier_up_raw_total = snapshot.pre_modifier_up_raw_total,
            talk_down_emitted_total = snapshot.talk_down_emitted_total,
            talk_down_llm_query_emitted_total = snapshot.talk_down_llm_query_emitted_total,
            talk_up_emitted_total = snapshot.talk_up_emitted_total,
            channel_send_fail_total = snapshot.channel_send_fail_total,
            "hotkey listener diagnostics"
        );
    }
}

#[derive(Default)]
struct HotkeySharedState {
    talk_down_count: usize,
    pre_modifier_down_counts: HashMap<Key, usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HotkeySharedStateReset {
    talk_down_count: usize,
    pre_modifier_down_count: usize,
}

impl HotkeySharedStateReset {
    fn had_pressed_keys(self) -> bool {
        self.talk_down_count > 0 || self.pre_modifier_down_count > 0
    }
}

impl HotkeySharedState {
    fn note_pre_modifier_down(&mut self, key: Key) {
        *self.pre_modifier_down_counts.entry(key).or_default() += 1;
    }

    fn note_pre_modifier_up(&mut self, key: Key) {
        let Some(count) = self.pre_modifier_down_counts.get_mut(&key) else {
            return;
        };
        if *count > 1 {
            *count -= 1;
            return;
        }
        self.pre_modifier_down_counts.remove(&key);
    }

    fn note_talk_down(&mut self) -> bool {
        let was_up = self.talk_down_count == 0;
        self.talk_down_count += 1;
        was_up
    }

    fn note_talk_up(&mut self) -> bool {
        if self.talk_down_count == 0 {
            return false;
        }
        self.talk_down_count -= 1;
        self.talk_down_count == 0
    }

    fn pre_modifier_active(&self) -> bool {
        !self.pre_modifier_down_counts.is_empty()
    }

    fn reset(&mut self) -> HotkeySharedStateReset {
        let reset = HotkeySharedStateReset {
            talk_down_count: self.talk_down_count,
            pre_modifier_down_count: self.pre_modifier_down_counts.values().sum(),
        };
        self.talk_down_count = 0;
        self.pre_modifier_down_counts.clear();
        reset
    }
}

impl HotkeyTasks {
    pub fn len(&self) -> usize {
        self.listener_count
    }
}

impl Drop for HotkeyTasks {
    fn drop(&mut self) {
        for handle in &self.handles {
            handle.abort();
        }
    }
}

pub fn ensure_input_access() -> Result<()> {
    let mut checked = false;
    let mut last_err: Option<(String, std::io::Error)> = None;
    let input_dir = Path::new("/dev/input");
    for entry in fs::read_dir(input_dir).context("failed to read /dev/input")? {
        let entry = entry?;
        let path = entry.path();
        let is_event = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("event"))
            .unwrap_or(false);
        if !is_event {
            continue;
        }

        checked = true;
        match File::open(&path) {
            Ok(_) => return Ok(()),
            Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                last_err = Some((path.display().to_string(), err));
            }
            Err(err) => {
                last_err = Some((path.display().to_string(), err));
            }
        }
    }
    let hint =
        "Add your user to the 'input' group with `sudo usermod -aG input $USER` and re-login.";
    if !checked {
        bail!("No /dev/input/event* devices found. {hint}");
    }
    if let Some((path, err)) = last_err {
        bail!(
            "No readable /dev/input/event* devices (last tried {}: {}). {hint}",
            path,
            err
        );
    }
    bail!("No readable /dev/input/event* devices. {hint}")
}

pub fn parse_key_name(name: &str) -> Result<Key> {
    let normalized = name.trim().to_ascii_uppercase();
    match normalized.as_str() {
        "KEY_RIGHTCTRL" | "RIGHTCTRL" | "RIGHT_CTRL" => Ok(Key::KEY_RIGHTCTRL),
        "KEY_LEFTCTRL" | "LEFTCTRL" | "LEFT_CTRL" => Ok(Key::KEY_LEFTCTRL),
        "KEY_RIGHTALT" | "RIGHTALT" | "RIGHT_ALT" => Ok(Key::KEY_RIGHTALT),
        "KEY_LEFTALT" | "LEFTALT" | "LEFT_ALT" => Ok(Key::KEY_LEFTALT),
        "KEY_RIGHTSHIFT" | "RIGHTSHIFT" | "RIGHT_SHIFT" => Ok(Key::KEY_RIGHTSHIFT),
        "KEY_LEFTSHIFT" | "LEFTSHIFT" | "LEFT_SHIFT" => Ok(Key::KEY_LEFTSHIFT),
        other => bail!("unsupported key name '{other}'"),
    }
}

pub fn parse_pre_modifier_key_names(name: &str) -> Result<Vec<Key>> {
    let normalized = name.trim().to_ascii_uppercase();
    match normalized.as_str() {
        "KEY_SHIFT" | "SHIFT" | "ANY_SHIFT" => Ok(vec![Key::KEY_LEFTSHIFT, Key::KEY_RIGHTSHIFT]),
        "KEY_ALT" | "ALT" | "ANY_ALT" => Ok(vec![Key::KEY_LEFTALT, Key::KEY_RIGHTALT]),
        _ => Ok(vec![parse_key_name(name)?]),
    }
}

pub fn spawn_hotkey_loop(
    tx: UnboundedSender<HotkeyEvent>,
    talk_key: Key,
    llm_pre_modifier_keys: Vec<Key>,
) -> Result<HotkeyTasks> {
    let initial_paths = find_hotkey_device_paths(talk_key, &llm_pre_modifier_keys)?;
    if initial_paths.is_empty() {
        anyhow::bail!(
            "no input devices exposing talk key {:?} or llm pre-modifier {:?} were found",
            talk_key,
            llm_pre_modifier_keys
        );
    }
    let listener_count = initial_paths.len();
    let shared_state = Arc::new(Mutex::new(HotkeySharedState::default()));

    let diagnostics = Arc::new(HotkeyDiagnostics::default());
    diagnostics.note_scan(initial_paths.len());
    let supervisor = tokio::spawn(run_hotkey_supervisor(
        tx,
        talk_key,
        llm_pre_modifier_keys,
        initial_paths,
        shared_state,
        diagnostics,
    ));

    Ok(HotkeyTasks {
        handles: vec![supervisor],
        listener_count,
    })
}

async fn run_hotkey_supervisor(
    tx: UnboundedSender<HotkeyEvent>,
    talk_key: Key,
    llm_pre_modifier_keys: Vec<Key>,
    initial_paths: Vec<PathBuf>,
    shared_state: Arc<Mutex<HotkeySharedState>>,
    diagnostics: Arc<HotkeyDiagnostics>,
) {
    let mut listeners: JoinSet<ListenerExit> = JoinSet::new();
    let mut active_paths: HashSet<PathBuf> = HashSet::new();

    for path in initial_paths {
        let active_path = path.clone();
        spawn_hotkey_listener(
            &mut listeners,
            path,
            tx.clone(),
            talk_key,
            llm_pre_modifier_keys.clone(),
            Arc::clone(&shared_state),
            Arc::clone(&diagnostics),
        );
        active_paths.insert(active_path);
    }

    let mut rescan_tick =
        tokio::time::interval(Duration::from_millis(HOTKEY_DEVICE_RESCAN_INTERVAL_MS));
    rescan_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut metrics_tick =
        tokio::time::interval(Duration::from_secs(HOTKEY_METRICS_LOG_INTERVAL_SECS));
    metrics_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            maybe_exit = listeners.join_next(), if !listeners.is_empty() => {
                if let Some(result) = maybe_exit {
                    match result {
                        Ok(exit) => {
                            active_paths.remove(&exit.path);
                            diagnostics.note_listener_exit(&exit.reason);
                            let reset = {
                                let mut state = shared_state
                                    .lock()
                                    .expect("hotkey shared state lock poisoned");
                                state.reset()
                            };
                            if reset.had_pressed_keys() {
                                warn!(
                                    path = %exit.path.display(),
                                    talk_down_count = reset.talk_down_count,
                                    pre_modifier_down_count = reset.pre_modifier_down_count,
                                    "reset hotkey shared state after listener exit to avoid stuck hotkeys"
                                );
                            }
                            match &exit.reason {
                                ListenerExitReason::OpenError(err) => {
                                    warn!(path = %exit.path.display(), error = %err, "hotkey listener failed to open device; waiting for rescan");
                                }
                                ListenerExitReason::FetchError(err) => {
                                    warn!(path = %exit.path.display(), error = %err, "hotkey listener device stream failed; waiting for rescan");
                                }
                                ListenerExitReason::TaskJoinError(err) => {
                                    warn!(path = %exit.path.display(), error = %err, "hotkey listener task failed unexpectedly");
                                }
                                ListenerExitReason::ChannelClosed => {
                                    warn!(path = %exit.path.display(), "hotkey listener channel closed; stopping hotkey supervisor");
                                    break;
                                }
                            }
                        }
                        Err(err) => {
                            warn!(error = %err, "hotkey listener join failed");
                        }
                    }
                }
            }
            _ = rescan_tick.tick() => {
                match find_hotkey_device_paths(talk_key, &llm_pre_modifier_keys) {
                    Ok(paths) => {
                        diagnostics.note_scan(paths.len());
                        for path in paths {
                            if active_paths.insert(path.clone()) {
                                spawn_hotkey_listener(
                                    &mut listeners,
                                    path,
                                    tx.clone(),
                                    talk_key,
                                    llm_pre_modifier_keys.clone(),
                                    Arc::clone(&shared_state),
                                    Arc::clone(&diagnostics),
                                );
                            }
                        }
                        if active_paths.is_empty() {
                            warn!("hotkey supervisor has no active listeners; waiting for /dev/input/event* recovery");
                        }
                    }
                    Err(err) => {
                        warn!(error = %err, "hotkey device rescan failed");
                    }
                }
            }
            _ = metrics_tick.tick() => {
                diagnostics.log_summary(active_paths.len());
            }
        }
    }
}

fn spawn_hotkey_listener(
    listeners: &mut JoinSet<ListenerExit>,
    path: PathBuf,
    tx: UnboundedSender<HotkeyEvent>,
    talk_key: Key,
    llm_pre_modifier_keys: Vec<Key>,
    shared_state: Arc<Mutex<HotkeySharedState>>,
    diagnostics: Arc<HotkeyDiagnostics>,
) {
    diagnostics.note_listener_started();
    listeners.spawn(async move {
        let join_path = path.clone();
        match tokio::task::spawn_blocking(move || {
            hotkey_listener_for_path(
                path,
                tx,
                talk_key,
                llm_pre_modifier_keys,
                shared_state,
                diagnostics,
            )
        })
        .await
        {
            Ok(exit) => exit,
            Err(err) => ListenerExit {
                path: join_path,
                reason: ListenerExitReason::TaskJoinError(err.to_string()),
            },
        }
    });
}

fn hotkey_listener_for_path(
    path: PathBuf,
    tx: UnboundedSender<HotkeyEvent>,
    talk_key: Key,
    llm_pre_modifier_keys: Vec<Key>,
    shared_state: Arc<Mutex<HotkeySharedState>>,
    diagnostics: Arc<HotkeyDiagnostics>,
) -> ListenerExit {
    let mut device = match Device::open(&path) {
        Ok(device) => device,
        Err(err) => {
            return ListenerExit {
                path,
                reason: ListenerExitReason::OpenError(err.to_string()),
            };
        }
    };

    debug!(
        path = %path.display(),
        name = ?device.name(),
        "hotkey listener attached"
    );

    loop {
        match device.fetch_events() {
            Ok(events) => {
                for ev in events {
                    let InputEventKind::Key(key) = ev.kind() else {
                        continue;
                    };
                    if !record_raw_key_event(
                        &diagnostics,
                        talk_key,
                        &llm_pre_modifier_keys,
                        key,
                        ev.value(),
                    ) {
                        continue;
                    }

                    let event = {
                        let mut state = shared_state
                            .lock()
                            .expect("hotkey shared state lock poisoned");
                        derive_hotkey_event(
                            &mut state,
                            talk_key,
                            &llm_pre_modifier_keys,
                            key,
                            ev.value(),
                        )
                    };

                    if let Some(event) = event {
                        diagnostics.note_emitted_event(event);
                        if tx.send(event).is_err() {
                            diagnostics.note_send_failure();
                            return ListenerExit {
                                path,
                                reason: ListenerExitReason::ChannelClosed,
                            };
                        }
                    }
                }
            }
            Err(err) => {
                return ListenerExit {
                    path,
                    reason: ListenerExitReason::FetchError(err.to_string()),
                };
            }
        }
    }
}

fn record_raw_key_event(
    diagnostics: &HotkeyDiagnostics,
    talk_key: Key,
    llm_pre_modifier_keys: &[Key],
    key: Key,
    value: i32,
) -> bool {
    if llm_pre_modifier_keys.contains(&key) {
        match value {
            1 => diagnostics.note_pre_modifier_down_raw(),
            0 => diagnostics.note_pre_modifier_up_raw(),
            _ => return false,
        }
        return true;
    }

    if key == talk_key {
        match value {
            1 => diagnostics.note_talk_down_raw(),
            0 => diagnostics.note_talk_up_raw(),
            _ => return false,
        }
        return true;
    }

    false
}

fn derive_hotkey_event(
    state: &mut HotkeySharedState,
    talk_key: Key,
    llm_pre_modifier_keys: &[Key],
    key: Key,
    value: i32,
) -> Option<HotkeyEvent> {
    if llm_pre_modifier_keys.contains(&key) {
        match value {
            1 => state.note_pre_modifier_down(key),
            0 => state.note_pre_modifier_up(key),
            _ => {}
        }
        return None;
    }

    if key == talk_key {
        return match value {
            1 if state.note_talk_down() => {
                let intent = if state.pre_modifier_active() {
                    HotkeyIntent::LlmQuery
                } else {
                    HotkeyIntent::Dictate
                };
                Some(HotkeyEvent::Down { intent })
            }
            0 if state.note_talk_up() => Some(HotkeyEvent::Up),
            _ => None,
        };
    }

    None
}

fn find_hotkey_device_paths(talk_key: Key, llm_pre_modifier_keys: &[Key]) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let input_dir = Path::new("/dev/input");
    for entry in fs::read_dir(input_dir).context("failed to read /dev/input")? {
        let entry = entry?;
        let path = entry.path();
        if !is_event_device_path(&path) {
            continue;
        }

        match Device::open(&path) {
            Ok(dev) => {
                if is_hotkey_capable_device(&dev, talk_key, llm_pre_modifier_keys) {
                    paths.push(path);
                }
            }
            Err(err) => {
                debug!(path = %path.display(), error = %err, "failed to open input device");
            }
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn is_hotkey_capable_device(device: &Device, talk_key: Key, llm_pre_modifier_keys: &[Key]) -> bool {
    device.supported_keys().is_some_and(|keys| {
        keys.contains(talk_key)
            || llm_pre_modifier_keys
                .iter()
                .any(|modifier| keys.contains(*modifier))
    })
}

fn is_event_device_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("event"))
}

#[cfg(test)]
mod tests {
    use super::{
        derive_hotkey_event, is_event_device_path, parse_key_name, parse_pre_modifier_key_names,
        HotkeyEvent, HotkeyIntent, HotkeySharedState, HotkeySharedStateReset,
    };
    use evdev::Key;
    use std::path::Path;

    #[test]
    fn parse_key_name_accepts_known_aliases() {
        assert_eq!(parse_key_name("KEY_RIGHTCTRL").unwrap(), Key::KEY_RIGHTCTRL);
        assert_eq!(parse_key_name("right_alt").unwrap(), Key::KEY_RIGHTALT);
        assert_eq!(parse_key_name("left_shift").unwrap(), Key::KEY_LEFTSHIFT);
    }

    #[test]
    fn parse_pre_modifier_key_names_accepts_any_shift_alias() {
        assert_eq!(
            parse_pre_modifier_key_names("KEY_SHIFT").unwrap(),
            vec![Key::KEY_LEFTSHIFT, Key::KEY_RIGHTSHIFT]
        );
    }

    #[test]
    fn parse_key_name_rejects_unknown_values() {
        assert!(parse_key_name("KEY_NOT_REAL").is_err());
    }

    #[test]
    fn shared_state_tracks_pre_modifiers_globally() {
        let mut state = HotkeySharedState::default();
        state.note_pre_modifier_down(Key::KEY_RIGHTSHIFT);
        assert!(state.pre_modifier_active());
        state.note_pre_modifier_down(Key::KEY_LEFTSHIFT);
        state.note_pre_modifier_up(Key::KEY_RIGHTSHIFT);
        assert!(state.pre_modifier_active());
        state.note_pre_modifier_up(Key::KEY_LEFTSHIFT);
        assert!(!state.pre_modifier_active());
    }

    #[test]
    fn shared_state_deduplicates_talk_presses_across_devices() {
        let mut state = HotkeySharedState::default();
        assert!(state.note_talk_down());
        assert!(!state.note_talk_down());
        assert!(!state.note_talk_up());
        assert!(state.note_talk_up());
    }

    #[test]
    fn shared_state_reset_clears_stuck_pressed_keys() {
        let mut state = HotkeySharedState::default();
        state.note_pre_modifier_down(Key::KEY_LEFTSHIFT);
        assert!(state.note_talk_down());

        assert_eq!(
            state.reset(),
            HotkeySharedStateReset {
                talk_down_count: 1,
                pre_modifier_down_count: 1
            }
        );
        assert!(!state.pre_modifier_active());
        assert!(state.note_talk_down());
    }

    #[test]
    fn derive_hotkey_event_snapshots_llm_intent_on_talk_press() {
        let mut state = HotkeySharedState::default();
        let llm_pre_modifiers = vec![Key::KEY_LEFTSHIFT, Key::KEY_RIGHTSHIFT];
        assert_eq!(
            derive_hotkey_event(
                &mut state,
                Key::KEY_RIGHTCTRL,
                &llm_pre_modifiers,
                Key::KEY_LEFTSHIFT,
                1
            ),
            None
        );
        assert_eq!(
            derive_hotkey_event(
                &mut state,
                Key::KEY_RIGHTCTRL,
                &llm_pre_modifiers,
                Key::KEY_RIGHTCTRL,
                1
            ),
            Some(HotkeyEvent::Down {
                intent: HotkeyIntent::LlmQuery
            })
        );
    }

    #[test]
    fn derive_hotkey_event_defaults_to_dictate_without_pre_modifier() {
        let mut state = HotkeySharedState::default();
        let llm_pre_modifiers = vec![Key::KEY_LEFTSHIFT, Key::KEY_RIGHTSHIFT];
        assert_eq!(
            derive_hotkey_event(
                &mut state,
                Key::KEY_RIGHTCTRL,
                &llm_pre_modifiers,
                Key::KEY_RIGHTCTRL,
                1
            ),
            Some(HotkeyEvent::Down {
                intent: HotkeyIntent::Dictate
            })
        );
    }

    #[test]
    fn derive_hotkey_event_ignores_key_repeat_values() {
        let mut state = HotkeySharedState::default();
        let llm_pre_modifiers = vec![Key::KEY_LEFTSHIFT];
        assert_eq!(
            derive_hotkey_event(
                &mut state,
                Key::KEY_RIGHTCTRL,
                &llm_pre_modifiers,
                Key::KEY_LEFTSHIFT,
                2
            ),
            None
        );
        assert_eq!(
            derive_hotkey_event(
                &mut state,
                Key::KEY_RIGHTCTRL,
                &llm_pre_modifiers,
                Key::KEY_RIGHTCTRL,
                2
            ),
            None
        );
    }

    #[test]
    fn event_device_path_filter_is_strict() {
        assert!(is_event_device_path(Path::new("/dev/input/event6")));
        assert!(!is_event_device_path(Path::new("/dev/input/mouse0")));
        assert!(!is_event_device_path(Path::new(
            "/dev/input/by-id/keyboard"
        )));
    }
}
