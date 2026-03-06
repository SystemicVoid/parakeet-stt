use std::collections::HashMap;
use std::fs::{self, File};
use std::io::ErrorKind;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use evdev::{Device, InputEventKind, Key};
use std::path::Path;

use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

#[derive(Debug, Clone, Copy)]
pub enum HotkeyEvent {
    Down { query_modifier_active: bool },
    Up,
    QueryModifierDown,
    QueryModifierUp,
}

pub struct HotkeyTasks {
    handles: Vec<JoinHandle<()>>,
}

#[derive(Default)]
struct HotkeySharedState {
    talk_down_count: usize,
    modifier_down_counts: HashMap<Key, usize>,
}

impl HotkeySharedState {
    fn note_modifier_down(&mut self, key: Key) -> bool {
        let was_active = self.modifier_active();
        *self.modifier_down_counts.entry(key).or_default() += 1;
        !was_active
    }

    fn note_modifier_up(&mut self, key: Key) -> bool {
        let Some(count) = self.modifier_down_counts.get_mut(&key) else {
            return false;
        };
        if *count > 1 {
            *count -= 1;
            return false;
        }
        self.modifier_down_counts.remove(&key);
        !self.modifier_active()
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

    fn modifier_active(&self) -> bool {
        !self.modifier_down_counts.is_empty()
    }
}

impl HotkeyTasks {
    pub fn len(&self) -> usize {
        self.handles.len()
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
        other => bail!("unsupported key name '{other}'"),
    }
}

pub fn parse_modifier_key_names(name: &str) -> Result<Vec<Key>> {
    let normalized = name.trim().to_ascii_uppercase();
    match normalized.as_str() {
        "KEY_ALT" | "ALT" | "ANY_ALT" => Ok(vec![Key::KEY_LEFTALT, Key::KEY_RIGHTALT]),
        _ => Ok(vec![parse_key_name(name)?]),
    }
}

pub fn spawn_hotkey_loop(
    tx: UnboundedSender<HotkeyEvent>,
    talk_key: Key,
    query_modifier_keys: Vec<Key>,
) -> Result<HotkeyTasks> {
    let devices = find_hotkey_devices(talk_key, &query_modifier_keys)?;
    if devices.is_empty() {
        anyhow::bail!(
            "no input devices exposing talk key {:?} or query modifier {:?} were found",
            talk_key,
            query_modifier_keys
        );
    }

    let mut handles = Vec::new();
    let shared_state = Arc::new(Mutex::new(HotkeySharedState::default()));

    for mut device in devices {
        let tx = tx.clone();
        let query_modifier_keys = query_modifier_keys.clone();
        let shared_state = Arc::clone(&shared_state);
        let handle = tokio::task::spawn_blocking(move || {
            loop {
                match device.fetch_events() {
                    Ok(events) => {
                        for ev in events {
                            if let InputEventKind::Key(key) = ev.kind() {
                                if query_modifier_keys.contains(&key) {
                                    match ev.value() {
                                        1 => {
                                            let mut state = shared_state
                                                .lock()
                                                .expect("hotkey shared state lock poisoned");
                                            if state.note_modifier_down(key) {
                                                let _ = tx.send(HotkeyEvent::QueryModifierDown);
                                            }
                                        }
                                        0 => {
                                            let mut state = shared_state
                                                .lock()
                                                .expect("hotkey shared state lock poisoned");
                                            if state.note_modifier_up(key) {
                                                let _ = tx.send(HotkeyEvent::QueryModifierUp);
                                            }
                                        }
                                        _ => {}
                                    }
                                    continue;
                                }

                                if key == talk_key {
                                    match ev.value() {
                                        1 => {
                                            let mut state = shared_state
                                                .lock()
                                                .expect("hotkey shared state lock poisoned");
                                            if state.note_talk_down() {
                                                let _ = tx.send(HotkeyEvent::Down {
                                                    query_modifier_active: state.modifier_active(),
                                                });
                                            }
                                        }
                                        0 => {
                                            let mut state = shared_state
                                                .lock()
                                                .expect("hotkey shared state lock poisoned");
                                            if state.note_talk_up() {
                                                let _ = tx.send(HotkeyEvent::Up);
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                    Err(err) => {
                        eprintln!("hotkey device error: {err}");
                        // If the device is gone or errored, we probably can't recover in this loop easily without re-opening.
                        // For now, we break to exit the thread for this device.
                        break;
                    }
                }
            }
        });
        handles.push(handle);
    }

    Ok(HotkeyTasks { handles })
}

fn find_hotkey_devices(talk_key: Key, query_modifier_keys: &[Key]) -> Result<Vec<Device>> {
    let mut devices = Vec::new();
    let input_dir = Path::new("/dev/input");
    for entry in fs::read_dir(input_dir).context("failed to read /dev/input")? {
        let entry = entry?;
        let path = entry.path();
        if !path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("event"))
            .unwrap_or(false)
        {
            continue;
        }

        match Device::open(&path) {
            Ok(dev) => {
                if dev.supported_keys().is_some_and(|keys| {
                    keys.contains(talk_key)
                        || query_modifier_keys
                            .iter()
                            .any(|modifier| keys.contains(*modifier))
                }) {
                    // We intentionally do NOT set O_NONBLOCK so that fetch_events blocks.
                    // let raw_fd = dev.as_raw_fd();
                    // let _ = fcntl(raw_fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK));
                    devices.push(dev);
                }
            }
            Err(err) => {
                eprintln!("failed to open {:?}: {}", path, err);
            }
        }
    }
    Ok(devices)
}

#[cfg(test)]
mod tests {
    use super::{parse_key_name, parse_modifier_key_names, HotkeySharedState};
    use evdev::Key;

    #[test]
    fn parse_key_name_accepts_known_aliases() {
        assert_eq!(parse_key_name("KEY_RIGHTCTRL").unwrap(), Key::KEY_RIGHTCTRL);
        assert_eq!(parse_key_name("right_alt").unwrap(), Key::KEY_RIGHTALT);
    }

    #[test]
    fn parse_modifier_key_names_accepts_any_alt_alias() {
        assert_eq!(
            parse_modifier_key_names("KEY_ALT").unwrap(),
            vec![Key::KEY_LEFTALT, Key::KEY_RIGHTALT]
        );
    }

    #[test]
    fn parse_key_name_rejects_unknown_values() {
        assert!(parse_key_name("KEY_NOT_REAL").is_err());
    }

    #[test]
    fn shared_state_tracks_modifiers_globally() {
        let mut state = HotkeySharedState::default();
        assert!(state.note_modifier_down(Key::KEY_RIGHTALT));
        assert!(state.modifier_active());
        assert!(!state.note_modifier_down(Key::KEY_LEFTALT));
        assert!(!state.note_modifier_up(Key::KEY_RIGHTALT));
        assert!(state.modifier_active());
        assert!(state.note_modifier_up(Key::KEY_LEFTALT));
        assert!(!state.modifier_active());
    }

    #[test]
    fn shared_state_deduplicates_talk_presses_across_devices() {
        let mut state = HotkeySharedState::default();
        assert!(state.note_talk_down());
        assert!(!state.note_talk_down());
        assert!(!state.note_talk_up());
        assert!(state.note_talk_up());
    }
}
