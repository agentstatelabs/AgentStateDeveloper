#!/bin/sh
# AgentStateDeveloper (ASD) uninstaller
#
# Removes asd / asd-mcp / asd-serve binaries from ~/.local/bin and
# ~/.cargo/bin, and best-effort removes the MCP registration from popular
# AI tool config files.
#
# Does NOT touch:
#   - ~/.config/asd/repos.toml (your repo registry — keep your settings)
#   - per-repo .asd-state.db / .asd/ sidecar — these are project artifacts
#
# Re-install:
#   curl -fsSL https://raw.githubusercontent.com/agentstatelabs/asd/main/install.sh | sh
set -e

BOLD=''
DIM=''
RED=''
GREEN=''
RESET=''
if [ -t 1 ]; then
    BOLD=$(printf '\033[1m')
    DIM=$(printf '\033[2m')
    RED=$(printf '\033[31m')
    GREEN=$(printf '\033[32m')
    RESET=$(printf '\033[0m')
fi

ok()   { printf "  ${GREEN}✓${RESET} %s\n" "$1"; }
skip() { printf "  ${DIM}–${RESET} %s\n" "$1"; }
warn() { printf "  ${RED}!${RESET} %s\n" "$1"; }

BINS="asd asd-mcp asd-serve"
INSTALL_DIRS="${HOME}/.local/bin ${HOME}/.cargo/bin"

printf "\n${BOLD}AgentStateDeveloper uninstaller${RESET}\n\n"

# ── Remove binaries ───────────────────────────────────────────────────────
printf "Removing binaries...\n"
for DIR in $INSTALL_DIRS; do
    for BIN in $BINS; do
        if [ -f "${DIR}/${BIN}" ]; then
            rm -f "${DIR}/${BIN}" && ok "${DIR}/${BIN}"
        fi
    done
done

# ── Remove MCP registration from common AI tool configs ─────────────────
printf "\nRemoving MCP entries (best-effort)...\n"

# Remove the "asd" key from mcpServers in a JSON file, in-place.
# Requires python3 (default on macOS 12+, most Linux distros).
remove_from_json() {
    FILE="$1"
    [ -f "$FILE" ] || { skip "not found: $FILE"; return; }

    python3 - "$FILE" <<'PYEOF'
import json, sys
path = sys.argv[1]
try:
    with open(path) as f:
        data = json.load(f)
except Exception as e:
    print(f"  ! could not parse {path}: {e}")
    sys.exit(0)

changed = False
for key in ("mcpServers", "servers"):
    if isinstance(data, dict) and key in data and isinstance(data[key], dict) and "asd" in data[key]:
        del data[key]["asd"]
        changed = True

if changed:
    with open(path, "w") as f:
        json.dump(data, f, indent=2)
    print(f"  ✓ removed asd entry from {path}")
else:
    print(f"  – no asd entry in {path}")
PYEOF
}

# Claude Code (project-wide)
remove_from_json "${HOME}/.claude.json"
remove_from_json "${HOME}/.claude/config.json"

# Codex / OpenCode
remove_from_json "${HOME}/.codex/config.json"

# Cursor (workspace-specific configs are not touched — too many locations)
remove_from_json "${HOME}/.cursor/mcp.json"

# ── Leftovers note ────────────────────────────────────────────────────────
printf "\n${BOLD}Done.${RESET}\n\n"
printf "Kept (intentionally):\n"
printf "  ${DIM}~/.config/asd/repos.toml${RESET}   (your repo registry)\n"
printf "  ${DIM}./.asd-state.db${RESET}            (per-project indexed state)\n"
printf "  ${DIM}./.asd/${RESET}                    (per-project sidecar)\n"
printf "\nIf you really want all traces gone:\n"
printf "  ${DIM}rm -rf ~/.config/asd${RESET}\n"
printf "  ${DIM}find . -name .asd-state.db -delete${RESET}\n"
printf "  ${DIM}find . -name .asd -type d -exec rm -rf {} +${RESET}\n\n"
