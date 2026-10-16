<p align="center">
  <img src="assets/banner.png" alt="hermit code — small, fast coding agent in Rust" width="100%">
</p>

<p align="center">
  <b>A tiny, fast coding agent for your terminal.</b><br>
  Edits your code, runs your commands, and checks that the code works. Runs on free models.
</p>

<p align="center">
  <a href="https://github.com/Dhanush-K-Gowda/hermit-code/actions/workflows/ci.yml"><img src="https://github.com/Dhanush-K-Gowda/hermit-code/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/Dhanush-K-Gowda/hermit-code/releases/latest"><img src="https://img.shields.io/github/v/release/Dhanush-K-Gowda/hermit-code?color=EC4899" alt="Release"></a>
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey" alt="macOS | Linux">
  <img src="https://img.shields.io/badge/binary-~4%20MB-FFA662" alt="~4 MB binary">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue" alt="MIT"></a>
</p>

---

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/Dhanush-K-Gowda/hermit-code/main/install.sh | sh
```

Then get a free key at [openrouter.ai/keys](https://openrouter.ai/keys) and run it in a project folder:

```sh
export OPENROUTER_API_KEY=sk-or-...
cd your-project && hermit
```

macOS (Apple Silicon and Intel) and Linux (x86_64 and ARM64, static binaries). To update, run the install command again. [More options](#install-options).

## What it looks like

```
❯ fix the failing test

● Grep fn add
  └ 1 match
● Edit src/lib.rs
  └ src/lib.rs +1 -1
    -pub fn add(a: i32, b: i32) -> i32 { a - b }
    +pub fn add(a: i32, b: i32) -> i32 { a + b }
● Run cargo test --quiet
  └ exit code 0
◆ Fixed `add`: it subtracted instead of adding. Tests pass.
● Verify cargo check --all-targets --quiet && cargo test --quiet
  ✓ checks passed
── 5 steps · 876 tokens · 8.9s
```

## Features

| | |
|---|---|
| ✅ **Checks that it works** | After every change it runs your build and tests, and sends failures back to the model until they pass |
| 🪶 **Tiny and fast** | One ~4 MB Rust binary, ~5 MB of memory, starts instantly. No Node, Python or TUI framework |
| 🆓 **Free models** | OpenRouter free models out of the box, with automatic fallback when one is rate-limited. Any OpenAI-compatible API works |
| 🧭 **Plan mode** | Read-only investigation that ends with a plan you approve before anything changes |
| 🔎 **Sub-agents** | The model hands broad codebase searches to a read-only `explore` helper, so your context stays small |
| 🖼️ **Images** | Paste a screenshot with Ctrl+V or drag in a file. It's sent to a model that can read images |
| 📋 **Todo list** | Multi-step work is planned and tracked live |
| 🧠 **Memory** | Remembers project facts in `HERMIT.md` across sessions |
| 💾 **Sessions** | Every conversation is saved. Leave and come back with `hermit --resume <id>` |
| 🗜️ **Smart context** | Long conversations are summarized by the model; recent turns stay word for word |
| 🔧 **Any model** | Models without native tool calling work through text-based tool calls |
| 🛡️ **Safe by default** | Stays inside the project folder, asks before unknown commands, `/undo` for edits |

## Usage

```sh
hermit                                  # interactive session
hermit -p "make the failing tests pass" # one-shot (exit 0 = done, 2 = checks failing)
hermit --plan -p "how would you add auth?"
hermit -c                               # continue the latest session here
hermit --resume <id>                    # resume a specific session, from any folder
hermit -m google/gemma-4-31b-it:free    # pick a model
hermit --yolo                           # don't ask before running commands
```

In the REPL, type `/` to see every command. Press → or Tab to accept a suggestion.

| | |
|---|---|
| `!git status` | Run a shell command yourself; the model sees the output |
| **Ctrl+V** | Paste an image from the clipboard |
| `/plan [task\|off]` | Plan mode (read-only, then you approve) |
| `/model [id]` · `/models [all]` | Switch model · list free models (`(images)` = can read images) |
| `/todo` · `/memory` · `/remember <fact>` | Task list · saved memory · add a note |
| `/sessions` · `/resume [n\|id]` | List · resume sessions |
| `/undo` · `/verify` · `/compact` | Revert last turn · run checks · summarize context |
| `/clear` · `/cost` · `/exit` | New conversation · token usage · quit (or Ctrl-C twice) |

## Config

Optional. Put it in `~/.config/hermit/config.toml` (global) or `.hermit.toml` (per project).

```toml
model = "poolside/laguna-s-2.1:free"
fallback_models = ["nvidia/nemotron-3-super-120b-a12b:free", "google/gemma-4-31b-it:free"]
vision_model = "google/gemma-4-31b-it:free"   # for images when the main model can't see
verify_cmd = "cargo test"                     # override auto-detection
allow_commands = ["make test"]                # run without asking

# Any OpenAI-compatible provider, e.g. local Ollama:
# base_url = "http://localhost:11434/v1"
# model = "qwen3-coder"
```

<details>
<summary><b>All settings</b></summary>

Load order: `~/.config/hermit/config.toml` → `./.hermit.toml` → env vars (`OPENROUTER_API_KEY` or `HERMIT_API_KEY`, `HERMIT_MODEL`, `HERMIT_BASE_URL`) → CLI flags. `$XDG_CONFIG_HOME` replaces `~/.config` when set.

```toml
max_steps = 40                 # model calls per request
max_fix_attempts = 3           # verify → fix rounds
context_budget_tokens = 60000  # summarize older turns above this
command_timeout_secs = 120
tool_mode = "auto"             # auto | native | text
temperature = 0.2
max_tokens = 4096
```

CLI flags: `-C <dir>`, `--verify <cmd>`, `--no-verify`, `--max-steps <n>`, `--text-tools`. See `hermit --help`.
</details>

## How it works

<details>
<summary><b>Verification</b>: the edit → check → fix loop</summary>

When a turn changes files, hermit runs the project's check command and hands any failures back to the model, up to `max_fix_attempts` times.

| Project | Check |
|---|---|
| `Cargo.toml` | `cargo check --all-targets && cargo test` |
| `go.mod` | `go build ./... && go test ./...` |
| `package.json` | `npm run build` / `tsc --noEmit`, `npm test` |
| `pyproject.toml`, `pytest.ini`, `tests/` | `python3 -m pytest -q -x` |
| `Makefile` with `test:` | `make test` |
</details>

<details>
<summary><b>Tools and safety</b></summary>

The model gets `read_file`, `write_file`, `edit_file`, `list_dir`, `glob`, `grep`, `run_command`, `todo_write`, `remember` and `explore`.

- Every path is confined to the workspace; `..` and symlink escapes are rejected.
- Edits apply immediately and show as diffs. `/undo` reverts the last turn (changes made by shell commands aren't tracked).
- Shell commands ask `y/n/always` unless they're on the read/build/test allowlist, or `--yolo` is set.
- Tool output is capped (first and last 8 KB kept), and search respects `.gitignore`.
</details>

<details>
<summary><b>Plan mode</b></summary>

`/plan` toggles it, `/plan <task>` starts right away, and `hermit --plan -p "…"` prints a plan and exits. In plan mode the model only gets read-only tools and safe commands; writes are refused even if it tries. It ends with a concrete plan, and then:

```
approve plan? [y] implement · [n] keep planning · or type feedback ›
```
</details>

<details>
<summary><b>Sub-agents</b></summary>

For broad questions ("where is auth handled?", "what would this change touch?") the model calls `explore` on its own. The sub-agent:
- runs in a fresh context with read-only tools only (no edits, todos, memory or more sub-agents)
- stops after 25 steps
- returns only its report, capped at 12 KB, to the main conversation

```
● Explore find token validation
  │ Grep validate_token
  │ Read src/auth.rs
  └ 3 steps · 0.1k chars report
```
</details>

<details>
<summary><b>Images</b></summary>

- **Attaching:** Ctrl+V pastes from the clipboard (`osascript` on macOS, `wl-paste`/`xclip` on Linux). Image paths you type or drag in are attached too. Each becomes `[Image #n]`. Formats: PNG, JPEG, GIF, WebP.
- **Size:** on macOS, large images are downscaled to 1568 px with the built-in `sips`. No image libraries are used.
- **Context:** only the 3 newest images are sent, and sessions store a note instead of the image data.
- **Routing:** if the current model can't read images, that turn goes to `vision_model`, then to a vision-capable fallback, then to a free vision model from the catalog. If none is available, only the text is sent.
</details>

<details>
<summary><b>Memory, sessions and context</b></summary>

- **Memory:** the model saves durable facts with `remember` to `./HERMIT.md`, or to `~/.config/hermit/HERMIT.md` for global preferences. These files, plus `AGENTS.md` or `CLAUDE.md`, are loaded into every session.
- **Sessions:** saved after every turn (latest 100). Exiting prints `hermit --resume <id>`, and resuming replays the whole conversation.
- **Compaction:** above the token budget, the model summarizes older turns (goal, changes, findings, next steps) and recent turns are kept verbatim. `/compact` does it on demand.
- **Text tools:** models that reject native tools automatically switch to `<tool_call>{json}</tool_call>` blocks, which are hidden while streaming.
</details>

<details id="install-options">
<summary><b>Install options</b></summary>

```sh
curl -fsSL …/install.sh | HERMIT_VERSION=v0.1.0 sh              # specific version
curl -fsSL …/install.sh | HERMIT_INSTALL_DIR=/usr/local/bin sh  # other location
cargo install --git https://github.com/Dhanush-K-Gowda/hermit-code  # from source
```

The script checks the SHA-256 checksum and installs to `~/.local/bin`. **Uninstall:** `rm ~/.local/bin/hermit`, plus `rm -rf ~/.config/hermit` to remove config, memory and sessions. Windows isn't supported.
</details>

## Development

```sh
cargo test                     # unit + end-to-end tests (mock model server)
cargo clippy --all-targets
git tag v0.1.1 && git push origin v0.1.1   # release: builds and publishes binaries
```

<details>
<summary><b>Project layout</b></summary>

| | |
|---|---|
| `src/agent.rs` | Agent loop, verify-and-fix, compaction, sub-agents, sessions |
| `src/llm/` | Streaming client, SSE parsing, text-based tool protocol |
| `src/tools/` | File, search, shell, todo and memory tools |
| `src/repl.rs` · `src/complete.rs` | REPL, slash commands, hints, plan approval |
| `src/ui.rs` · `src/markdown.rs` | Terminal output, streaming Markdown and tables |
| `src/image.rs` | Clipboard images, downscaling, data URLs |
| `src/context.rs` · `src/session.rs` · `src/safety.rs` | Context budget · sessions · workspace and command rules |

Releases: the `release` workflow builds macOS (arm64, x86_64) and static Linux (x86_64, arm64) binaries when a `v*` tag matching `Cargo.toml` is pushed. CI runs clippy and tests on macOS and Linux.
</details>

## License

[MIT](LICENSE)
