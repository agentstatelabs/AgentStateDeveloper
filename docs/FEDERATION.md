# Federation — cross-repo context across many repos

ASD is single-repo by default: one `.asd-state.db` per project. **Federation**
lets you ask questions *across* repos — "if I change this endpoint, what breaks
in the other services, and what invariants do those callers hold?" — by joining
each repo's detected contracts on a shared key. Nothing is merged into one
database; each repo stays authoritative for itself.

> **Federation vs. isolation — two different needs.** Federation (this doc) is
> for repos that talk to each other and you want the *cross-repo* picture.
> Working on 2–3 *unrelated* projects in separate sessions is the opposite —
> see [Independent sessions](#independent-sessions) below.

## The model

- Each repo has its own `.asd-state.db` (built by `asd index`), which includes
  its detected cross-service **endpoints** (HTTP routes/clients, pub-sub),
  resolved to full contracts (see `asd endpoints`).
- A shared **registry** at `~/.config/asd/repos.toml` lists the repos in your
  federation set. `asd index` auto-registers; `asd repo add` registers
  explicitly.
- Federation commands read the registry, load each repo's endpoints, and match
  **outbound contract in repo A → inbound contract in repo B** by contract hash.
- The **CTXone hub** reads the *same* registry and exposes the same capability
  to agents over MCP.

## Setup — point ASD at multiple repos

```bash
# Build + auto-register each repo (run once per repo; re-run to refresh).
cd ~/projects/orders-api    && asd index .
cd ~/projects/billing       && asd index .
cd ~/projects/web-frontend  && asd index .

# Or register an already-indexed repo explicitly:
asd repo add orders-api ~/projects/orders-api/.asd-state.db

# See the federation set:
asd repo list
```

Each repo gets a distinct `repo_id` from its git root (override with
`ASD_REPO_ID`). Distinct repos in distinct directories get distinct ids
automatically — you only need `ASD_REPO_ID` if you index two repos from the
same working directory.

Point at a different registry file with `ASD_REGISTRY=/path/to/repos.toml`
(useful for a scratch/experimental federation set).

## Query — cross-repo edges and impact

```bash
# Every cross-repo edge: a client call in one repo matched to a route in another.
asd repo edges
asd repo edges --agent            # machine-readable JSON
asd repo edges --include-in-repo  # also show in-repo edges

# Decision-aware impact: what consumes an endpoint you're about to change,
# and what invariants/hazards do those callers carry (read from THEIR repo)?
asd repo impact get_orders                 # by route-handler qname
asd repo impact "http:GET /api/orders/{}"  # or by contract
asd repo impact get_orders --agent
```

`asd repo impact` opens each downstream consumer's own db and reports the
invariants and hazards recorded on the consuming symbol — so a change in one
repo surfaces the promises its cross-repo callers made, from their own ledger.

## Over MCP — the CTXone hub (Team)

The CTXone hub reads the same `~/.config/asd/repos.toml` (it auto-discovers the
registered repos) and exposes federation to agents as MCP tools:

- `code_cross_repo_edges` — the cross-repo edge set.
- `code_impact` — decision-aware federated impact for a qname or contract.

These require the `asd` CLI on the hub's PATH. This is the Team-tier surface:
one shared hub, the whole team's agents get cross-repo context.

## Coverage & freshness notes

- Contract detection currently covers **HTTP** (routes + clients, with nested /
  aliased / multi-mount router-prefix resolution) and **pub-sub** (Celery).
  More transports (gRPC, GraphQL, event schemas) are on the roadmap.
- Federation matches whatever is in each repo's **last index**. Re-run
  `asd index` (or `asd watch`) in a repo after changing its routes/clients so
  its contracts are current before relying on `asd repo edges/impact`.
- An outbound call with no matching inbound route is a **drift** signal — a
  caller reaching an endpoint no server serves at that path. That's a feature:
  the federated view surfaces real cross-service contract drift.

## Independent sessions

If instead you're working on several **unrelated** repos at once (separate
sessions, no cross-repo question), you don't want a shared active repo — you
want each session pinned to its own. On the CLI this is automatic: `asd` walks
up from the current directory to that project's `.asd-state.db`. For agents,
install the MCP server per-project so each session targets its own db. (See the
switch/isolation notes in the README's Agent setup section.)
