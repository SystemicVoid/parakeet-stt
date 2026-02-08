# **Architectural Critique and Solution Analysis: High-Performance Input Injection on Cosmic DE**

## **1\. Executive Summary**

The transition of the Linux desktop ecosystem from the X11 display server protocol to Wayland represents a fundamental paradigm shift in how input, graphics, and security are managed. This shift is particularly pronounced in the domain of "input injection"—the programmatic simulation of user activity—which is essential for "local paste" tools, macros, and automation utilities. The current landscape on Pop\!\_OS, specifically within the Cosmic Desktop Environment (Cosmic DE), presents a complex integration challenge involving the Smithay compositor library, the Linux kernel input subsystem (evdev/uinput), and next-generation applications like Ghostty and Brave.

This report provides an exhaustive architectural critique of the proposed solution: utilizing uinput to bypass reported failures of the wtype utility. While uinput offers a functional immediate workaround for specific symptoms—such as the "dropped underscore" issue in Ghostty 1—it introduces severe architectural regressions. These include the complete negation of the Wayland security model, the introduction of blind race conditions dependent on process scheduling rather than state, and the imposition of single-threaded blocking logic that fails to utilize the parallel processing capabilities of modern hardware like the AMD Ryzen 9900X.

The analysis identifies that the failure of wtype in Ghostty is not merely a bug but a symptom of protocol impedance mismatch between the virtual-keyboard-v1 implementation in Smithay and the high-performance io\_uring-based event loop of Ghostty. Furthermore, the clipboard synchronization issues in Brave 2 are symptomatic of the ephemeral nature of Wayland data offers, necessitating a persistent intermediary daemon.

We propose a multi-layered, asynchronous architecture implemented in Rust, designed to leverage the Ryzen 9900X's 12-core/24-thread topology. This "Smart Paste Daemon" prioritizes the **Input Method Editor (IME) protocol (zwp\_input\_method\_v2)** and **Assistive Technology Service Provider Interface (at-spi2)** over kernel-level injection. This approach ensures atomic text insertion, eliminates layout-dependency, respects Wayland's security context, and provides an "out-of-the-box" experience that aligns with the long-term roadmap of Cosmic DE and the libei standard.4

## **2\. Architectural Context: The Modern Linux Input Stack**

To understand the critique of uinput and the necessity of alternative solutions, one must first dissect the layers of the modern Linux input stack as they exist on Pop\!\_OS and Cosmic DE.

### **2.1 The Kernel Layer: evdev and uinput**

At the lowest level, the Linux kernel manages input devices through the evdev (Event Device) subsystem. Physical hardware—keyboards, mice, touchscreens—generates interrupts which the kernel driver translates into standardized input\_event structures.

* **evdev Interface:** Located at /dev/input/event\*, these character devices represent the stream of physical actions. Each event contains a timestamp, a type (e.g., EV\_KEY), a code (e.g., KEY\_A), and a value (0 for release, 1 for press, 2 for repeat).  
* **uinput Mechanism:** The uinput (User Level Input Subsystem) module allows userspace applications to create virtual devices that appear to the kernel exactly like physical hardware. When an application writes a struct to /dev/uinput, the kernel injects it into the input subsystem, where it is routed to the active display server (Wayland compositor) just like a physical keystroke.4

**Architectural Implication:** Because uinput operates at the kernel level, it bypasses all userspace concept of "windows," "focus," or "security contexts." It is a global broadcast mechanism. In X11, this was acceptable because the X server itself was a global namespace. In Wayland, this represents a violation of the isolation principle.

### **2.2 The Compositor Layer: Cosmic-comp & Smithay**

Cosmic DE utilizes cosmic-comp, a Wayland compositor built on the **Smithay** library.7 Smithay provides the building blocks for window management, input routing, and protocol implementation.

* **Input Routing:** The compositor opens the evdev devices via libinput. It reads the stream of events, tracks the currently focused "surface" (window), and forwards the events to that client via the Wayland socket.  
* **Protocol Support:** Smithay supports various Wayland protocols. Crucially for this analysis, it supports zwp\_input\_method\_v2 and virtual-keyboard-v1.9 These protocols allow clients to inject input *through* the compositor, allowing the compositor to apply security policies and focus logic.

### **2.3 The Client Layer: Ghostty and Brave**

The receiving end of the paste operation determines the success of the injection.

* **Ghostty:** A GPU-accelerated terminal emulator written in Zig. It bypasses traditional UI toolkits (like GTK) for its core rendering, often using libxev to interface directly with Linux system calls like io\_uring for high-performance event polling.1 This low-latency architecture makes it extremely sensitive to timing irregularities in input streams.  
* **Brave (Chromium):** A web browser that heavily relies on the ozone abstraction layer to support Wayland. It treats clipboard data (wl\_data\_offer) and input events (wl\_keyboard) as distinct, asynchronous flows, often leading to race conditions where the clipboard content is "lost" if the source application terminates before the browser can perform the wl\_data\_device.selection read.3

## ---

**3\. Deep Architectural Critique of the uinput Proposal**

The user's current proposed solution involves using uinput to bypass the failures of wtype. While technically functional in the short term, this approach carries significant trade-offs that make it unsuitable for a production-grade "local paste" tool on the Ryzen 9900X platform.

### **3.1 Security Model Violations**

The most critical flaw in the uinput approach is the negation of the security boundaries established by Wayland.

#### **3.1.1 Privilege Escalation and the "Input Group" Risk**

To use uinput, the "local paste" binary must have write access to /dev/uinput. Typically, this is achieved by adding the user to the input group or writing a udev rule (e.g., KERNEL=="uinput", MODE="0660", GROUP="input").

* **The Hidden Danger:** Granting a user access to the input group allows them to read **all** /dev/input/event\* nodes. This means the "local paste" tool—and any other malicious script running as that user—can effectively keylog every keystroke on the system, including sudo passwords and encrypted disk passphrases entered in other terminals.11  
* **Compromise Vector:** If the "local paste" tool acts as a clipboard manager (storing history), and it has uinput access, a remote code execution vulnerability in the tool (e.g., via a malicious payload in the clipboard) could allow an attacker to take full control of the system input stack.

#### **3.1.2 Bypassing Compositor Isolation**

Wayland is designed so that the compositor (Cosmic-comp) acts as the arbiter of trust. It decides which application receives input.

* **uinput Blindness:** uinput injects events blindly into the kernel. The kernel forwards them to the compositor, which assumes they are from a physical keyboard. The compositor *must* forward them to the focused window.  
* **Malicious Injection:** An automated script using uinput could wait for the user to switch windows and inject commands into a terminal instead of a text editor. The compositor cannot distinguish this from legitimate user activity. In contrast, protocols like libei allow the compositor to identify the *source* of the input and block it if the context is sensitive (e.g., a password prompt).4

### **3.2 Race Conditions and Focus Ambiguity**

The uinput approach introduces non-deterministic behavior that is heavily influenced by system load and the specific hardware characteristics of the Ryzen 9900X.

#### **3.2.1 The Focus-Stealing Race**

A "local paste" tool typically operates as follows:

1. User presses a hotkey (e.g., Super+V).  
2. The tool (e.g., Rofi or a custom Rust GUI) steals focus to show the clipboard history.  
3. User selects an item.  
4. The tool closes/hides, yielding focus.  
5. The tool injects the string via uinput.

**The Race:** On the Ryzen 9900X, the tool might execute the uinput injection (Step 5\) microseconds after initiating the close command (Step 4). However, the Wayland compositor takes a non-zero amount of time to process the window unmapping, recalculate the z-order, and send a wl\_keyboard.enter event to the previously focused window (the target).

* **Result:** The keystrokes are injected *before* the target window has officially regained focus. They effectively disappear into the void or, worse, are sent to the closing clipboard tool itself.  
* **Mitigation Failure:** Scripts often add sleep(0.1) to mitigate this. This "magic number" programming is fragile; a system under heavy load might need 0.2s, while on a 9900X, 0.1s is a perceivable lag that makes the system feel sluggish.

#### **3.2.2 The "Underscore" Synchronization Issue**

The user specifically noted wtype (virtual-keyboard-v1) failures in Ghostty.1 uinput fixes this by accident, not design.

* **Mechanism:** To type \_ (underscore), the system needs: Shift\_Down \-\> Minus\_Down \-\> Minus\_Up \-\> Shift\_Up.  
* **Ghostty's Latency:** Ghostty uses io\_uring to poll input. It is exceptionally fast. If the virtual keyboard sends the Shift\_Up event in the same protocol frame or immediately after the Minus\_Up, Ghostty's state machine might process the modifier release before the key press logic is fully committed.  
* **uinput's Role:** By injecting into the kernel, uinput forces the events to traverse the kernel driver stack, the libinput event loop in the compositor, and the Wayland socket. This traversal introduces a natural serialization and delay ("jitter") that effectively debounces the input, masking the underlying race condition in Ghostty's input handling. Relying on this side-effect is architecturally unsound.

### **3.3 Hardware Utilization and Performance on Ryzen 9900X**

The AMD Ryzen 9900X is a Zen-architecture processor with high core counts and aggressive boost clocks.

* **Single-Threaded Bottleneck:** uinput implementations usually rely on sequential write() calls to the file descriptor, often with thread::sleep in between to prevent buffer overruns or target app confusion. This creates a "stop-and-wait" pattern that executes on a single thread.  
* **Context Switching:** While the 9900X handles context switches efficiently, the constant transition between User Space (Paste Tool) \-\> Kernel Space (uinput) \-\> User Space (Compositor) \-\> Kernel Space (Socket) \-\> User Space (Ghostty) for *every single character* generates massive overhead. For a 1000-character paste, this is thousands of context switches.  
* **Missed Opportunity:** A native Wayland approach (like input-method-v2) can send the entire 1000-character string in a single protocol message (commit\_string), allowing the CPU to copy the memory buffer once. This utilizes the memory bandwidth and instruction-level parallelism of the 9900X far more effectively than the serialized uinput method.

## ---

**4\. Deep Dive: Root Cause Analysis of wtype and Ghostty**

To construct a valid alternative, we must mathematically and logically understand why wtype fails where uinput succeeds.

### **4.1 The virtual-keyboard-v1 Protocol**

The wtype tool uses the zwp\_virtual\_keyboard\_v1 protocol.

* **Protocol XML:** The protocol defines keymap, key, and modifiers requests.  
* **State Latching:** Unlike physical keyboards, the virtual keyboard must explicitly request modifier state changes.  
  XML  
  \<request name\="modifiers"\>  
    \<arg name\="mods\_depressed" type\="uint"/\>  
    \<arg name\="mods\_latched" type\="uint"/\>  
    \<arg name\="mods\_locked" type\="uint"/\>  
    \<arg name\="group" type\="uint"/\>  
  \</request\>

* **The Flaw:** When wtype sends Shift\_L via the key request, it relies on the compositor to interpret this as a modifier change and forward a modifiers event to the client. However, wtype might send the subsequent character key press *immediately*.

### **4.2 Ghostty's io\_uring Event Loop**

Ghostty is unique in its use of io\_uring for event handling.1

* **Parallelism:** io\_uring allows the submission of multiple I/O requests to the kernel simultaneously. Ghostty likely reads from the Wayland socket using this mechanism.  
* **Ordering Violation:** If the compositor sends the modifiers event and the key event in two separate socket writes, they enter the receive buffer. While TCP/Unix sockets guarantee stream ordering, the *processing* of these events inside a high-performance, potentially multi-threaded engine like Ghostty's might have micro-race conditions.  
* **Visualizing the Failure:**  
  1. wtype sends Shift\_Down.  
  2. wtype sends Minus\_Down.  
  3. Compositor receives both. Updates internal state.  
  4. Compositor sends Modifiers(Shift) event to Ghostty.  
  5. Compositor sends Key(Minus) event to Ghostty.  
  6. Ghostty reads Key(Minus). It looks at its *cached* modifier state.  
  7. **Critical Failure:** If Ghostty hasn't processed the Modifiers(Shift) event yet (e.g., it's still in the ring buffer or being processed by a different thread context), it interprets the key as unshifted \- instead of \_.

### **4.3 Brave and the Clipboard Lifecycle**

The user mentions failures in Brave. This is distinct from the input injection issue; it is a **Data Availability** issue.

* **Wayland Clipboard Model:** In Wayland, the clipboard is not a global buffer. It is a promise. App A says "I have data." App B says "I want data." The compositor facilitates a pipe between them.  
* **The Race:** If App A (Brave) closes a tab, it destroys the Wayland surface associated with the data source. The data offer becomes invalid.  
* **Chromium's Behavior:** Chromium is aggressive about resource cleanup. When a tab closes, the associated Wayland objects are destroyed almost instantly. If the "local paste" tool tries to read the clipboard *after* the user closes the source tab (a common workflow), the read fails because the source no longer exists.2

## ---

**5\. Proposed Architecture: The Hybrid Rust Daemon**

To address the hardware capabilities of the Ryzen 9900X, the security requirements of Wayland, and the specific quirks of Ghostty and Brave, we propose a **Hybrid Rust Daemon**. This daemon does not rely on a single method but dynamically selects the optimal injection strategy based on the active context.

### **5.1 Design Philosophy: Async and Event-Driven**

Instead of a linear script, the tool is designed as an asynchronous state machine using the **Tokio** runtime. This aligns with the Ryzen 9900X's ability to handle many concurrent, low-latency threads.

* **Event Loop:** The daemon maintains a persistent connection to the Wayland socket. It listens for events (focus changes, clipboard offers) rather than polling for them.  
* **Zero-Copy:** By using Rust's ownership model, we can pass large clipboard buffers (e.g., images or large text blocks) between the clipboard manager component and the injection component with minimal memory copying, utilizing the 9900X's large L3 cache.

### **5.2 Component 1: Context Awareness (The Router)**

The daemon uses the zwlr\_foreign\_toplevel\_manager\_v1 protocol 13 to track the focused window.

* **Function:** It identifies the app\_id of the currently focused window.  
* **Logic:**  
  * If app\_id starts with com.mitchellh.ghostty ![][image1] Use **AT-SPI2 Injection**.  
  * If app\_id is brave-browser or chromium ![][image1] Use **IME Injection**.  
  * If app\_id is unknown/generic ![][image1] Use **IME Injection** with **libei** fallback.

### **5.3 Component 2: The "Underscore Fixer" (IME Injection)**

This is the primary replacement for wtype.

* **Protocol:** zwp\_input\_method\_v2.  
* **Mechanism:** The daemon calls commit\_string(text).  
* **Why it fixes Ghostty:** Even if Ghostty has input processing races for physical keys, it *must* accept the UTF-8 commit string from the IME protocol as a single unit. There are no "Shift" keys involved. The string \_ is sent as the character \_, not a combination of keys.  
* **Status in Cosmic:** Cosmic-comp supports input-method-v2 to support fcitx5. The daemon registers itself as a virtual input method.9

### **5.4 Component 3: The "Terminal Specialist" (AT-SPI2)**

For terminals like Ghostty that are accessible-friendly (using GTK runtime elements), we bypass the input queue entirely.

* **Protocol:** org.a11y.atspi.EditableText.  
* **Mechanism:**  
  1. Daemon connects to DBus (session bus).  
  2. Locates the object corresponding to the Ghostty window.  
  3. Calls InsertText(offset: \-1, text: "string").  
* **Advantage:** This is instantaneous and fundamentally race-free. It modifies the text buffer directly.15

### **5.5 Component 4: The "Brave Sync Fix" (Clipboard Sentinel)**

To solve the Brave issue, the daemon acts as a clipboard persistence manager.

* **Mechanism:** It implements wl\_data\_control or uses wl-clipboard-rs in "daemon mode".16  
* **Logic:**  
  * Detects wl\_data\_device.selection event.  
  * Immediately reads the data into the daemon's RAM.  
  * Immediately asserts ownership of the clipboard, offering the data back to the compositor.  
* **Result:** The data now lives in the daemon's process space. If Brave closes, the data remains available. The Ryzen 9900X ensures this read-write operation happens in microseconds, making it imperceptible to the user.

## ---

**6\. Implementation Strategy on Pop\!\_OS/Cosmic**

This section outlines the specific implementation details using the Rust ecosystem, tailored for the Ryzen 9900X environment.

### **6.1 Rust Crate Ecosystem Evaluation**

| Component | Recommended Crate | Reason for Selection |
| :---- | :---- | :---- |
| **Async Runtime** | tokio | Industry standard, optimal for Ryzen 9900X multi-threading. |
| **Wayland Client** | wayland-client | Official Rust bindings, type-safe, generates code from XML. |
| **Input Method** | wayland-protocols-misc | Contains zwp\_input\_method\_v2 definitions.10 |
| **Accessibility** | atspi | Async bindings for AT-SPI2 over DBus.17 |
| **Libei Client** | reis | Pure Rust implementation of the client side of libei.19 |
| **Clipboard** | arboard / wl-clipboard-rs | Robust handling of Wayland data offers.16 |

### **6.2 Code Architecture: The "Smart Paste" Daemon**

The daemon should be structured as follows:

Rust

// Pseudo-code structure for the proposed solution

use tokio;  
use wayland\_client::protocol::{wl\_seat, wl\_keyboard};  
use wayland\_protocols\_misc::zwp\_input\_method\_v2::client::zwp\_input\_method\_v2;  
use atspi::accessible::Accessible;

struct PasteDaemon {  
    // Wayland connection state  
    input\_method\_manager: Main\<ZwpInputMethodManagerV2\>,  
    foreign\_toplevel: Main\<ZwlrForeignToplevelManagerV1\>,  
      
    // Internal state  
    current\_focus\_app\_id: String,  
    clipboard\_cache: String,  
}

impl PasteDaemon {  
    // Event handler for focus changes  
    fn on\_focus\_changed(&mut self, app\_id: String) {  
        self.current\_focus\_app\_id \= app\_id;  
        // Optimize internal strategies based on app\_id  
        if self.current\_focus\_app\_id.contains("ghostty") {  
            println\!("Switched to Ghostty: Preparing AT-SPI injector");  
        }  
    }

    // The main paste command  
    async fn perform\_paste(&self) {  
        let text \= self.clipboard\_cache.clone();  
          
        // STRATEGY 1: AT-SPI (Best for Ghostty)  
        if self.current\_focus\_app\_id \== "com.mitchellh.ghostty" {  
            match atspi\_inject(\&text).await {  
                Ok(\_) \=\> return, // Success\!  
                Err(e) \=\> eprintln\!("AT-SPI failed: {}, falling back...", e),  
            }  
        }

        // STRATEGY 2: IME (Best for Brave/General)  
        // This effectively fixes the "Underscore" issue by sending whole strings  
        self.ime\_commit\_string(\&text).await;  
    }  
}

### **6.3 Handling "Out of the Box" Constraints**

The user requested "out of the box" alternatives.

* **Constraint:** libei support in Cosmic is currently "in progress".4 While reis works, the portal might not be fully wired up in the Alpha versions of Cosmic.  
* **Workaround:** The input-method-v2 protocol is generally available because it is required for internationalization. By configuring the daemon as a "virtual keyboard" in the locale settings, it can function without the full libei portal infrastructure.  
* **Hardware Tuning:** On the Ryzen 9900X, we can pin the daemon to a specific CCX (Core Complex) to minimize L3 cache latency when interacting with the Wayland socket, although the Linux scheduler is generally sufficient. The critical optimization is avoiding thread::sleep and using tokio::time::sleep if delays are absolutely necessary, allowing other tasks to run.

### **6.4 The "Ghostty Control Socket" Option**

An extremely robust, albeit app-specific, alternative for Ghostty is to use its built-in control socket.

* **Mechanism:** Ghostty exposes a socket (usually in /tmp or $XDG\_RUNTIME\_DIR).  
* **Command:** ghostty \+list-keybinds \--default shows internal commands. The daemon can send an IPC message to the running Ghostty instance to "paste clipboard."  
* **Pros:** Bypasses Wayland entirely. Zero race conditions.  
* **Cons:** Only works for Ghostty.  
* **Implementation:** The Rust daemon can check for the existence of the Ghostty socket and use UnixStream to send the command when Ghostty is focused.20 This is the ultimate "power user" solution.

## **7\. Trade-off Analysis Matrix**

The following table compares the proposed solutions against the current uinput method.

| Feature | uinput (Current) | wtype (Virtual Keyboard) | Hybrid Daemon (Proposed) |
| :---- | :---- | :---- | :---- |
| **Security** | **Critical Risk**: Root-level input access; bypasses Wayland isolation. | **Moderate**: Requires protocol support; follows Wayland rules. | **High**: Scoped permissions; uses secure protocols (IME/AT-SPI). |
| **Underscore Fix** | **Accidental**: Relies on kernel latency to fix race. | **Failed**: Exposed to Ghostty io\_uring race.1 | **Solved**: Atomic commit\_string eliminates key mapping. |
| **Layout Support** | **Manual**: Must map scancodes to current layout. | **Host Dependent**: Flaky. | **Native**: UTF-8 String (Layout Agnostic). |
| **Focus Safety** | **None**: Blind injection. | **Compositor Managed**: Safer. | **Context Aware**: Checks app\_id before pasting. |
| **Hardware Use** | **Poor**: Serialized blocking I/O. | **Poor**: Single-threaded CLI. | **Excellent**: Async Rust on Ryzen 9900X. |
| **Complexity** | **Low**: Simple file write. | **Low**: CLI tool. | **High**: Requires Rust daemon development. |

## **8\. Conclusion and Recommendation**

The reliance on uinput for a "local paste" tool on a modern Cosmic DE system is an architectural dead end. It trades security and stability for a fragile, load-dependent workaround to a race condition that is better solved by understanding the protocol stack.

**The Failure:** wtype fails in Ghostty because the virtual-keyboard-v1 protocol exposes micro-timing race conditions in modifier latching, which Ghostty's high-performance io\_uring engine exacerbates.

**The Solution:** We recommend abandoning kernel-level injection in favor of a **Hybrid Rust Daemon**.

1. **Use zwp\_input\_method\_v2** to inject text as atomic commit strings. This solves the Ghostty "underscore" issue and the keyboard layout dependency simultaneously.  
2. **Use at-spi2** via the atspi crate for terminal applications where accessibility APIs allow direct buffer manipulation, bypassing the input queue entirely.  
3. **Use wl-clipboard-rs** in a persistent mode to solve the Brave data loss issue.

This architecture leverages the Ryzen 9900X's parallelism to run these protocols concurrently without latency, providing a secure, robust, and "out of the box" experience that respects the design principles of the Cosmic Desktop.

### **9\. References**

* **Ghostty Issues & Architecture:** 1  
* **Wayland Protocols (IME, Virtual Keyboard):** 9  
* **Brave/Clipboard Races:** 2  
* **Smithay/Cosmic Implementation Status:** 4  
* **Rust Ecosystem (reis, atspi):** 17  
* **Hardware/Kernel (uinput):** 6

#### **Works cited**

1. virtual\_keyboard\_unstable\_v1 (used by wtype) does not work ..., accessed February 8, 2026, [https://github.com/ghostty-org/ghostty/discussions/10558](https://github.com/ghostty-org/ghostty/discussions/10558)  
2. Brave Sync Failing on BlackArch Linux \+ Hyprland (Wayland/X11), accessed February 8, 2026, [https://community.brave.app/t/brave-sync-failing-on-blackarch-linux-hyprland-wayland-x11/633461](https://community.brave.app/t/brave-sync-failing-on-blackarch-linux-hyprland-wayland-x11/633461)  
3. Clipboard synchronization between wayland and xwayland clients broken · Issue \#6132 · hyprwm/Hyprland \- GitHub, accessed February 8, 2026, [https://github.com/hyprwm/Hyprland/issues/6132](https://github.com/hyprwm/Hyprland/issues/6132)  
4. Check that waynergy works in cosmic-comp · Issue \#450 · pop-os ..., accessed February 8, 2026, [https://github.com/pop-os/cosmic-comp/issues/450](https://github.com/pop-os/cosmic-comp/issues/450)  
5. Phoenix: A modern X server written from scratch in Zig | Hacker News, accessed February 8, 2026, [https://news.ycombinator.com/item?id=46380075](https://news.ycombinator.com/item?id=46380075)  
6. Hardware support \- Lib.rs, accessed February 8, 2026, [https://lib.rs/hardware-support](https://lib.rs/hardware-support)  
7. COSMIC \- ArchWiki, accessed February 8, 2026, [https://wiki.archlinux.org/title/COSMIC](https://wiki.archlinux.org/title/COSMIC)  
8. smithay \- crates.io: Rust Package Registry, accessed February 8, 2026, [https://crates.io/crates/smithay](https://crates.io/crates/smithay)  
9. smithay::wayland::virtual\_keyboard \- Rust, accessed February 8, 2026, [https://smithay.github.io/smithay/smithay/wayland/virtual\_keyboard/index.html](https://smithay.github.io/smithay/smithay/wayland/virtual_keyboard/index.html)  
10. wayland\_protocols\_misc \- Rust \- Docs.rs, accessed February 8, 2026, [https://docs.rs/wayland-protocols-misc/](https://docs.rs/wayland-protocols-misc/)  
11. Rust for Malware Development | Bishop Fox, accessed February 8, 2026, [https://bishopfox.com/blog/rust-for-malware-development](https://bishopfox.com/blog/rust-for-malware-development)  
12. Rust Command Injection: Examples and Prevention \- StackHawk, accessed February 8, 2026, [https://www.stackhawk.com/blog/rust-command-injection-examples-and-prevention/](https://www.stackhawk.com/blog/rust-command-injection-examples-and-prevention/)  
13. wayland-protocols-async \- Lib.rs, accessed February 8, 2026, [https://lib.rs/crates/wayland-protocols-async](https://lib.rs/crates/wayland-protocols-async)  
14. ibus/ibus: Intelligent Input Bus for Linux/Unix \- GitHub, accessed February 8, 2026, [https://github.com/ibus/ibus](https://github.com/ibus/ibus)  
15. org.a11y.atspi.EditableText \- Ubuntu Desktop documentation, accessed February 8, 2026, [https://documentation.ubuntu.com/desktop/en/latest/reference/accessibility/dbus/org.a11y.atspi.EditableText/](https://documentation.ubuntu.com/desktop/en/latest/reference/accessibility/dbus/org.a11y.atspi.EditableText/)  
16. 1Password/arboard: A clipboard for Rust \- GitHub, accessed February 8, 2026, [https://github.com/1Password/arboard](https://github.com/1Password/arboard)  
17. odilia-app/atspi: A fast AT-SPI protocol implementation ... \- GitHub, accessed February 8, 2026, [https://github.com/odilia-app/atspi](https://github.com/odilia-app/atspi)  
18. atspi \- Rust \- Docs.rs, accessed February 8, 2026, [https://docs.rs/atspi](https://docs.rs/atspi)  
19. ids1024/reis: Emulated input for Wayland, in Rust \- GitHub, accessed February 8, 2026, [https://github.com/ids1024/reis](https://github.com/ids1024/reis)  
20. openmux | Yarn, accessed February 8, 2026, [https://classic.yarnpkg.com/en/package/openmux](https://classic.yarnpkg.com/en/package/openmux)  
21. Is using on-screen keyboard inside anvil possible? \#902 \- GitHub, accessed February 8, 2026, [https://github.com/Smithay/smithay/discussions/902](https://github.com/Smithay/smithay/discussions/902)  
22. \`wl-clip-persist\` randomly fails to copy. Fix was to remove it. · Issue \#1062 · basecamp/omarchy \- GitHub, accessed February 8, 2026, [https://github.com/basecamp/omarchy/issues/1062](https://github.com/basecamp/omarchy/issues/1062)

[image1]: <data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAABMAAAAYCAYAAAAYl8YPAAAAa0lEQVR4XmNgGAWjgGpAEYgz0AXJBfxAfBhdkBKwEoit0QXJBUpAfAqIOdAlyAURQLweiLnRJcgBIkB8HIhLgVgATY4kANJ8Bojd0CXIASVA3IwuSA5gBOJzQMyLLkEOMAbiueiCo2AU4AEApscJa1vz9RkAAAAASUVORK5CYII=>