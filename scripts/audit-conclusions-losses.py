"""Walk a branch's history for commits that DROPPED conclusion record ids.

Companion to scripts/check-conclusions-drop.sh: that one guards the future,
this one asks whether the past already cost something. Run it after a scare,
or periodically.

    python3 scripts/audit-conclusions-losses.py

Result when this was first run over 363 commits of main (2026-09-01): exactly
one loss, `led_9a6e25e92e8b4caf8fc8319b1fbe3935` — a decision on
`agentstatedeveloper_mcp.lib.ApiError:237` — dropped by 4fc35cd (the
feat/lens-metrics-explorer merge) and restored two merges later by 9ac59a9,
whose branch predated the drop and carried the record back. Net loss zero, by
luck rather than design.

First-parent walk on purpose: it models "what landed on main", which is the
question. A record removed on a side branch and restored by the merge never
harmed anyone.
"""
import json, subprocess, sys, collections

def sh(*a):
    return subprocess.run(a, capture_output=True, text=True).stdout

def ids_at(ref):
    out = collections.defaultdict(set)
    files = sh("git","ls-tree","-r","--name-only",ref,"--",".asd/conclusions").split()
    for f in files:
        if not f.endswith(".jsonl"):
            continue
        cls = f.rsplit("/",1)[-1][:-6]
        for line in sh("git","show",f"{ref}:{f}").splitlines():
            line=line.strip()
            if not line: continue
            try: r=json.loads(line)
            except Exception: continue
            if "id" in r: out[cls].add(r["id"])
    return out

commits = sh("git","rev-list","--first-parent","origin/main").split()
print(f"walking {len(commits)} commits on main's first-parent line\n")

losses = []
prev_ids = None
prev_sha = None
# rev-list is newest-first; walk oldest-first so "parent -> child" reads forward.
for sha in reversed(commits):
    cur = ids_at(sha)
    if prev_ids is not None:
        for cls, pset in prev_ids.items():
            dropped = pset - cur.get(cls, set())
            if dropped:
                subj = sh("git","log","-1","--pretty=%s",sha).strip()
                losses.append((sha, subj, cls, sorted(dropped)))
    prev_ids, prev_sha = cur, sha

if not losses:
    print("NO LOSSES: no commit on main ever dropped a conclusion record id.")
    sys.exit(0)

total = sum(len(d) for *_ ,d in losses)
print(f"{len(losses)} commit(s) dropped {total} record(s):\n")
for sha, subj, cls, dropped in losses:
    print(f"  {sha[:8]}  {cls}: -{len(dropped)}  {subj[:64]}")
    for d in dropped[:5]:
        print(f"      {d}")
    if len(dropped) > 5:
        print(f"      … and {len(dropped)-5} more")
