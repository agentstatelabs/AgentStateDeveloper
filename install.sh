#!/bin/sh
# AgentStateDeveloper (ASD) installer
#
# Downloads the latest `asd`, `asd-mcp`, and `asd-serve` binaries for your
# platform and drops them in $INSTALL_DIR (default: ~/.local/bin).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/agentstatelabs/asd/main/install.sh | sh
#   # or pipe with a specific version / install dir:
#   ASD_VERSION=v1.1.13 INSTALL_DIR=/usr/local/bin sh install.sh
#
# Environment:
#   ASD_VERSION         — release tag to install (default: latest)
#   INSTALL_DIR         — target directory (default: ~/.local/bin)
#   ASD_RELEASE_BASE    — base URL for release artifacts. Defaults to the
#                         public GitHub mirror; override when fetching from
#                         the GitLab origin or a private CDN.
#
# Plan N t-001 (1.1.13): frictionless distribution. CTXone parity.
set -e

# ─── Configuration ──────────────────────────────────────────────────────────
GITHUB_REPO="${ASD_GITHUB_REPO:-agentstatelabs/asd}"
ASD_RELEASE_BASE="${ASD_RELEASE_BASE:-https://github.com/${GITHUB_REPO}}"
INSTALL_DIR="${INSTALL_DIR:-${HOME}/.local/bin}"
BINS="asd asd-mcp asd-serve"

# ─── Pretty output ──────────────────────────────────────────────────────────
BOLD=''
DIM=''
GREEN=''
YELLOW=''
CYAN=''
RESET=''
if [ -t 1 ]; then
    BOLD=$(printf '\033[1m')
    DIM=$(printf '\033[2m')
    GREEN=$(printf '\033[32m')
    YELLOW=$(printf '\033[33m')
    CYAN=$(printf '\033[36m')
    RESET=$(printf '\033[0m')
fi

say()  { printf "%s\n" "$1"; }
ok()   { printf "  ${GREEN}✓${RESET} %s\n" "$1"; }
info() { printf "  ${DIM}–${RESET} %s\n" "$1"; }
warn() { printf "  ${YELLOW}!${RESET} %s\n" "$1"; }
die()  { printf "  ${YELLOW}✗${RESET} %s\n" "$1" >&2; exit 1; }

# ─── Platform detection ─────────────────────────────────────────────────────
OS_RAW="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH_RAW="$(uname -m)"

case "$ARCH_RAW" in
    x86_64|amd64)   ARCH="x86_64" ;;
    aarch64|arm64)  ARCH="aarch64" ;;
    *) die "Unsupported architecture: $ARCH_RAW" ;;
esac

case "$OS_RAW" in
    linux)  TARGET="${ARCH}-unknown-linux-gnu" ;;
    darwin) TARGET="${ARCH}-apple-darwin" ;;
    *)      die "Unsupported OS: $OS_RAW (try install.ps1 on Windows)" ;;
esac

say ""
say "${BOLD}${CYAN}AgentStateDeveloper installer${RESET}"
say "  Target: ${TARGET}"
say "  Dir:    ${INSTALL_DIR}"
say ""

mkdir -p "$INSTALL_DIR"

# ─── Resolve release tag ────────────────────────────────────────────────────
if [ -n "$ASD_VERSION" ]; then
    TAG="$ASD_VERSION"
    info "Using pinned version: ${TAG}"
else
    info "Resolving latest release..."
    TAG=$(curl -fsSL "https://api.github.com/repos/${GITHUB_REPO}/releases/latest" 2>/dev/null \
        | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' \
        | head -1)
    if [ -z "$TAG" ]; then
        warn "Could not resolve latest release from ${GITHUB_REPO}."
        say ""
        say "Either no releases are published yet, or the repo URL needs"
        say "configuring. To build from source:"
        say ""
        say "  ${DIM}git clone https://github.com/${GITHUB_REPO}.git${RESET}"
        say "  ${DIM}cd asd && cargo install --path crates/agentstatedeveloper-cli${RESET}"
        say "  ${DIM}cargo install --path crates/agentstatedeveloper-mcp${RESET}"
        say ""
        die "Aborting install."
    fi
    info "Latest is ${TAG}"
fi

# ─── Download each binary ───────────────────────────────────────────────────
say ""
say "${BOLD}Downloading ${TAG}...${RESET}"
for BIN in $BINS; do
    URL="${ASD_RELEASE_BASE}/releases/download/${TAG}/${BIN}-${TARGET}"
    DEST="${INSTALL_DIR}/${BIN}"
    info "${BIN}"
    if ! curl -fsSL "$URL" -o "$DEST"; then
        warn "Failed to download ${BIN} from ${URL}"
        warn "(the release may not include this binary for ${TARGET})"
        rm -f "$DEST"
        die "Aborting install."
    fi
    chmod +x "$DEST"
    ok "${BIN}"
done

say ""
ok "Installed to ${INSTALL_DIR}"

# ─── PATH check ─────────────────────────────────────────────────────────────
case ":$PATH:" in
    *":${INSTALL_DIR}:"*)
        ;;
    *)
        say ""
        warn "${INSTALL_DIR} is not on your PATH. Add this to your shell rc:"
        say ""
        say "  ${DIM}export PATH=\"${INSTALL_DIR}:\$PATH\"${RESET}"
        ;;
esac

# ─── Smoke check ────────────────────────────────────────────────────────────
say ""
if "${INSTALL_DIR}/asd" --version >/dev/null 2>&1; then
    VERSION=$("${INSTALL_DIR}/asd" --version | head -1)
    ok "${VERSION}"
else
    warn "asd installed but --version probe failed. Try running ${INSTALL_DIR}/asd --version manually."
fi

# ─── Next steps ─────────────────────────────────────────────────────────────
say ""
say "${BOLD}${CYAN}Get started:${RESET}"
say "  ${DIM}# Index a repo${RESET}"
say "  asd index ."
say ""
say "  ${DIM}# Register with your agent's MCP config${RESET}"
say "  asd install claude    ${DIM}# or codex, cursor, gemini${RESET}"
say ""
say "  ${DIM}# First context query${RESET}"
say "  asd prepare-change \"<describe your change>\""
say ""
say "Docs: https://github.com/${GITHUB_REPO}#readme"
say ""
