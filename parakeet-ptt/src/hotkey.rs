use std::fs;
use std::path::Path;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use evdev::{Device, InputEventKind, Key};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone, Copy)]
pub enum HotkeyEvent {
    Down,
    Up,
}

pub fn spawn_hotkey_loop(tx: UnboundedSender<HotkeyEvent>) -> Result<thread::JoinHandle<()>> {
    let mut devices = find_right_ctrl_devices()?;
    if devices.is_empty() {
        anyhow::bail!("no input devices exposing KEY_RIGHTCTRL were found");
    }

    let handle = thread::spawn(move || {
        let mut is_down = false;
        loop {
            for device in devices.iter_mut() {
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
                                    2 => {
                                        // auto-repeat; ignore
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        continue;
                    }
                    Err(err) => {
                        // Log-style diagnostics are not available here; just continue.
                        let _ = tx.send(HotkeyEvent::Up);
                        eprintln!("hotkey device error: {err}");
                        thread::sleep(Duration::from_millis(50));
                    }
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
    });

    Ok(handle)
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
            Ok(mut dev) => {
                if dev.supported_keys().map(|k| k.contains(Key::KEY_RIGHTCTRL)).unwrap_or(false) {
                    let _ = dev.set_non_blocking(true);
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
