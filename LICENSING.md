# Licensing & Editions

AgentStateDeveloper (ASD) is **open source, commercially supported**. The
code in this repository is licensed under the **Business Source License 1.1
(BSL 1.1)** and converts to **Apache 2.0** four years after each release.
[LICENSE](LICENSE) is the authoritative text; this document is the
plain-English summary plus how the **OSS / Team / Enterprise** editions fit
together.

ASD is one half of a suite. Its shared team layer is
**[CTXone](https://github.com/ctxone/ctxone)** — see
[Editions](#editions) below and the READMEs for how they pair.

## The license in plain English

### You CAN (no commercial license needed)

- Use ASD in production for your own applications and services.
- Use it inside your company, startup, or enterprise for internal
  operations — employees, contractors, and subsidiaries all count as
  internal.
- Self-host it on your own infrastructure.
- Modify the source and build products on top of it.
- Use the CLI, MCP server, HTTP server, and Lens UI as part of your own
  business operations.
- Use it for research, education, testing, and development.

### You CANNOT (without a commercial license)

- **Offer ASD itself as a competing commercial managed service** — the
  "ASD-as-a-Service where the product *is* ASD" pattern.
- **Embed, bundle, or redistribute ASD as part of a product you sell,
  license, or distribute to third parties.**

If you run ASD internally — your developers use it, your CI calls it, your
agents connect to it — you're fully in the clear. That's internal business
use, which is permitted without any arrangement.

### Automatic conversion to Apache 2.0

Every release of ASD converts to the **Apache License 2.0** four years
after its release date. After conversion, all BSL restrictions lift for
that version — the four-year clock keeps the ecosystem protected while
guaranteeing long-term openness.

## Editions

ASD ships in three editions. The **OSS** edition — this repository — is
fully functional on its own for an individual developer. **Team** and
**Enterprise** are commercially supported and add collaboration and
governance on top; they do not take anything away from OSS.

| Edition | Who it's for | What it adds on top |
|---|---|---|
| **OSS** | Individual developers | Everything in this repo, self-hosted, no account: the semantic index (9 languages), decision ledger, effect declarations, call graph, impact analysis, invariants, cross-service edges **within one repo**, `prepare-change` / `architecture` / `dead-code`, the git-native sidecar, and the full agent onboarding (`asd skill` / `asd bootstrap` / MCP). |
| **Team** | Teams sharing a codebase | The **cross-repo** layer: manifest import + cross-repo impact, portfolio architecture across services, cross-repo dead-endpoint detection, and team-shared runtime confidence. The shared brain is **[CTXone](https://github.com/ctxone/ctxone)** — decisions, plans, and memory that travel across the whole team. |
| **Enterprise** | Organizations | Org-scale governance: a Postgres-backed endpoint registry, change-governance gates (a contract change with N downstream consumers requires approval), audit / SIEM export with a verifiable hash chain, org-wide savings + adoption dashboards, and policy-governed, RBAC-scoped agent rollout. |

The boundaries are deliberate:

- **OSS is single-repo and self-contained.** No network, no account, no
  server required — everything lives beside your code and in git.
- **Team adds the cross-repo / federation layer** and pairs ASD with
  CTXone as the shared team memory.
- **Enterprise adds org-scale governance, audit, and access control.**

## Commercially supported

ASD is built and supported commercially by **AgentStateLabs**. Team and
Enterprise editions — along with support, onboarding help, and roadmap
input — are available. The commercial offering is still being shaped, so
the best path today is to reach out and we'll work out what fits your team.

**Contact:** [licensing@agentstatelabs.com](mailto:licensing@agentstatelabs.com)

## Questions

If you're unsure whether your use is covered by the BSL 1.1 grant, email
**licensing@agentstatelabs.com** and we'll clarify. The bar is
straightforward: **internal use is free; redistribution or hosted resale
requires a commercial license.**
