<script lang="ts">
	import { getAudit } from '$lib/api';
	import type { AuditEvent } from '$lib/types';
	import { symbols } from '$lib/stores.svelte';

	// "Who approved what, when" — the approval family of audit events:
	// the moment an entry entered the queue (ledger.append gated to
	// awaiting-approval) and every resolution (approve / reject / withdraw).
	// Server-side event_type filtering is substring-only, so we pull the
	// `ledger.` family once and slice the approval events client-side.
	const RESOLUTIONS = new Set(['ledger.approve', 'ledger.reject', 'ledger.withdraw']);

	let events = $state<AuditEvent[]>([]);
	let configured = $state(true);
	let err = $state<string | null>(null);
	let loading = $state(true);

	$effect(() => {
		loading = true;
		err = null;
		getAudit({ eventType: 'ledger.', limit: 1000 })
			.then((r) => {
				configured = r.configured;
				events = r.events.filter(
					(e) =>
						RESOLUTIONS.has(e.event_type) ||
						(e.event_type === 'ledger.append' && e.outcome === 'awaiting-approval')
				);
				loading = false;
			})
			.catch((e) => {
				err = e instanceof Error ? e.message : String(e);
				loading = false;
			});
	});

	// Group by UTC day, newest day first, newest event first within a day.
	// (The audit JSONL is append-ordered oldest-first.)
	const days = $derived.by(() => {
		const byDay = new Map<string, AuditEvent[]>();
		for (const e of [...events].reverse()) {
			const day = e.timestamp.slice(0, 10);
			const bucket = byDay.get(day);
			if (bucket) bucket.push(e);
			else byDay.set(day, [e]);
		}
		return [...byDay.entries()];
	});

	function dayLabel(day: string): string {
		const today = new Date().toISOString().slice(0, 10);
		const yesterday = new Date(Date.now() - 86_400_000).toISOString().slice(0, 10);
		if (day === today) return `today · ${day}`;
		if (day === yesterday) return `yesterday · ${day}`;
		return day;
	}

	function timeOf(ts: string): string {
		try {
			return new Date(ts).toISOString().slice(11, 19) + 'Z';
		} catch {
			return ts;
		}
	}

	/** Past-tense verb for the row: alice APPROVED entry …. */
	function verb(e: AuditEvent): string {
		if (e.event_type === 'ledger.append') return 'proposed';
		if (e.outcome === 'error') return 'failed to ' + e.event_type.slice('ledger.'.length);
		switch (e.event_type) {
			case 'ledger.approve':
				return 'approved';
			case 'ledger.reject':
				return 'rejected';
			case 'ledger.withdraw':
				return 'withdrew';
			default:
				return e.event_type;
		}
	}

	function tone(e: AuditEvent): 'ok' | 'bad' | 'warn' | 'pending' {
		if (e.outcome === 'error' || e.event_type === 'ledger.reject') return 'bad';
		if (e.event_type === 'ledger.append') return 'pending';
		if (e.outcome.startsWith('already-') || e.event_type === 'ledger.withdraw') return 'warn';
		return 'ok';
	}

	function qnameOf(e: AuditEvent): string | null {
		return e.secondary_id ? symbols.qnameOf(e.secondary_id) : null;
	}
</script>

<svelte:head>
	<title>Approval history — ASD Lens</title>
</svelte:head>

<header class="h-header">
	<div class="title-row">
		<h1>Approval history</h1>
		<nav class="tabs" aria-label="approvals views">
			<a class="tab" href="/approvals">Queue</a>
			<a class="tab active" href="/approvals/history" aria-current="page">History</a>
		</nav>
	</div>
	<p class="muted">
		Who approved what, when — every proposal that entered the approval queue and how it was
		resolved, straight from the audit log. The raw event stream lives on
		<a href="/audit">Audit</a>.
	</p>
</header>

{#if loading}
	<div class="muted">loading…</div>
{:else if err}
	<div class="error">{err}</div>
{:else if !configured}
	<div class="banner">
		<strong>No audit log configured.</strong>
		<span>
			Start <code>asd-serve</code> with <code>ASD_AUDIT_LOG=/path/to/audit.jsonl</code> to begin
			capturing approval events.
		</span>
	</div>
{:else if events.length === 0}
	<div class="muted empty">No approval activity recorded yet.</div>
{:else}
	<div class="count-row">
		{events.length} approval event{events.length === 1 ? '' : 's'} · newest first
	</div>
	{#each days as [day, dayEvents] (day)}
		<section class="day">
			<h2 class="day-head">{dayLabel(day)}</h2>
			<ol class="tl">
				{#each dayEvents as e (e.event_id)}
					{@const qname = qnameOf(e)}
					<li class="tl-row" data-tone={tone(e)}>
						<span class="tl-dot" aria-hidden="true"></span>
						<div class="tl-body">
							<div class="tl-line">
								<span class="actor">{e.actor_kind}:{e.actor_id}</span>
								<span class="verb" data-tone={tone(e)}>{verb(e)}</span>
								{#if e.subject_id}
									<code class="entry" title="ledger entry id">{e.subject_id}</code>
								{/if}
								{#if qname}
									<span class="on">on</span>
									<a class="qname" href="/symbols/{encodeURIComponent(qname)}">{qname}</a>
								{:else if e.secondary_id}
									<span class="on">on</span>
									<code class="sid" title="symbol id (no longer indexed)">{e.secondary_id}</code>
								{/if}
								<span class="outcome" data-tone={tone(e)}>{e.outcome}</span>
								<time class="when" datetime={e.timestamp}>{timeOf(e.timestamp)}</time>
							</div>
							{#if e.matched_policy}
								<div class="tl-meta">policy <code>{e.matched_policy}</code></div>
							{/if}
							{#if e.reason}
								<div class="tl-reason">{e.reason}</div>
							{/if}
						</div>
					</li>
				{/each}
			</ol>
		</section>
	{/each}
{/if}

<style>
	.h-header {
		margin-bottom: 20px;
		border-bottom: 1px solid var(--border);
		padding-bottom: 12px;
	}
	h1 {
		margin: 0;
		font-size: 18px;
		font-weight: 600;
	}
	.title-row {
		display: flex;
		align-items: baseline;
		gap: 16px;
	}
	.tabs {
		display: flex;
		gap: 4px;
	}
	.tab {
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		font-weight: 600;
		padding: 3px 10px;
		border-radius: 999px;
		border: 1px solid transparent;
		color: var(--fg-dim);
	}
	.tab:hover {
		color: var(--accent);
	}
	.tab.active {
		color: var(--accent);
		border-color: rgba(122, 162, 255, 0.4);
		background: rgba(122, 162, 255, 0.08);
	}
	.muted {
		color: var(--fg-dim);
	}
	.h-header .muted {
		margin: 4px 0 0 0;
		font-size: 12px;
		max-width: 72ch;
	}
	.h-header .muted a {
		color: var(--accent);
	}
	.error {
		color: var(--bad);
	}
	.empty {
		padding: 24px 0;
	}
	.banner {
		padding: 8px 12px;
		background: rgba(235, 203, 139, 0.08);
		border: 1px solid rgba(235, 203, 139, 0.3);
		color: #ebcb8b;
		border-radius: 4px;
		font-size: 12px;
	}
	.banner strong {
		margin-right: 6px;
	}
	.count-row {
		color: var(--fg-dim);
		font-size: 11px;
		margin-bottom: 16px;
	}
	.day {
		margin-bottom: 20px;
	}
	.day-head {
		margin: 0 0 8px 0;
		font-size: 11px;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		color: var(--fg-dim);
	}
	.tl {
		list-style: none;
		margin: 0;
		padding: 0;
		border-left: 1px solid var(--border);
	}
	.tl-row {
		position: relative;
		padding: 6px 0 6px 18px;
	}
	.tl-dot {
		position: absolute;
		left: -4px;
		top: 13px;
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: var(--fg-dim);
	}
	.tl-row[data-tone='ok'] .tl-dot {
		background: var(--ok);
	}
	.tl-row[data-tone='bad'] .tl-dot {
		background: var(--bad);
	}
	.tl-row[data-tone='warn'] .tl-dot {
		background: #ebcb8b;
	}
	.tl-row[data-tone='pending'] .tl-dot {
		background: var(--accent);
	}
	.tl-line {
		display: flex;
		align-items: baseline;
		gap: 8px;
		flex-wrap: wrap;
		font-size: 12px;
	}
	.actor {
		font-weight: 600;
		color: var(--fg);
	}
	.verb {
		font-weight: 600;
	}
	.verb[data-tone='ok'] {
		color: var(--ok);
	}
	.verb[data-tone='bad'] {
		color: var(--bad);
	}
	.verb[data-tone='warn'] {
		color: #ebcb8b;
	}
	.verb[data-tone='pending'] {
		color: var(--accent);
	}
	.entry {
		font-size: 11px;
		background: var(--bg-alt);
		border: 1px solid var(--border);
	}
	.on {
		color: var(--fg-dim);
	}
	.qname {
		color: var(--accent);
		text-decoration: underline;
		text-decoration-style: dotted;
		overflow-wrap: anywhere;
	}
	.qname:hover {
		text-decoration-style: solid;
	}
	.sid {
		font-size: 11px;
		color: var(--fg-dim);
	}
	.outcome {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		padding: 1px 6px;
		border-radius: 3px;
	}
	.outcome[data-tone='ok'] {
		background: rgba(111, 207, 151, 0.15);
		color: var(--ok);
	}
	.outcome[data-tone='bad'] {
		background: rgba(224, 108, 117, 0.15);
		color: var(--bad);
	}
	.outcome[data-tone='warn'],
	.outcome[data-tone='pending'] {
		background: rgba(235, 203, 139, 0.15);
		color: #ebcb8b;
	}
	.outcome[data-tone='pending'] {
		background: rgba(122, 162, 255, 0.15);
		color: var(--accent);
	}
	.when {
		margin-left: auto;
		font-size: 11px;
		color: var(--fg-dim);
		font-family: 'SF Mono', ui-monospace, monospace;
		white-space: nowrap;
	}
	.tl-meta {
		margin-top: 2px;
		font-size: 11px;
		color: var(--fg-dim);
	}
	.tl-reason {
		margin-top: 2px;
		font-size: 11px;
		color: var(--bad);
		font-style: italic;
	}
</style>
