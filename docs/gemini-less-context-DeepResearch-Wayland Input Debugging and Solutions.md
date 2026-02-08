# **Architectural Analysis of Input Injection Vectors in Wayland: From Kernel Primitives to User-Space Protocols**

## **1\. The Divergence of Input Architectures: X11 Legacy vs. Wayland Isolation**

The transition from the X Window System (X11) to Wayland represents the most significant architectural shift in the Linux desktop ecosystem in decades. This transition is not merely a replacement of display protocols; it is a fundamental re-architecting of the privilege model governing input devices, event processing, and inter-process communication (IPC). For systems engineers and architects working within high-performance environments—such as a Ryzen 9900X workstation running Pop\!\_OS—understanding this divergence is critical to resolving automation failures that arise from the obsolescence of global namespaces.

### **1.1 The Monolithic Trust Model of X11**

Under the X11 architecture, the X Server functions as a central, omniscient broker. It maintains a global state of all windows, input devices, and clipboards. This architecture was conceived in an era of trusted computing where isolation between applications was not a primary design constraint.

In this model, any client connected to the X Server typically possesses implicit privileges to query the entire window tree, inspect properties of other windows, and, crucially, inject synthetic input events. The XTEST extension, widely utilized by tools like xdotool, allows a client to request the server to generate input events that are indistinguishable from hardware interrupts to the receiving client.

The mechanism of XTEST is straightforward: a client sends a request to the server to simulate a key press. The server processes this request and creates an event, which it then dispatches to the window currently holding input focus. Because the X Server manages the focus and the event queue globally, the injection is synchronous and reliable. The client injecting the event does not need to know which application is focused; it simply instructs the server to "press 'A'", and the server handles the routing.

This architectural simplicity enabled a rich ecosystem of automation tools but introduced catastrophic security vulnerabilities. Keyloggers, screen scrapers, and "clickjacking" attacks are trivial in X11 because there is no barrier preventing Client A from snooping on Client B’s input stream.

### **1.2 The Compartmentalized Security Model of Wayland**

Wayland inverts the X11 model. It is defined by a strict adherence to isolation and the principle of least privilege. In a Wayland session, the compositor (in the case of Pop\!\_OS Cosmic, cosmic-comp, built upon the smithay library) acts as the display server, window manager, and input arbitrator.

There is no shared global coordinate space visible to clients. A Wayland client only knows about its own surfaces. It cannot query the position of other windows, nor can it determine which window currently holds focus unless it is the focused window itself. This isolation breaks the fundamental mechanism of X11-style automation. A client cannot inject events into the global input stream because, conceptually, there is no global input stream accessible to user-space applications—there are only discrete input streams routed by the compositor to specific clients.

Input injection in Wayland therefore requires the compositor to explicitly expose an interface for it. This has led to a fragmented landscape where automation capabilities depend entirely on the specific compositor implementation and the protocols it chooses to support.

### **1.3 Protocol Fragmentation: The Ecosystem Split**

The lack of a centralized standard for input injection in the early days of Wayland led to divergent implementations, creating a compatibility minefield for developers.

**Table 1: Comparison of Wayland Input Injection Architectures**

| Feature | wlroots (Sway, Hyprland) | GNOME (Mutter) | Smithay (Cosmic/Pop\!\_OS) |
| :---- | :---- | :---- | :---- |
| **Primary Protocol** | virtual-keyboard-unstable-v1 | libei (Emulated Input) | virtual-keyboard-unstable-v1 (Legacy) / libei (Future) |
| **Automation Tool** | wtype, ydotool | gnome-remote-desktop | wtype (Partial support) |
| **Security Model** | Protocol-based access | Portal-based permissions | Hybrid (Transitioning) |
| **Keymap Handling** | Client uploads keymap | Server/Portal managed | Client uploads keymap |
| **Reliability** | Variable (Compositor dependent) | High (Strict API) | **Low** (Current Alpha state) |

The virtual-keyboard-unstable-v1 protocol 1, utilized by wtype, attempts to bridge this gap by allowing a client to create a virtual input device. However, as the "unstable" suffix suggests, this protocol is not guaranteed to be consistent across implementations. It relies on the client (the automation tool) to upload a keymap to the compositor. The compositor then maps the injected keycodes to the target application's expectations.

This handshake is the source of the failure observed in Ghostty. When wtype injects a key, it is effectively saying, "I am pressing keycode X, which corresponds to symbol Y in my uploaded keymap." If the compositor (cosmic-comp) or the target client (Ghostty) disagrees on the keymap state—specifically regarding modifier latches like Shift or Control—the event is misinterpreted or dropped. This is a classic distributed state problem where the sender (wtype), the broker (cosmic-comp), and the receiver (Ghostty) must all be perfectly synchronized regarding the state of the virtual keyboard. In high-performance, asynchronous environments, this synchronization frequently breaks.

## ---

**2\. Forensic Analysis of the Ghostty Failure**

The specific symptoms reported—the dropping of underscores and the unreliable injection of keystrokes in the Ghostty terminal emulator—provide a case study in the fragility of user-space protocol injection.

### **2.1 The Ghostty Architecture: Zig and High-Performance I/O**

Ghostty is distinguished by its use of the Zig programming language and a custom, high-performance event loop architecture. Unlike traditional terminal emulators that might rely on GLib or Qt event loops, Ghostty implements its own runtime, often leveraging libxev to interface with system primitives. On Linux, this frequently involves io\_uring for asynchronous I/O submission and completion.2

io\_uring represents a paradigm shift in Linux I/O. Instead of blocking syscalls, the application submits requests to a ring buffer shared with the kernel and polls a completion ring for results. This reduces context switching overhead and maximizes throughput, aligning with the "System Symbiosis" principle of leveraging the Ryzen 9900X's capabilities.

However, this asynchronous model introduces complexity in handling strictly ordered sequential events, such as keyboard input. A physical keystroke sequence for an underscore (\_) on a standard US layout involves four distinct events:

1. **Shift Press:** EV\_KEY (Left Shift) \-\> State 1 (Pressed)  
2. **Minus Press:** EV\_KEY (Minus) \-\> State 1 (Pressed)  
3. **Minus Release:** EV\_KEY (Minus) \-\> State 0 (Released)  
4. **Shift Release:** EV\_KEY (Left Shift) \-\> State 0 (Released)

For the terminal to render \_, the Shift modifier must be active (latched) at the exact moment the Minus press is processed by the input method editor (IME) or terminal state machine.

### **2.2 The "Underscore" Bug: Race Conditions in State Machines**

The failure mechanism observed in Ghostty 2 suggests a desynchronization between the virtual keyboard protocol events and Ghostty's internal state processing.

When wtype sends these events via virtual-keyboard-unstable-v1, they are transmitted as Wayland protocol messages to the compositor, which then forwards them to Ghostty. Because these are protocol messages, they are subject to IPC latency and scheduler variance.

If Ghostty's libxev loop retrieves the Shift Release event from the socket buffer in the same batch as the Minus Press, or if the internal logic handling modifier state updates has a race condition, the application may process the key press with the modifier already cleared.

* **Scenario A (Correct):** Shift Down \-\> (State: Shifted) \-\> Minus Down \-\> Resolve '\_' \-\> Minus Up \-\> Shift Up.  
* **Scenario B (Race/Coalescing):** Shift Down \-\> Minus Down \-\> Shift Up \-\> (Batch Process) \-\> Shift Release processed first or simultaneously \-\> State cleared \-\> Minus resolves to '-'.  
* **Scenario C (Dropped Event):** The compositor receives the burst of events from wtype but drops one due to buffer constraints or focus ambiguity, leading to a partial sequence that produces no output or incorrect output.

The snippet 3 referencing dead keys and 4 regarding IME handling in Ghostty further illuminates the issue. The logic if (was\_composing) return true; in Ghostty's input handler indicates that if the terminal believes it is in a composition state (initiated by a modifier or dead key), it may swallow subsequent keys while waiting for a commit signal. Virtual keyboards often fail to send the precise sequence of "pre-edit" and "commit" events required by complex IME implementations, causing the terminal to hang in a composing state and discard the input.

### **2.3 Smithay and cosmic-comp: The Middleman**

The cosmic-comp compositor is built on smithay.5 Smithay implements the server-side logic for Wayland protocols. As a "library for building compositors" rather than a monolithic server like Mutter, Smithay relies on the implementer (cosmic-comp) to wire up the event propagation correctly.

The research indicates that Smithay's implementation of virtual-keyboard-v1 is "Done" 5, but this binary status does not account for the nuances of focus handling. wtype is a CLI tool. When executed, it must momentarily assert itself to the compositor to create the virtual seat. If this assertion coincides with the window management logic of cosmic-comp (e.g., handling focus changes, workspace switching, or tiling layout adjustments), the virtual seat's focus target might be undefined for a microsecond.

If wtype blasts its event payload during this microsecond of undefined focus, the events fall into the void. This explains the "missing characters" symptom reported by the user—specifically that it happens after an arbitrary number of characters. It is a probabilistic failure dependent on the scheduling of the compositor's render loop versus the client's input loop.

### **2.4 Conclusion on User-Space Injection**

The failure of wtype on Ghostty/Cosmic is not a simple bug; it is a systemic failure of the user-space protocol injection model in its current immature state. The reliance on cooperative state management between three independent asynchronous actors (wtype, cosmic-comp, Ghostty) creates a fragility that cannot be engineered away with simple shell scripts or delays. A robust solution requires bypassing this cooperative layer entirely.

## ---

**3\. The Stale Paste & Clipboard Synchronization**

The second failure mode—the "Stale Paste"—is distinct from the input injection failure. It is a data consistency race condition inherent to the Wayland clipboard model.

### **3.1 The Wayland Clipboard Lifecycle**

In X11, the clipboard was a property of the X Server. In Wayland, the clipboard is a peer-to-peer data transfer negotiation brokered by the compositor.

**The Copy Process (wl-copy):**

1. **Announcement:** The copying client creates a wl\_data\_source and advertises the available MIME types (e.g., text/plain, text/html) to the compositor.  
2. **Focus Update:** The compositor sends a selection event to the currently focused client (and others listening via wl\_data\_device), notifying them that a new data source is available.  
3. **Lazy Evaluation:** Crucially, **no data is transferred at this stage.** The clipboard contains only a pointer to the copying client.

**The Paste Process (wl-paste / Ctrl+V):**

1. **Request:** The pasting client (Ghostty) receives the user command (Ctrl+V).  
2. **Negotiation:** It requests data from the current selection's wl\_data\_source in a specific format (e.g., text/plain;charset=utf-8).  
3. **Pipe Creation:** The pasting client provides a file descriptor (pipe) to the copying client.  
4. **Transfer:** The copying client writes the data to the pipe. The pasting client reads until EOF.

### **3.2 The Race Mechanism**

The user's automation script likely performs the following sequence:

Bash

wl-copy "New Payload"  
wtype \-M ctrl \-M shift \-k v \-m shift \-m ctrl

The failure occurs because wl-copy exits as soon as it has successfully *advertised* the new selection to the compositor. It does not wait for the target application (Ghostty) to acknowledge or process this advertisement.

When wtype immediately injects Ctrl+Shift+V, Ghostty's input handler processes the keypress. If Ghostty has not yet received or processed the selection event from the compositor (which arrives asynchronously via the Wayland socket), Ghostty still holds a reference to the *previous* wl\_data\_source.

Consequently, Ghostty initiates a data transfer request to the *old* clipboard owner. The "stale" data is pasted. By the time the new selection event arrives and is processed, the paste action has already completed.

This is a classic "Time-of-Check to Time-of-Use" (TOCTOU) race condition, but distributed across IPC boundaries.

### **3.3 The Solution: Readiness Wait Barrier**

To solve this, the automation architecture must enforce a barrier synchronization. We cannot simply "wait 100ms" because system load (compilation jobs on the Ryzen 9900X) can induce variance in IPC latency. We must poll the system state until convergence is observed.

**The Algorithm:**

1. **Write Phase:** Invoke wl-copy with the new payload.  
2. **Verification Phase:** Enter a polling loop.  
   * Initiate a read request (wl-paste or equivalent).  
   * Compare the returned data hash with the payload hash.  
   * **Condition:** If Hash(Read)\!= Hash(Write), sleep Interval and retry.  
   * **Condition:** If Hash(Read) \== Hash(Write), the system has converged. Proceed.  
3. **Injection Phase:** Only now, trigger the Ctrl+Shift+V injection.

This approach guarantees that the clipboard state perceived by the system (and thus Ghostty) matches the intended state before the trigger is pulled.

## ---

**4\. The Kernel Path: uinput and evdevil**

Given the architectural deficiencies of the protocol path (wtype) and the synchronization requirements of the clipboard, the optimal solution for "Forge" to recommend is the **Kernel Path**. This involves using the Linux Kernel Input Subsystem (uinput) to create a virtual hardware device.

### **4.1 The Kernel Input Subsystem Architecture**

The Linux Input Subsystem serves as the abstraction layer between hardware drivers and user-space applications. Physical devices (keyboards, mice) communicate with kernel drivers (e.g., usbhid), which translate hardware signals into standardized input events defined in \<linux/input.h\>.

These events (EV\_KEY, EV\_REL, EV\_ABS) are published to character devices in /dev/input/event\*. User-space consumers, such as libinput (used by Wayland compositors) or Xorg's evdev driver, read from these nodes.

The uinput module allows a user-space application to do the reverse: write events *into* the kernel subsystem, which then exposes them as a read-only /dev/input/event\* node to the rest of the system.

**Advantages for Automation:**

1. **Universal Compatibility:** To the compositor (cosmic-comp) and the client (Ghostty), the input is indistinguishable from a physical USB keyboard. It does not rely on Wayland protocols. It works in X11, Wayland, TTYs, and even during early boot (if the module is loaded).  
2. **Timing Precision:** Events are time-stamped by the kernel upon injection.  
3. **Bypass Protocol Bugs:** Since the injection happens below the display server layer, bugs in virtual-keyboard-unstable-v1 or libxev's handling of synthetic protocol events are irrelevant. Ghostty sees a "real" keypress.

### **4.2 Rust Implementation: evdevil / evdev-rs**

To implement this in Rust, ensuring high performance and type safety, we utilize crates that bind to libevdev. evdevil 6 and evdev-rs 7 are the primary candidates. mouse-keyboard-input is a higher-level wrapper, but for the "Deep Research" mandate, we examine the lower-level implementation for maximum control.

**Device Creation:**

The creation of a uinput device requires defining its capabilities. This is a critical step for stealth (anti-cheat evasion) and functionality. A "keyboard" that only declares the 'A' key but no 'Enter' key looks suspicious and might fail to trigger certain input stack logic.

Rust

// Concept Code: High-Fidelity Device Creation  
use evdevil::{DeviceBuilder, InputId, BusType, Key, UInputDevice};

fn create\_virtual\_device() \-\> UInputDevice {  
    let device \= DeviceBuilder::new()  
       .name("Forge Virtual Interface")  
       .input\_id(InputId {  
            bus: BusType::USB,  
            vendor: 0x1d6b, // Linux Foundation  
            product: 0x0104,  
            version: 0x01,  
        })  
        // Enable synchronization events (CRITICAL)  
       .enable\_event\_type(\&evdevil::EventType::Synchronization)  
       .enable\_event\_type(\&evdevil::EventType::Key)  
        // Enable all standard keys to mimic a full keyboard  
       .enable\_key(Key::KEY\_A)  
       .enable\_key(Key::KEY\_B)  
        //... (Iterate all keys)  
       .enable\_key(Key::KEY\_LEFTSHIFT)  
       .enable\_key(Key::KEY\_LEFTCTRL)  
       .enable\_key(Key::KEY\_V)  
       .build()  
       .expect("Failed to create uinput device");  
      
    return device;  
}

**Event Injection & Synchronization:**

The Linux input protocol is event-based but grouped by synchronization frames. A "keystroke" is not a single signal.

To press 'A':

1. Write EV\_KEY, KEY\_A, 1 (Value 1 \= Press).  
2. Write EV\_SYN, SYN\_REPORT, 0 (Flush the frame).

To release 'A':

1. Write EV\_KEY, KEY\_A, 0 (Value 0 \= Release).  
2. Write EV\_SYN, SYN\_REPORT, 0\.

If EV\_SYN is omitted, the consumer (libinput) will buffer the event indefinitely, waiting for the frame to close. This is a common source of "my uinput code does nothing" errors.

### **4.3 Permissions and udev Strategy**

Accessing /dev/uinput requires root privileges by default. Running the automation agent as root violates the security principle of least privilege and complicates integration with user-session tools (like wl-clipboard).

**The udev Solution:**

We can use the Linux device manager (udev) to change the permissions of the uinput node dynamically.

**File:** /etc/udev/rules.d/99-forge-uinput.rules

Code snippet

KERNEL=="uinput", MODE="0660", GROUP="uinput", OPTIONS+="static\_node=uinput"

**Setup Command:**

Bash

sudo groupadd \-f uinput  
sudo usermod \-aG uinput $USER  
echo "uinput" | sudo tee /etc/modules-load.d/uinput.conf  
sudo modprobe uinput  
sudo udevadm control \--reload-rules && sudo udevadm trigger

This configuration grants any user in the uinput group read/write access to the interface. This allows the Rust agent to run as a standard user process, integrating seamlessly with the user's Wayland session and environment variables.

### **4.4 Hardware Spoofing and Stealth**

For applications sensitive to automation (Anti-Cheat), a generic uinput device is a red flag.

* **Vendor/Product ID:** Use lsusb to find the IDs of a real keyboard (e.g., Corsair, Logitech) and replicate them in the InputId struct.  
* **Device Topology:** The uinput device appears on a virtual bus. Advanced heuristics can detect this. However, most Linux anti-cheats (even kernel-level ones like EAC for Linux) are currently less aggressive than their Windows counterparts regarding bus topology, focusing instead on behavior (timing).  
* **Jitter:** The Rust implementation must strictly avoid sleep(10ms). It should use a randomized delay: sleep(Duration::from\_millis(rand::thread\_rng().gen\_range(8..14))).

## ---

**5\. The Future Path: libei and Portals**

While uinput is the pragmatic solution for today, the Linux desktop ecosystem is converging on a different standard: libei (Emulated Input). It is crucial for a Systems Architect to understand this trajectory to avoid technical debt.

### **5.1 The libei Architecture**

libei (Client) and libeis (Server) provide a dedicated protocol for input emulation that is distinct from the Wayland display protocol.

* **Separation of Concerns:** Unlike virtual-keyboard-v1, which is a Wayland extension, libei is a separate stream. It allows for strict separation between the "Display" (Wayland) and "Input Emulation" (EI).  
* **Strict Typing:** libei defines semantic input types. It knows the difference between a "scroll" event and a "key" event at a high level, whereas uinput deals in raw kernel codes.  
* **Receiver-Side Constraints:** The compositor (libeis implementation) can enforce constraints, such as identifying *which* client sent the event.

### **5.2 The Portal Workflow (XDG RemoteDesktop)**

The primary entry point for libei is the XDG Desktop Portal system. This provides a secure, user-consented pathway for automation.

1. **Request:** The automation app calls the RemoteDesktop portal via D-Bus.  
2. **Consent:** The portal displays a system dialog: "App X wants to control your keyboard/mouse. Allow?"  
3. **Session:** Upon approval, the portal hands back a file descriptor.  
4. **Connection:** The app passes this FD to libei, which establishes a secure channel to the compositor.

### **5.3 Current State in Cosmic (Smithay)**

As of early 2026, libei support in the Cosmic ecosystem is in active development but not fully mature for general-purpose automation.8

* **Smithay:** Has initial support for libeis.  
* **Cosmic Comp:** Is integrating the RemoteDesktop portal backend.  
* **Status:** While input-leap (software KVM) is driving this development, using it for a simple "paste" script today would require navigating unstable APIs and potentially handling D-Bus portal interaction boilerplate that is significantly more complex than the uinput solution.

**Rust Crate: reis** The reis crate 10 provides a pure Rust implementation of the client side (libei). It parses the EI protocol. For a "pure Wayland" solution in the future, the Forge agent would drop the evdevil dependency and adopt reis. This would allow the tool to work in sandboxed environments (like Flatpaks) *if* the portal permissions are granted, bypassing the /dev/uinput restriction.

## ---

**6\. Second and Third Order Consequences**

Implementing a kernel-level input injection system (uinput) has profound implications beyond simply "fixing the paste button." These consequences must be managed to ensure system stability and security.

### **6.1 Security: The Sandboxing Breach (Flatpak)**

The user's environment includes prek, claude code cli, and various web tools. If any of these are installed via Flatpak, the uinput solution faces a hard boundary.

**The Flatpak Constraint:**

Flatpak containers utilize kernel namespaces to isolate the application from the host system. The /dev filesystem inside the container is virtualized. Crucially, /dev/uinput is **not** exposed to the container by default. This is a security feature to prevent sandboxed apps from keylogging the host.

**The "Nuclear" Bypass:**

To make the Rust tool work inside a Flatpak, one would need to run:

flatpak override \--user \--device=all com.example.MyTool

This grants access to all device nodes. This is a catastrophic degradation of the sandbox. If the tool is compromised (e.g., via a supply chain attack on a crate dependency), it has full access to the host's webcam, microphone, disks, and input devices.

**The "Sidecar" Architecture (Recommended):**

To maintain security while enabling functionality, we must decouple the *Command* from the *Execution*.

1. **The Agent (Host):** The Rust uinput binary runs as a systemd user service on the host. It listens on a Unix Domain Socket (e.g., /run/user/1000/forge.sock).  
2. **The Client (Flatpak):** The sandboxed application connects to this socket (sockets can be exposed to Flatpaks selectively via \--filesystem=/run/user/1000/forge.sock).  
3. **Protocol:** The Client sends {"action": "paste", "payload": "..."}. The Agent performs the uinput injection.  
   This limits the attack surface. The sandboxed app can *only* trigger predefined actions; it cannot arbitrarily read keystrokes or access other hardware.

### **6.2 Gaming: Anti-Cheat and Heuristics**

The user explicitly mentioned "Anti-Cheat" contexts. This is the most volatile third-order consequence.

**Heuristic Analysis of uinput:**

Modern anti-cheat systems (BattlEye, Easy Anti-Cheat, Vanguard) operate partially in kernel space (on Windows) or as privileged user-space processes (on Linux). They scan /sys/class/input to build a topology of connected devices.

* **Red Flag:** A device named "rust-keyboard" with BUS\_VIRTUAL (0x06).  
* **Red Flag:** A device that appears and disappears frequently.  
* **Red Flag:** A device with zero jitter in input timing.

**Ban Risk:**

Using a naive uinput script in a multiplayer game (e.g., Apex Legends, Overwatch 2 via Proton) can lead to a permanent account ban. The anti-cheat interprets the synthetic input as a macro or aimbot.

**Mitigation: Hardware Spoofing:**

The Rust agent must spoof the identity of a legitimate peripheral.

* **Identity:** Set vendor, product, and version fields in InputId to match a real device (e.g., a generic Dell keyboard).  
* **Capabilities:** The device should report capability bits (EV\_KEY) for *all* standard keys, not just the ones being injected. A "keyboard" that only has the 'V' key is anomalous.  
* **Permanence:** The uinput device should be created once (at login) and kept open by a daemon, rather than created/destroyed for every paste operation. This mimics a physical device remaining plugged in.

### **6.3 System Stability: The Ring Buffer**

Flooding the input subsystem can cause denial of service. If the Rust loop spins without sleep (e.g., due to a logic bug in the "Readiness Wait" loop), it can fill the kernel's event ring buffer.

* **Symptom:** The system becomes unresponsive to real mouse/keyboard input because the buffer is full of synthetic events.  
* **Mitigation:** Implement a "Dead Man's Switch" or rate limiter in the Rust agent. Ensure that if the process receives SIGINT or panics, it cleans up the uinput device handle to prevent "stuck keys" (where a Key Press is sent, but the process dies before sending Key Release, causing the OS to repeat the key indefinitely).

## ---

**7\. Strategic Recommendations**

Based on this deep analysis, the following strategy is formulated for Hugo's environment.

### **7.1 The "Forge" Input Agent (Rust)**

We reject wtype. We build a custom tool, forge-input.

**Dependencies:**

* evdevil (or evdev-rs) for uinput control.  
* wl-clipboard-rs for native clipboard polling.  
* clap for CLI argument parsing.  
* tokio for the async runtime (to handle socket listeners if using the Sidecar pattern).

**Core Logic:**

1. **Startup:**  
   * Initialize uinput device with Spoofed ID (Logitech K120).  
   * Drop privileges (if started as root, though udev rules preferred).  
2. **Command: Paste:**  
   * **Step 1:** Read current clipboard hash (![][image1]).  
   * **Step 2:** Execute wl-copy with new content.  
   * **Step 3 (The Guard):** Loop with 15ms delay. Read clipboard hash (![][image2]). Break when ![][image3] AND ![][image4]. Timeout at 500ms.  
   * **Step 4 (The Injection):**  
     * emit(EV\_KEY, KEY\_LEFTCTRL, 1\)  
     * emit(EV\_KEY, KEY\_LEFTSHIFT, 1\)  
     * emit(EV\_KEY, KEY\_V, 1\)  
     * emit(EV\_SYN, SYN\_REPORT, 0\)  
     * *Sleep(Random(12ms, 22ms))*  
     * emit(EV\_KEY, KEY\_V, 0\)  
     * emit(EV\_KEY, KEY\_LEFTSHIFT, 0\)  
     * emit(EV\_KEY, KEY\_LEFTCTRL, 0\)  
     * emit(EV\_SYN, SYN\_REPORT, 0\)

### **7.2 Deployment Configuration**

1. **Permissions:**  
   Apply the udev rule to /etc/udev/rules.d/99-uinput.rules.  
2. **Environment:**  
   Add the uinput group to the user: usermod \-aG uinput hugo.  
3. **Flatpak Bridge:**  
   If automation is needed from within VS Code (Flatpak), write a small script in \~/.local/bin/forge-paste that writes to the forge-input socket, and map that socket into the container.

### **7.3 Long-Term Roadmap**

* **Q1 2026:** Rely on forge-input (uinput).  
* **Q3 2026:** Monitor cosmic-comp changelogs for "XDG RemoteDesktop Portal" stabilization.  
* **Q4 2026:** Begin porting forge-input to use the reis crate (libei) to align with the "System Symbiosis" principle of native Wayland integration, once the Smithay implementation matures.

This architecture solves the immediate pain points (Underscores, Stale Paste) with maximum leverage (robust code, minimal manual intervention once deployed) while fortifying against the security and stability risks inherent in kernel-level automation.

---

**Report Metadata**

* **Primary Technologies:** Rust (evdevil, wl-clipboard-rs), Wayland (virtual-keyboard-v1), Linux Kernel (uinput), Smithay.  
* **Hardware Context:** AMD Ryzen 9 9900X (High concurrency for compilation of Rust agents).  
* **Software Context:** Pop\!\_OS / Cosmic DE (Smithay-based).  
* **Date:** February 2026\.

#### **Works cited**

1. rcalixte/awesome-wayland: A curated list of Wayland resources \- GitHub, accessed February 8, 2026, [https://github.com/rcalixte/awesome-wayland](https://github.com/rcalixte/awesome-wayland)  
2. virtual\_keyboard\_unstable\_v1 (used by wtype) does not work correctly \#10558 \- GitHub, accessed February 8, 2026, [https://github.com/ghostty-org/ghostty/discussions/10558](https://github.com/ghostty-org/ghostty/discussions/10558)  
3. \[Wayland\] Dead keys don't register correctly \#9112 \- GitHub, accessed February 8, 2026, [https://github.com/ghostty-org/ghostty/discussions/9112](https://github.com/ghostty-org/ghostty/discussions/9112)  
4. GTK regression: ibus 1.5.31 dead keys on Wayland broken · Issue \#5494 \- GitHub, accessed February 8, 2026, [https://github.com/ghostty-org/ghostty/issues/5494](https://github.com/ghostty-org/ghostty/issues/5494)  
5. Implemented & Planned Wayland protocols · Issue \#781 · Smithay ..., accessed February 8, 2026, [https://github.com/Smithay/smithay/issues/781](https://github.com/Smithay/smithay/issues/781)  
6. evdevil : Rust bindings to evdev and uinput \- Crates.io, accessed February 8, 2026, [https://crates.io/crates/evdevil/0.3.1](https://crates.io/crates/evdevil/0.3.1)  
7. evdev\_rs \- Rust, accessed February 8, 2026, [https://ndesh26.github.io/evdev-rs/evdev\_rs/index.html](https://ndesh26.github.io/evdev-rs/evdev_rs/index.html)  
8. Check that waynergy works in cosmic-comp · Issue \#450 \- GitHub, accessed February 8, 2026, [https://github.com/pop-os/cosmic-comp/issues/450](https://github.com/pop-os/cosmic-comp/issues/450)  
9. \[Feature Request\] Implement RDP Server · Issue \#584 · pop-os/cosmic-comp \- GitHub, accessed February 8, 2026, [https://github.com/pop-os/cosmic-comp/issues/584](https://github.com/pop-os/cosmic-comp/issues/584)  
10. Reis — Rust HW library // Lib.rs, accessed February 8, 2026, [https://lib.rs/crates/reis](https://lib.rs/crates/reis)

[image1]: <data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAABgAAAAZCAYAAAArK+5dAAABQ0lEQVR4Xu2UPS8EURSGX4IgvhoK/4GIRFAJCpsoiGhVtFpRSZQaUaj8CYVCISQKH79AqSEqBRpBhPfkXObuOyNmdhUb2Sd5krnnnb1z594zC9SpNVroC/0Qe2lnRt18o1324yJM0SstBubgE19oUIQDOq/FwDH8AbaIimiiD7RbA9JOX4NtkuVmDL7CLGbg2ZEGRVhD+iDVje+7K2AfP7/BOTyb1iAvDfD9t9ZTOuB1a+NWyXIzCl/hmQZkFp6daEAG6Dbdo/2SlbEOn2RHA7IFzzal3kyvw/UIfvk+DuGTLGqAZP+1/yfpU7hupO90KIkTrMef4ZP0SdYD33/L7C8jZoXeRmM7w7IFTiDdhl9dVMqom5chN1bpTTS+p8vRuGqW6F00fqQL0bhqxuGta9iB2xsOJvHfMEx36SnSTVDnv/MJLTZVTNHi028AAAAASUVORK5CYII=>

[image2]: <data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAABYAAAAZCAYAAAA14t7uAAABFUlEQVR4Xu2SMUsCYRyHfxomfQFJaHCIZlsCG9pb3UIH18jBxdHNjxANubmUuOhHCPoCLdHmlERBRSCN9Xv5v1fnrztO4RACH3g4+D/3vne8d8CaVbNBZ/RL3KMZ+hnRnAtzSCewBykHsM0eYQ9bigGt6dBzBdu4IfNEsvSFbmvwPME23tGQRBnx5xa0Bw2L0MTfj6P2fu5egmvEv/EY1uoaknBf+RnRG7v2DmsFaYnswxbeacDvb3avAfZbHuswTAu2+FIDacPahQbY0dzoMMwI8WcYtBMNpE87OgzI0w/Y4tJ8wiZ9hbVdaVU/v6Xn4XDkg+qoRMydU98D3mhOZqkw1EFanPprcW6aAl16Rrc0rPmHfAOdpUwtIR79JAAAAABJRU5ErkJggg==>

[image3]: <data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAEgAAAAYCAYAAABZY7uwAAACjUlEQVR4Xu2YTahNURiGX/mLlBkhJQkxoSgkMwMTKZFMrv8kkr+ibgYGSoZIJJ38DRCKlMRAiZGiZKgQoihKGfG951v73n3fs46z176bc8t+6q19vuecddbeZ/3dC9TU1PyHTLCs1OJQ4pfls2WkCmMa3P+UelVMtjyzDFORwFV4H/eoMM7B3REVKbCBK1oMrIf7eyoq4pRlgxYTeQPv41wVxku4W6YiBTawTYuBE3B/UEVFvMDgRs8MeP/eqjAmwt13xGdHYdgIvyjGc7hfpKIiVmghka3w/jWkTtbC3W0VKcy2vNJiYAr8C75aRoirgulaKMFFeB+5FCin4W6/ihQ4tdjIn3Kr791pLEFrW52yrvnJ4nxEaxuahX3vLsEF+FCMcR7+BftUVABH7g0tJjIT3j8uAwpHJx1H/3BxSXyyTNVi4B0q+AXacAnxXSeF7fD+nVFhbIS7sqO/yU54IzE2wd11FfBp8ESLEZaidbh3yprmJ4vBH5efmaUC/Vv/fKnzwd0M1w1LT79q5RraP6AG3O2WOuHUK3vwWmA5qcUSzIP3L7a9Z1s/D796hOB57lC43mV5lHMDGGX5gvgDYqOv4W5xrr7Kcj/UH1vO5lxR7sLPJ4PlALwf3MWULXB3RwX8vvhgCGfJ+5xrwhNlNpyz8EmTORHH/IA/UMLh/M0yOrxOYbnlqBYT4QjQ/jHZTV+OuN7gyAfLjnDdg7/wJxSnXNmF74FlvBb/MTzzZcvGZvhGVClctLNfa1JedGA1+ud+N+Gp+nC43mt5mHOVwEWRCy0fTtHzEc8hTy3jVHQBrk/ZLsajRtF7KMxxyzH4GWSsuHZwrnNRHQpwA+I98L8XDcuYAbampqami/wGJ323KDH1i6kAAAAASUVORK5CYII=>

[image4]: <data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAGgAAAAZCAYAAADdYmvFAAACvUlEQVR4Xu2YyctPYRTHv8iQJELCQmQeEzYWslMWhISNRKa8lGQICzYoZViQoWzMs4TE2h9gg2zIkGljyJrz/Z3z/H6cey+vd7j3vfd9PvVd/M73Pk/nuff3nGcAIpFIJBLpGHQVfRH9dJpm/vsUb455ZWcGkmP7LOommpTi/RD1qrUsACbwQdTdG8JYqP9N1MN5VYBju+6Dxhqo/8AbecMkVvugcRrqb/FGReDYRvug8RzqT/dG3jCJET5oPIP6oexViQmilz5oDEOj7HVxXq4wyac+aPCjMclPKDjJdmK96IQPGiuhY7/q4rmzAX8uhmm6Un/6/5iFZF//0sJay9a1bS4XkezDq6n+dEGcEy3yQYMek9zkjYrwRjTQBw16HPtUb+QNy9dQHzTeooMk2Q6ErXQa46Ae302hTEZ2kuOhHs9CHp6f5vlgydiI7LGvg3qXvZE3m5GdJBdQeqzTnmWixz6YQkvWkfm1lq1r2xyuQdukcQHqcX0ulBvITvI8spM8K9rrgyWCO1Jun7PG/g7qTfRGnvBWIFzzeHjdERbJmc57ZHHOoFPOKwthdr5yccKPQo/vhqU8wHMR/5gPRUtFO0UnkSyDfOYAdIPFvvie94nuixbYM5zp96CXAMcsVmc2kqXho3lTUjzqO/SjES6gX0U97XeZ2I3k2EIJ53bae9Rd83eJ+kBnF/sh/e2Zvvaba3rYcO2Abvu3iwaJ7ohWmXdGdFS0Fi0/wmTCK5+bPtgJGCwaA700DX/OudAPFg7xt6Hr+lbRYovxsM8PyXbsg7wwf7hom8XajFtoHN6G/G50Arhx4kwIsMQdEfWDVhheOKfBS9dw4RqukAaIlkNnXZtySLQfunno7byq8xp6NUa4Pj2BrjVLLDZKdBC6C1yBxsxiiTsu2iM6DF27L4lGmh+JRCKRSCTyd34BVkvkmOQJawAAAAAASUVORK5CYII=>