# hermit

A small, fast coding agent written in Rust. It talks to any OpenAI-compatible model, defaulting to **free OpenRouter models**. It reads and edits files, runs shell commands, and **checks that the code works**: after it makes changes, it runs your build and tests, and if they fail it sends the failures back to the model until they pass.

- Single 4 MB binary, about 5 MB peak memory (on a single-threaded tokio runtime)
- Streams responses, retries with backoff, and falls back to another model when a free one is rate-limited
- Tool output is capped (head and tail are kept), and old context is compacted automatically

## Setup

```sh
curl https://sh.rustup.rs -sSf | sh       # if Rust isn't installed
cargo install --path .                    # or: cargo build --release
export OPENROUTER_API_KEY=sk-or-...       # free key: https://openrouter.ai/keys
```

## Usage

```sh
hermit                                   # interactive REPL in the current directory
hermit -p "make the failing tests pass"  # one-shot; exit 0 = done/verified, 2 = checks failing
hermit -C ../other-repo -m google/gemma-4-31b-it:free
hermit --yolo -p "..."                   # never ask before running commands
hermit --verify "make lint test" -p "..."
hermit --no-verify
```

REPL commands: `/model [id]`, `/models` (lists current free models that support tool calling), `/undo`, `/verify`, `/clear`, `/cost`, `/exit`. Ctrl-C cancels the running request or command.

## Tools the model gets

`read_file`, `write_file`, `edit_file` (exact, unique string replace), `list_dir`, `glob`, `grep` (both respect `.gitignore`), `run_command`.

- Every path is confined to the workspace, and `..` or symlink escapes are rejected.
- File edits apply immediately and are shown as diffs. `/undo` reverts the last turn.
- Shell commands ask y/n unless they're on the allowlist of read, build and test commands (`cargo test`, `git diff`, `npm test`, …), or you answer `a` (always) for the session.

## Verification

When a turn changed files, hermit runs a check command:

| Project file | Command |
|---|---|
| `Cargo.toml` | `cargo check --all-targets --quiet && cargo test --quiet` |
| `go.mod` | `go build ./... && go test ./...` |
| `package.json` | `npm run build` / `tsc --noEmit` and `npm test` (when defined) |
| `pyproject.toml`, `pytest.ini`, `setup.py`, `tests/` | `python3 -m pytest -q -x` |
| `Makefile` with a `test:` target | `make test` |

If the check fails, the output goes back to the model, up to `max_fix_attempts` times.

## Config

Settings are loaded in this order, each one overriding the previous: `~/.config/hermit/config.toml`, then `./.hermit.toml`, then env vars (`HERMIT_API_KEY` or `OPENROUTER_API_KEY`, `HERMIT_MODEL`, `HERMIT_BASE_URL`), then CLI flags.

```toml
model = "poolside/laguna-s-2.1:free"
fallback_models = ["nvidia/nemotron-3-super-120b-a12b:free", "google/gemma-4-31b-it:free"]
verify_cmd = "cargo test"          # override auto-detection
allow_commands = ["make test"]     # extra commands that don't need approval
max_steps = 40
max_fix_attempts = 3
context_budget_tokens = 60000
command_timeout_secs = 120

# Any OpenAI-compatible provider works, e.g. local Ollama:
# base_url = "http://localhost:11434/v1"
# model = "qwen3-coder"
```

If a `HERMIT.md`, `AGENTS.md` or `CLAUDE.md` file exists in the workspace root, it's added to the system prompt.

## Development

```sh
cargo test     # unit tests, plus an end-to-end test against a mock model server
cargo clippy --all-targets
```
