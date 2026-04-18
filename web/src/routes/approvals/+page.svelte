<script lang="ts">
	import { getAwaitingApproval } from '$lib/api';
	import type { LedgerEntry } from '$lib/types';
	import { symbols, approvals } from '$lib/stores';

	let entries = $state<LedgerEntry[]>([]);
	let err = $state<string | null>(null);
	let loading = $state(true);

	$effect(() => {
		loading = true;
		err = null;
		getAwaitingApproval()
			.then((list) => {
				entries = list;
				approvals.set(list.length);
				loading = false;
			})
			.catch((e) => {
				err = String(e);
				loading = false;
			});
	});

	function ts(s: string): string {
		try {
			return new Date(s).toISOString().replace('T', ' ').replace(/\..*$/, 'Z');
		} catch {
			return s;
		}
	}

	function approversFor(tags: string[] | undefined): string[] {
		if (!tags) return [];
		return tags.filter((t) => t.startsWith('approver:')).map((t) => t.slice('approver:'.length));
	}
</script>

<header class="q-header">
	<h1>Awaiting approval</h1>
	<p class="muted">
		Ledger entries gated by a policy that requires human or senior-agent attestation before they land.
	</p>
</header>

{#if loading}
	<div class="muted">loading…</div>
{:else if err}
	<div class="error">{err}</div>
{:else if entries.length === 0}
	<div class="muted empty">No entries awaiting approval.</div>
{:else}
	<ul class="queue">
		{#each entries as le (le.entry_id)}
			{@const qname = symbols.qnameOf(le.symbol_id)}
			{@const approvers = approversFor(le.tags)}
			<li>
				<div class="row-head">
					<span class="le-kind kind-{le.kind}">{le.kind}</span>
					<span class="summary">{le.summary}</span>
				</div>
				<div class="row-meta">
					{#if qname}
						<a class="qname" href="/symbols/{encodeURIComponent(qname)}">{qname}</a>
					{:else}
						<code class="sid">{le.symbol_id}</code>
					{/if}
					<span class="sep">·</span>
					<span>{le.author.kind}:{le.author.id}</span>
					<span class="sep">·</span>
					<span>{ts(le.created_at)}</span>
				</div>
				{#if le.matched_policy}
					<div class="row-policy">
						<span class="policy-label">policy</span>
						<code>{le.matched_policy}</code>
					</div>
				{/if}
				{#if approvers.length > 0}
					<div class="row-approvers">approvers: {approvers.join(', ')}</div>
				{/if}
			</li>
		{/each}
	</ul>
{/if}

<style>
	.q-header {
		margin-bottom: 20px;
		border-bottom: 1px solid var(--border);
		padding-bottom: 12px;
	}
	h1 {
		margin: 0;
		font-size: 18px;
		font-weight: 600;
	}
	.muted {
		color: var(--fg-dim);
	}
	.q-header .muted {
		margin: 4px 0 0 0;
		font-size: 12px;
	}
	.error {
		color: var(--bad);
	}
	.empty {
		padding: 24px 0;
	}
	.queue {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.queue li {
		padding: 10px 14px;
		margin-bottom: 8px;
		background: var(--bg-alt);
		border: 1px solid var(--border);
		border-left: 3px solid #ebcb8b;
		border-radius: 4px;
	}
	.row-head {
		display: flex;
		gap: 10px;
		align-items: baseline;
	}
	.le-kind {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		padding: 2px 6px;
		border-radius: 3px;
		background: var(--bg);
	}
	.kind-hazard {
		background: rgba(224, 108, 117, 0.18);
		color: var(--bad);
	}
	.kind-decision {
		background: rgba(122, 162, 255, 0.18);
		color: var(--accent);
	}
	.kind-constraint,
	.kind-assumption {
		background: rgba(235, 203, 139, 0.18);
		color: #ebcb8b;
	}
	.kind-rationale,
	.kind-tradeoff {
		background: var(--bg);
		color: var(--fg-dim);
	}
	.summary {
		font-weight: 600;
	}
	.row-meta {
		color: var(--fg-dim);
		font-size: 11px;
		margin-top: 4px;
		display: flex;
		gap: 6px;
		align-items: baseline;
		flex-wrap: wrap;
	}
	.row-meta .qname {
		color: var(--accent);
		text-decoration: underline;
		text-decoration-style: dotted;
	}
	.row-meta .qname:hover {
		text-decoration-style: solid;
	}
	.row-meta .sep {
		opacity: 0.6;
	}
	.sid {
		font-family: "SF Mono", ui-monospace, monospace;
		font-size: 11px;
	}
	.row-policy {
		margin-top: 6px;
		display: inline-flex;
		align-items: baseline;
		gap: 6px;
		padding: 3px 8px;
		background: var(--bg);
		border: 1px solid var(--border);
		border-radius: 10px;
		font-size: 11px;
	}
	.row-policy code {
		background: transparent;
		padding: 0;
		color: var(--fg-dim);
	}
	.policy-label {
		color: var(--fg-dim);
		text-transform: uppercase;
		letter-spacing: 0.08em;
		font-size: 9px;
	}
	.row-approvers {
		margin-top: 4px;
		color: var(--fg-dim);
		font-size: 11px;
		font-style: italic;
	}
</style>
