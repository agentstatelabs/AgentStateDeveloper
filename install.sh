#!/bin/sh
# AgentStateDeveloper (ASD) installer
#
# Downloads the platform-specific release tarball from the
# agentstatelabs/agentstatedeveloper-releases public mirror, extracts
# asd / asd-mcp / asd-serve, and drops them in $INSTALL_DIR
# (default: ~/.local/bin).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/agentstatelabs/AgentStateDeveloper/main/install.sh | sh
#
#   # pin a version + custom dir:
#   ASD_VERSION=v1.1.14 INSTALL_DIR=/usr/local/bin \
#     sh -c "$(curl -fsSL https://raw.githubusercontent.com/agentstatelabs/AgentStateDeveloper/main/install.sh)"
#
# Environment:
#   ASD_VERSION         — release tag to install (default: latest, e.g. "v1.1.14")
#   INSTALL_DIR         — target directory (default: ~/.local/bin)
#   ASD_RELEASES_REPO   — GitHub repo hosting release artifacts
#                         (default: agentstatelabs/agentstatedeveloper-releases)
#
# Plan N t-001 (1.1.14): frictionless distribution. CTXone parity.
set -e

# ─── Configuration ──────────────────────────────────────────────────────────
RELEASES_REPO="${ASD_RELEASES_REPO:-agentstatelabs/agentstatedeveloper-releases}"
SOURCE_REPO="agentstatelabs/AgentStateDeveloper"
INSTALL_DIR="${INSTALL_DIR:-${HOME}/.local/bin}"
BINS="asd asd-mcp asd-serve"

# ─── Pretty output ──────────────────────────────────────────────────────────
BOLD=''; DIM=''; GREEN=''; YELLOW=''; CYAN=''; RESET=''
if [ -t 1 ]; then
    BOLD=$(printf '\033[1m'); DIM=$(printf '\033[2m')
    GREEN=$(printf '\033[32m'); YELLOW=$(printf '\033[33m')
    CYAN=$(printf '\033[36m'); RESET=$(printf '\033[0m')
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
    info "Resolving latest release from ${RELEASES_REPO}..."
    TAG=$(curl -fsSL "https://api.github.com/repos/${RELEASES_REPO}/releases/latest" 2>/dev/null \
        | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' \
        | head -1)
    if [ -z "$TAG" ]; then
        warn "Could not resolve latest release from ${RELEASES_REPO}."
        say ""
        say "Build from source instead:"
        say ""
        say "  ${DIM}git clone https://github.com/${SOURCE_REPO}.git${RESET}"
        say "  ${DIM}cd AgentStateDeveloper${RESET}"
        say "  ${DIM}cargo install --path crates/agentstatedeveloper-cli${RESET}"
        say "  ${DIM}cargo install --path crates/agentstatedeveloper-mcp${RESET}"
        say ""
        die "Aborting install."
    fi
    info "Latest is ${TAG}"
fi

# ─── Download + extract the tarball ────────────────────────────────────────
TARBALL="asd-${TAG}-${TARGET}.tar.gz"
URL="https://github.com/${RELEASES_REPO}/releases/download/${TAG}/${TARBALL}"
TMP=$(mktemp -d -t asd-install.XXXXXX)
trap 'rm -rf "$TMP"' EXIT

say ""
say "${BOLD}Downloading ${TAG} (${TARGET})...${RESET}"
info "${URL}"
if ! curl -fsSL "$URL" -o "${TMP}/${TARBALL}"; then
    die "Download failed. Check that ${TARGET} is included in this release."
fi

info "Extracting..."
# Tarballs are structured as asd-<TAG>-<TARGET>/{asd,asd-mcp,asd-serve}
# Use --strip-components=1 to flatten the top-level directory.
if ! tar -xzf "${TMP}/${TARBALL}" -C "$TMP" --strip-components=1; then
    die "Extraction failed."
fi

for BIN in $BINS; do
    if [ ! -f "${TMP}/${BIN}" ]; then
        die "Tarball is missing ${BIN} — release artifact is malformed."
    fi
    install -m 0755 "${TMP}/${BIN}" "${INSTALL_DIR}/${BIN}"
    ok "${BIN}"
done

say ""
ok "Installed to ${INSTALL_DIR}"

# ─── PATH check ─────────────────────────────────────────────────────────────
case ":$PATH:" in
    *":${INSTALL_DIR}:"*) ;;
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
    VERSION_LINE=$("${INSTALL_DIR}/asd" --version | head -1)
    ok "${VERSION_LINE}"
else
    warn "asd installed but --version probe failed. Run ${INSTALL_DIR}/asd --version manually."
fi

# ─── Next steps ─────────────────────────────────────────────────────────────
say ""
say "${BOLD}${CYAN}Get started:${RESET}"
say "  ${DIM}# Index a repo${RESET}"
say "  asd index ."
say ""
say "  ${DIM}# Register with your agent's MCP config${RESET}"
say "  asd install claude    ${DIM}# or codex, cursor, gemini (Plan N t-005)${RESET}"
say ""
say "  ${DIM}# First context query${RESET}"
say "  asd prepare-change \"<describe your change>\""
say ""
say "Docs:     https://github.com/${SOURCE_REPO}#readme"
say "Releases: https://github.com/${RELEASES_REPO}/releases"
say ""
