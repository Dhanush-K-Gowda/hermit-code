#!/bin/sh
# hermit installer — macOS and Linux.
#
#   curl -fsSL https://raw.githubusercontent.com/Dhanush-K-Gowda/hermit-code/main/install.sh | sh
#
# Options (environment variables):
#   HERMIT_VERSION=v0.1.0       install a specific release (default: latest)
#   HERMIT_INSTALL_DIR=/path    where to put the binary (default: ~/.local/bin)
#   HERMIT_DOWNLOAD_BASE=url    download from a mirror (expects <base>/hermit-<target>.tar.gz)

set -eu

REPO="Dhanush-K-Gowda/hermit-code"
VERSION="${HERMIT_VERSION:-latest}"
INSTALL_DIR="${HERMIT_INSTALL_DIR:-$HOME/.local/bin}"

if [ -t 1 ]; then
    BOLD="$(printf '\033[1m')"; DIM="$(printf '\033[2m')"; RED="$(printf '\033[31m')"
    GREEN="$(printf '\033[32m')"; CORAL="$(printf '\033[38;5;209m')"; RESET="$(printf '\033[0m')"
else
    BOLD=""; DIM=""; RED=""; GREEN=""; CORAL=""; RESET=""
fi

say() { printf '%s\n' "$*"; }
step() { printf '%s●%s %s\n' "$CORAL" "$RESET" "$*"; }
fail() { printf '%s✗ %s%s\n' "$RED" "$*" "$RESET" >&2; exit 1; }

# ── platform ────────────────────────────────────────────────────────────────

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
    Darwin) os_part="apple-darwin" ;;
    Linux) os_part="unknown-linux-musl" ;;
    *) fail "hermit supports macOS and Linux only (found $os)." ;;
esac
case "$arch" in
    x86_64 | amd64) arch_part="x86_64" ;;
    arm64 | aarch64) arch_part="aarch64" ;;
    *) fail "unsupported CPU architecture: $arch" ;;
esac
# An x86_64 shell under Rosetta on Apple Silicon should still get the native build.
if [ "$os" = "Darwin" ] && [ "$arch_part" = "x86_64" ] && [ "$(sysctl -in sysctl.proc_translated 2>/dev/null || echo 0)" = "1" ]; then
    arch_part="aarch64"
fi
target="$arch_part-$os_part"
asset="hermit-$target.tar.gz"

if [ -n "${HERMIT_DOWNLOAD_BASE:-}" ]; then
    base="$HERMIT_DOWNLOAD_BASE"
elif [ "$VERSION" = "latest" ]; then
    base="https://github.com/$REPO/releases/latest/download"
else
    base="https://github.com/$REPO/releases/download/$VERSION"
fi

# ── tools ───────────────────────────────────────────────────────────────────

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q -O "$2" "$1"; }
else
    fail "curl or wget is required."
fi
if command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
    sha256() { echo ""; }
fi

tmp="$(mktemp -d 2>/dev/null || mktemp -d -t hermit)"
trap 'rm -rf "$tmp"' EXIT INT TERM

# ── download + verify ───────────────────────────────────────────────────────

say ""
step "Downloading hermit ${DIM}($VERSION, $target)${RESET}"
fetch "$base/$asset" "$tmp/$asset" || fail "download failed: $base/$asset
  Is there a release for $target? See https://github.com/$REPO/releases"

if fetch "$base/$asset.sha256" "$tmp/$asset.sha256" 2>/dev/null; then
    expected="$(cut -d ' ' -f 1 < "$tmp/$asset.sha256")"
    actual="$(sha256 "$tmp/$asset")"
    if [ -z "$actual" ]; then
        say "  ${DIM}(no sha256 tool found; skipping checksum)${RESET}"
    elif [ "$expected" != "$actual" ]; then
        fail "checksum mismatch for $asset (expected $expected, got $actual)"
    else
        say "  ${DIM}checksum ok${RESET}"
    fi
else
    say "  ${DIM}(no checksum published; skipping verification)${RESET}"
fi

tar -xzf "$tmp/$asset" -C "$tmp" || fail "could not unpack $asset"
[ -f "$tmp/hermit" ] || fail "archive does not contain a hermit binary"

# ── install ─────────────────────────────────────────────────────────────────

step "Installing to ${BOLD}$INSTALL_DIR${RESET}"
mkdir -p "$INSTALL_DIR" 2>/dev/null || fail "cannot create $INSTALL_DIR (try HERMIT_INSTALL_DIR=... or run with sudo)"
[ -w "$INSTALL_DIR" ] || fail "$INSTALL_DIR is not writable (try HERMIT_INSTALL_DIR=... or run with sudo)"
# Write to a temp name and rename, so a running hermit isn't overwritten mid-flight.
cp "$tmp/hermit" "$INSTALL_DIR/.hermit.new"
chmod 755 "$INSTALL_DIR/.hermit.new"
mv -f "$INSTALL_DIR/.hermit.new" "$INSTALL_DIR/hermit"
if [ "$os" = "Darwin" ]; then
    xattr -d com.apple.quarantine "$INSTALL_DIR/hermit" 2>/dev/null || true
fi

installed="$("$INSTALL_DIR/hermit" --version 2>/dev/null || true)"
[ -n "$installed" ] || fail "installed binary does not run on this system"
printf '%s✓ %s installed%s\n' "$GREEN" "$installed" "$RESET"

# ── PATH + next steps ───────────────────────────────────────────────────────

case ":$PATH:" in
    *":$INSTALL_DIR:"*) on_path=1 ;;
    *) on_path=0 ;;
esac
if [ "$on_path" = 0 ]; then
    case "${SHELL:-}" in
        */zsh) rc="$HOME/.zshrc" ;;
        */bash) rc="$HOME/.bashrc" ;;
        */fish) rc="" ;;
        *) rc="$HOME/.profile" ;;
    esac
    say ""
    say "${BOLD}$INSTALL_DIR is not on your PATH.${RESET} Add it with:"
    if [ -z "$rc" ]; then
        say "  fish_add_path $INSTALL_DIR"
    else
        say "  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> $rc && . $rc"
    fi
fi

say ""
say "Next:"
say "  1. Get a free API key at ${BOLD}https://openrouter.ai/keys${RESET}"
say "  2. export OPENROUTER_API_KEY=sk-or-..."
say "  3. cd your-project && ${CORAL}hermit${RESET}"
say ""
