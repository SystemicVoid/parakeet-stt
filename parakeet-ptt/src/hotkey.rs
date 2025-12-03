use std::fs::{self, File};
use std::io::ErrorKind;

use anyhow::{bail, Context, Result};
use evdev::{Device, InputEventKind, Key};
use std::path::Path;

use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

#[derive(Debug, Clone, Copy)]
pub enum HotkeyEvent {
    Down,
    Up,
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

pub fn spawn_hotkey_loop(tx: UnboundedSender<HotkeyEvent>) -> Result<HotkeyTasks> {
    let devices = find_right_ctrl_devices()?;
    if devices.is_empty() {
        anyhow::bail!("no input devices exposing KEY_RIGHTCTRL were found");
    }

    let mut handles = Vec::new();

    for mut device in devices {
        let tx = tx.clone();
        let handle = tokio::task::spawn_blocking(move || {
            let mut is_down = false;
            loop {
                match device.fetch_events() {
                    Ok(events) => {
                        for ev in events {
                            if let InputEventKind::Key(Key::KEY_RIGHTCTRL) = ev.kind() {
                                match ev.value() {
                                    1 => {
                                        if !is_down {
                                            let _ = tx.send(HotkeyEvent::Down);
                                            is_down = true;
                                        }
                                    }
                                    0 => {
                                        if is_down {
                                            let _ = tx.send(HotkeyEvent::Up);
                                            is_down = false;
                                        }
                                    }
                                    _ => {}
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

fn find_right_ctrl_devices() -> Result<Vec<Device>> {
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
                if dev
                    .supported_keys()
                    .map(|k| k.contains(Key::KEY_RIGHTCTRL))
                    .unwrap_or(false)
                {
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
