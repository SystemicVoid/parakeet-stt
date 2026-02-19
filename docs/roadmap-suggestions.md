This is a classic "Buy vs. Build" trap, specifically the **"Model vs. Logic" trap**.

As your Strategic Architect, my ruling is absolute: **Do not fine-tune the model.**

Fine-tuning a 600M+ param ASR model for 50 vocab words is negative leverage. It turns you into a dataset curator and MLOps janitor. It degrades the model's general English capabilities, requires retraining for every new JS framework that drops, and wastes your RTX 5060 Ti cycles on training rather than inference.

We will replicate Monologue’s "Auto Dictionary" and "Auto Editing" using a **deterministic, zero-latency Rust harness** inside `parakeet-ptt`.

Here is the "World Class PM" Roadmap to Delight, filtered through the Forge architecture.

---

### The Roadmap: "Parakeet Fluent"

Monologue is delightful because it trusts *intent* over *audio*. We will build that trust layer.

#### Phase 1: The "Jargon Injector" (High Leverage / Low Effort)

**Goal:** Fix the "Cloud Code" -> "Claude Code" problem immediately.
**Mechanism:** A hot-swappable replacement layer in `parakeet-ptt` using the Aho-Corasick algorithm (O(n) complexity, effectively zero latency even with 10k words).

* **Why not Regex?** Regex is slow at scale. Aho-Corasick is designed for finding multiple patterns simultaneously in a single pass.
* **The Config:** A simple `jargon.toml` file.
```toml
[global]
"cloud code" = "Claude Code"
"next js" = "Next.js"
"convex" = "Convex"
"you genial" = "Eugenia"

```



#### Phase 2: Contextual Modes (The "Monologue" Magic)

**Goal:** "Modes" that adapt to the active window (e.g., VS Code vs. Discord).
**Mechanism:** `parakeet-ptt` already detects the active window (implied by your Linux/Wayland stack). We map window classes to replacement profiles.

* **VS Code Profile:** Aggressive technical replacements. "function" -> `fn`.
* **Browser Profile:** Standard capitalization.
* **Terminal Profile:** Lowercase everything, kebab-case preference.

#### Phase 3: "Smart Format" (The Delight Layer)

**Goal:** Handle the formatting automatically.
**Mechanism:** A lightweight logic layer that detects if you are dictating code or prose.

* *Input:* "git commit message fix the login bug"
* *Output:* `git commit -m "fix the login bug"` (Triggered by keyword "git commit")

---

### Implementation Strategy: The "Jargon Injector"

We are going to use **Agentic Orchestration** to build Phase 1 right now. We will use the `aho-corasick` crate in Rust.

#### Step 1: define the config structure

We need a file that `parakeet-ptt` watches. If you edit it, the rust binary should reload it instantly without restarting.

#### Step 2: Agentic Execution

Do not write this code manually. It is boilerplate-heavy. Use this master prompt with Claude Code to implement the harness in your Rust client.

**Copy/Paste this prompt into your terminal:**

```bash
claude -p "ACT AS: Senior Rust Engineer.

CONTEXT:
We are modifying 'parakeet-ptt' (Rust/Tokio).
We need a post-processing layer to replace misheard jargon (e.g., 'cloud code' -> 'Claude Code') with zero latency.

TASK:
1. Add 'aho-corasick' and 'notify' (for file watching) to Cargo.toml.
2. Create a new module 'src/text_processor.rs'.
3. Implement a 'TextProcessor' struct that:
   - Loads a 'jargon.toml' file from XDG_CONFIG_HOME/parakeet/jargon.toml.
   - Uses Aho-Corasick to replace keys with values.
   - Runs a background tokio task to watch the file for changes and hot-reload the Aho-Corasick automaton.
4. Integrate this into 'src/main.rs':
   - Initialize 'TextProcessor' on startup.
   - Pass the final transcript from the WebSocket through 'processor.process(text)' before injection.

CONSTRAINTS:
- Use 'anyhow' for error handling.
- Use 'tracing' for logs.
- The replacement must be case-insensitive matching but preserve the casing of the target value.
- Do NOT block the main thread.

GOAL:
I want to be able to edit 'jargon.toml', save it, and have the next dictation use the new words immediately."

```

### The "Fragility" Counter-Strategy

You mentioned the fragility of rules ("cloud code" isn't *always* "Claude Code").

**The Forge Principle: Optimize for the 95%.**
You are a developer. When you say "cloud code" to your computer, you mean the Anthropic CLI 99% of the time. If you actually want to talk about generic code in the cloud, you can correct it manually.

**Delight comes from the tool guessing right on your most frequent tasks, not from being mediocre at everything.**

**Recommended `jargon.toml` Starter Pack:**

```toml
# The "Stack"
"next jay ess" = "Next.js"
"next js" = "Next.js"
"type script" = "TypeScript"
"javascript" = "JavaScript"
"convex" = "Convex"
"rust lang" = "Rust"
"cargo build" = "cargo build" # force lowercase

# The "Team" & "Tools"
"cloud code" = "Claude Code"
"claude code" = "Claude Code" # force capitalization
"you genial" = "Eugenia"
"hugo" = "Hugo"

# The "Commands" (Aggressive)
"git status" = "git status"
"git push" = "git push"
"sudo apt" = "sudo apt"

```

Build this harness. It gives you the "Monologue" experience with 0ms latency impact and 100% privacy on your local machine.
