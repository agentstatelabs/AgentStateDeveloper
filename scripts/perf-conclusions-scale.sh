#!/usr/bin/env bash
#
# perf-conclusions-scale.sh — regression guard for the O(all-symbols) hang in
# `asd conclusions export` and `asd conclusions list`.
#
# Field history: on threadweaver-ios (97,354 symbols, ~130 conclusions) both
# commands drove their walk from the SYMBOL index — one git probe per symbol,
# ~97k probes to collect a few hundred entries. `conclusions export --quiet`
# ran >120s at 89–100% CPU with no output and stalled the pre-commit hook;
# `conclusions list` was >12s and climbing. The fix drives the walk from the
# ledger tree instead: O(symbols_with_conclusions), ~0.5s on the same DB.
#
# WHY THIS IS A SHELL/CI TEST AND NOT A UNIT TEST: the pathology only manifests
# on an ON-DISK store (each probe is a real B-tree lookup in a multi-million-row
# objects table) AND the fixed path is only fast because the real `asd index`
# pipeline populates the FTS symbol cache. An in-memory test engine has neither
# property — there, per-symbol probes are cheap and the fix looks no faster, so
# an in-memory timing assertion would give FALSE confidence. Only the real
# binary over a real on-disk index reproduces the field conditions.
#
# What it does: generate a synthetic repo with many symbols but only a handful
# of conclusions, index it with the real binary, then assert export + list each
# finish well under a wall-clock budget. The fixed code finishes in well under
# a second regardless of symbol count; if the per-symbol walk is reintroduced,
# the budget trips hard.
#
# Calibration (measured on the fix branch, laptop SSD, synthetic repo):
#   40,000 symbols / 3 conclusions →  fixed code: <1s   pre-fix code: ~36s
# A 15s budget sits ~3x below the fixed-code time and ~2x above it on a CI
# runner 4x slower than the calibration box, while the pre-fix O(all-symbols)
# walk (≥36s, scaling with symbol count) blows past it and is killed. Raise
# ASD_SCALE_SYMBOLS if a future runner is fast enough to shrink the margin.
#
# Env knobs (with CI-friendly defaults):
#   ASD_BIN                 asd binary to test          (default: asd on PATH)
#   ASD_SCALE_SYMBOLS       synthetic symbol count      (default: 40000)
#   ASD_SCALE_BUDGET_SECS   per-command wall budget     (default: 15)
set -euo pipefail

ASD="${ASD_BIN:-asd}"
SYMS="${ASD_SCALE_SYMBOLS:-40000}"
BUDGET="${ASD_SCALE_BUDGET_SECS:-15}"
PER_FILE=200

command -v "$ASD" >/dev/null 2>&1 || { echo "FATAL: asd binary not found: $ASD" >&2; exit 2; }

# `timeout` (GNU coreutils) hard-kills a true hang. Present in CI (bookworm);
# on macOS it's `gtimeout` if coreutils is installed, else we fall back to
# plain elapsed-vs-budget (no kill) so the script still validates locally.
if command -v timeout >/dev/null 2>&1; then TIMEOUT=timeout
elif command -v gtimeout >/dev/null 2>&1; then TIMEOUT=gtimeout
else TIMEOUT=""; fi

WORK="$(mktemp -d)"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

echo "== perf-conclusions-scale: generating ~${SYMS} symbols (${PER_FILE}/file) =="
mkdir -p "$WORK/src"
files=$(( (SYMS + PER_FILE - 1) / PER_FILE ))
for f in $(seq 1 "$files"); do
  # One awk invocation per file emits PER_FILE trivial functions — each a
  # symbol. Fast even at tens of thousands of functions.
  awk -v f="$f" -v n="$PER_FILE" 'BEGIN{
    for (i=1; i<=n; i++) { printf "def fn_%d_%d(x):\n    return x + %d\n\n", f, i, i }
  }' > "$WORK/src/mod_$f.py"
done

DB="$WORK/.asd-state.db"
echo "== indexing (real pipeline, on-disk SQLite) =="
"$ASD" index "$WORK" --db "$DB" >/dev/null

# Pull a few real qnames from the index to attach conclusions to. Format is
# adapter-defined, so we read them back rather than guess.
# (portable to bash 3.2 — no mapfile)
QNAMES=()
while IFS= read -r line; do
  [ -n "$line" ] && QNAMES+=("$line")
done < <("$ASD" list symbols --db "$DB" 2>/dev/null \
  | grep -o '"qname"[[:space:]]*:[[:space:]]*"[^"]*"' \
  | sed 's/.*"qname"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/' \
  | head -3)
if [ "${#QNAMES[@]}" -eq 0 ]; then
  echo "FATAL: no qnames returned from 'asd list symbols' — index empty?" >&2
  exit 2
fi

echo "== seeding ${#QNAMES[@]} conclusions =="
for qn in "${QNAMES[@]}"; do
  "$ASD" ledger append "$qn" --kind decision \
    --summary "scale-test decision on $qn" --db "$DB" >/dev/null
done

# Time a command, enforce the budget. `timeout` kills a true hang so a
# regression fails fast instead of stalling the whole pipeline.
run_budgeted() {
  local label="$1"; shift
  local start end elapsed
  start=$(date +%s)
  if [ -n "$TIMEOUT" ]; then
    if ! "$TIMEOUT" "$BUDGET" "$@" >/dev/null 2>&1; then
      echo "FAIL: '$label' exceeded ${BUDGET}s budget (killed) — O(all-symbols) regression?" >&2
      exit 1
    fi
  else
    "$@" >/dev/null 2>&1
  fi
  end=$(date +%s)
  elapsed=$(( end - start ))
  if [ "$elapsed" -gt "$BUDGET" ]; then
    echo "FAIL: '$label' took ${elapsed}s > ${BUDGET}s budget — O(all-symbols) regression?" >&2
    exit 1
  fi
  echo "  ok: $label finished in ${elapsed}s (budget ${BUDGET}s)"
}

echo "== timing conclusions export + list under ${BUDGET}s budget =="
OUT="$WORK/out"
run_budgeted "conclusions export" "$ASD" conclusions export --db "$DB" --out "$OUT" --quiet
run_budgeted "conclusions list"   "$ASD" conclusions list   --db "$DB"

# Sanity: export must have written exactly the conclusions we seeded — guards
# against a "fast because it returned nothing" false pass.
seeded="${#QNAMES[@]}"
got=$("$ASD" conclusions export --db "$DB" --out "$OUT" --quiet 2>/dev/null \
  | grep -o '[0-9]\+ entries' | grep -o '[0-9]\+' | head -1)
if [ "${got:-0}" -ne "$seeded" ]; then
  echo "FAIL: expected $seeded exported entries, got ${got:-0}" >&2
  exit 1
fi
echo "  ok: exported exactly $seeded entries (matches seeded)"

echo "== PASS: conclusions export/list stay O(conclusions) at ${SYMS} symbols =="
