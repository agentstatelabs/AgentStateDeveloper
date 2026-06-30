#!/usr/bin/env bash
#
# Populate a Tier-2 realism corpus: shallow-clone a few real third-party repos,
# one or more per ecosystem, into a target directory. The conformance crate's
# `realism_external_corpus` test then runs the full adapter pipeline over them
# and asserts only coarse invariants (no panics, non-trivial counts).
#
# We clone rather than vendor so no third-party code lands in this repo.
#
#   ./scripts/fetch-realism-corpus.sh [TARGET_DIR]
#   export ASD_REALISM_CORPUS="$(pwd)/.realism-corpus"
#   cargo test -p agentstatedeveloper-conformance -- --ignored --nocapture
#
# Determinism note: these clone each repo's DEFAULT BRANCH at --depth 1, which
# is reproducible enough for "does it panic / does it find symbols" but not
# byte-identical over time. For a frozen corpus, replace a `URL` line with a
# `URL@<sha>` entry and the script will check that commit out.

set -u

TARGET="${1:-$(pwd)/.realism-corpus}"
mkdir -p "$TARGET"

# One repo per line: "<language> <git-url>[@<sha>]". Edit freely — a failed
# clone is logged and skipped, never fatal, so the corpus is best-effort.
REPOS=(
  "python     https://github.com/pallets/flask.git"
  "python     https://github.com/psf/requests.git"
  "typescript https://github.com/colinhacks/zod.git"
  "go         https://github.com/go-chi/chi.git"
  "go         https://github.com/gin-gonic/gin.git"
  "java       https://github.com/spring-projects/spring-petclinic.git"
  "ruby       https://github.com/sinatra/sinatra.git"
  "csharp     https://github.com/ardalis/CleanArchitecture.git"
  "kotlin     https://github.com/ktorio/ktor.git"
  "swift      https://github.com/vapor/vapor.git"
  "rust       https://github.com/tokio-rs/axum.git"
)

ok=0
fail=0
for entry in "${REPOS[@]}"; do
  lang=$(echo "$entry" | awk '{print $1}')
  spec=$(echo "$entry" | awk '{print $2}')
  url="${spec%@*}"
  sha=""
  [ "$spec" != "$url" ] && sha="${spec##*@}"
  name=$(basename "$url" .git)
  dest="$TARGET/$lang-$name"

  if [ -d "$dest/.git" ]; then
    echo "= $lang/$name already present, skipping"
    ok=$((ok + 1))
    continue
  fi

  echo "+ cloning $lang/$name ..."
  if [ -n "$sha" ]; then
    if git clone --quiet --filter=blob:none "$url" "$dest" \
      && git -C "$dest" checkout --quiet "$sha"; then
      ok=$((ok + 1))
    else
      echo "! FAILED $url@$sha — skipping" >&2
      rm -rf "$dest"
      fail=$((fail + 1))
    fi
  else
    if git clone --quiet --depth 1 "$url" "$dest"; then
      ok=$((ok + 1))
    else
      echo "! FAILED $url — skipping" >&2
      rm -rf "$dest"
      fail=$((fail + 1))
    fi
  fi
done

echo ""
echo "realism corpus: $ok cloned/present, $fail failed → $TARGET"
echo ""
echo "Next:"
echo "  export ASD_REALISM_CORPUS=\"$TARGET\""
echo "  cargo test -p agentstatedeveloper-conformance -- --ignored --nocapture"
