The specific issues you are facing with `wtype` (virtual-keyboard-v1 protocol) on Cosmic DE—particularly with terminal emulators like Ghostty and Chrome-based apps like Brave—are due to the Wayland protocol implementation gaps or race conditions between the compositor (`cosmic-comp`) and the clients. The only stable alternative that bypasses the compositor's protocol layer entirely is to use **Linux Kernel User Input (`uinput`)**.

This method creates a virtual hardware device at the kernel level, making your injections indistinguishable from physical hardware events to all applications.

### Recommended Library: `mouse-keyboard-input`
For a Rust-based tool, the **`mouse-keyboard-input`** crate is the most stable "library" solution. Unlike `enigo` (which often defaults to X11 or flaky Wayland protocols), this crate explicitly targets `uinput` on Linux, ensuring compatibility across X11, Wayland, and different compositors (Cosmic, Hyprland, GNOME).

**Crate:** [`mouse-keyboard-input`](https://crates.io/crates/mouse-keyboard-input)

#### Implementation Example
This example demonstrates injecting `Ctrl+Shift+V` (common terminal paste) and a fallback text string.

```rust
use mouse_keyboard_input::VirtualDevice;
use mouse_keyboard_input::key_codes::*;
use std::thread;
use std::time::Duration;

fn main() {
    // 1. Create a virtual device (requires uinput permissions)
    let mut device = VirtualDevice::new();

    // 2. Simulate Ctrl+Shift+V (Paste in Terminals)
    // Press Modifiers
    device.press(KEY_LEFTCTRL).unwrap();
    device.press(KEY_LEFTSHIFT).unwrap();
    
    // Press V
    device.click(KEY_V).unwrap();
    
    // Release Modifiers
    device.release(KEY_LEFTSHIFT).unwrap();
    device.release(KEY_LEFTCTRL).unwrap();

    // 3. Fallback: Type text directly (if paste fails)
    // Note: This maps chars to keystrokes manually or use a helper if provided
    thread::sleep(Duration::from_millis(50));
    let text = "Hello World";
    for c in text.chars() {
        // You would need a simple mapper here for char -> keycode
        // device.click(char_to_keycode(c)).unwrap();
    }
}
```

### Critical Prerequisite: `uinput` Permissions
Since this bypasses the compositor, it requires permission to write to `/dev/uinput`. Without this, your tool will panic or fail silently.

1.  **Create a udev rule** (e.g., `/etc/udev/rules.d/99-uinput.rules`):
    ```bash
    KERNEL=="uinput", MODE="0660", GROUP="uinput", OPTIONS+="static_node=uinput"
    ```
2.  **Create the group and add your user**:
    ```bash
    sudo groupadd uinput
    sudo usermod -aG uinput $USER
    ```
3.  **Load the module** (if not loaded):
    ```bash
    echo "uinput" | sudo tee /etc/modules-load.d/uinput.conf
    sudo modprobe uinput
    ```
4.  **Reboot** to apply group and rule changes.

### Lateral Strategy: `ydotool` (Daemon approach)
If you prefer not to implement the `uinput` device handling in your Rust code, you can shell out to **`ydotool`**. It uses a client/daemon architecture where the daemon (`ydotoold`) runs as root (or with permissions) and the client sends commands.
*   **Pros:** widely tested, robust.
*   **Cons:** requires running a separate background daemon.

### Why `wtype` Fails on Cosmic/Ghostty
`wtype` relies on the `virtual-keyboard-v1` Wayland protocol.
1.  **Cosmic / Smithay:** The compositor's implementation of this protocol might have timing issues regarding focus changes or modifier states.
2.  **Ghostty:** Has known issues with synthetic input via Wayland protocols (often ignoring them to prevent "ghost" inputs or due to strict handling of seat focus).
3.  **Brave:** Chromium's ozone-wayland backend often has a delay in accepting clipboard ownership transfer, leading to the "stale paste" issue you observed.

**Solution for Stale Paste:**
The `uinput` method solves the *keystroke* reliability. For the *content* (stale paste), your file mentions a race condition where the clipboard isn't updated before the paste command fires.
*   **Fix:** In your Rust logic, after writing to the clipboard (`wl-copy`), poll `wl-paste` (or the crate equivalent) in a loop until it matches your written content *before* firing the `Ctrl+V` keystrokes via `uinput`. This "Readiness Wait" is the only way to guarantee the target app sees the new data.
