use std::fs::{self, File};
use std::io::ErrorKind;

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

pub fn spawn_hotkey_loop(
    tx: UnboundedSender<HotkeyEvent>,
    talk_key: Key,
    query_modifier_key: Key,
) -> Result<HotkeyTasks> {
    let devices = find_hotkey_devices(talk_key, query_modifier_key)?;
    if devices.is_empty() {
        anyhow::bail!(
            "no input devices exposing talk key {:?} or query modifier {:?} were found",
            talk_key,
            query_modifier_key
        );
    }

    let mut handles = Vec::new();

    for mut device in devices {
        let tx = tx.clone();
        let handle = tokio::task::spawn_blocking(move || {
            let mut talk_down = false;
            let mut modifier_down = false;
            loop {
                match device.fetch_events() {
                    Ok(events) => {
                        for ev in events {
                            if let InputEventKind::Key(key) = ev.kind() {
                                if key == query_modifier_key {
                                    match ev.value() {
                                        1 => {
                                            if !modifier_down {
                                                let _ = tx.send(HotkeyEvent::QueryModifierDown);
                                                modifier_down = true;
                                            }
                                        }
                                        0 => {
                                            if modifier_down {
                                                let _ = tx.send(HotkeyEvent::QueryModifierUp);
                                                modifier_down = false;
                                            }
                                        }
                                        _ => {}
                                    }
                                    continue;
                                }

                                if key == talk_key {
                                    match ev.value() {
                                        1 => {
                                            if !talk_down {
                                                let _ = tx.send(HotkeyEvent::Down {
                                                    query_modifier_active: modifier_down,
                                                });
                                                talk_down = true;
                                            }
                                        }
                                        0 => {
                                            if talk_down {
                                                let _ = tx.send(HotkeyEvent::Up);
                                                talk_down = false;
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

fn find_hotkey_devices(talk_key: Key, query_modifier_key: Key) -> Result<Vec<Device>> {
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
                    keys.contains(talk_key) || keys.contains(query_modifier_key)
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
    use super::parse_key_name;
    use evdev::Key;

    #[test]
    fn parse_key_name_accepts_known_aliases() {
        assert_eq!(parse_key_name("KEY_RIGHTCTRL").unwrap(), Key::KEY_RIGHTCTRL);
        assert_eq!(parse_key_name("right_alt").unwrap(), Key::KEY_RIGHTALT);
    }

    #[test]
    fn parse_key_name_rejects_unknown_values() {
        assert!(parse_key_name("KEY_NOT_REAL").is_err());
    }
}
