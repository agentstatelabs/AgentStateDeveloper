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

<header class="page-header">
	<div class="title-row">
		<h1>Approval history</h1>
		<nav class="tabs" aria-label="approvals views">
			<a class="tab" href="/approvals">Queue</a>
			<a class="tab active" href="/approvals/history" aria-current="page">History</a>
		</nav>
	</div>
	<p class="page-desc">
		Who approved what, when — every proposal that entered the approval queue and how it was
		resolved, straight from the audit log. The raw event stream lives on
		<a href="/audit">Audit</a>.
	</p>
</header>

{#if loading}
	<div class="state-loading">loading…</div>
{:else if err}
	<div class="state-error">{err}</div>
{:else if !configured}
	<div class="banner" data-tone="warn">
		<strong>No audit log configured.</strong>
		<span>
			Start <code>asd-serve</code> with <code>ASD_AUDIT_LOG=/path/to/audit.jsonl</code> to begin
			capturing approval events.
		</span>
	</div>
{:else if events.length === 0}
	<div class="state-empty">
		No approval activity recorded yet — resolutions appear here as approvers act on the queue.
	</div>
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
	.count-row {
		color: var(--lens-muted);
		font-size: var(--lens-font-size-2xs);
		margin-bottom: var(--lens-space-4);
	}
	.day {
		margin-bottom: var(--lens-space-5);
	}
	.day-head {
		margin: 0 0 var(--lens-space-2);
		font-size: var(--lens-font-size-2xs);
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: var(--lens-tracking-caps);
		color: var(--lens-muted);
	}
	.tl {
		list-style: none;
		margin: 0;
		padding: 0;
		border-left: 1px solid var(--lens-border);
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
		background: var(--lens-muted);
	}
	.tl-row[data-tone='ok'] .tl-dot {
		background: var(--lens-ok);
	}
	.tl-row[data-tone='bad'] .tl-dot {
		background: var(--lens-danger);
	}
	.tl-row[data-tone='warn'] .tl-dot {
		background: var(--lens-warn);
	}
	.tl-row[data-tone='pending'] .tl-dot {
		background: var(--lens-accent);
	}
	.tl-line {
		display: flex;
		align-items: baseline;
		gap: var(--lens-space-2);
		flex-wrap: wrap;
		font-size: var(--lens-font-size-xs);
	}
	.actor {
		font-weight: 600;
		color: var(--lens-text);
	}
	.verb {
		font-weight: 600;
	}
	.verb[data-tone='ok'] {
		color: var(--lens-ok);
	}
	.verb[data-tone='bad'] {
		color: var(--lens-danger);
	}
	.verb[data-tone='warn'] {
		color: var(--lens-warn);
	}
	.verb[data-tone='pending'] {
		color: var(--lens-accent);
	}
	.entry {
		font-size: var(--lens-font-size-2xs);
		background: var(--lens-surface);
		border: 1px solid var(--lens-border);
	}
	.on {
		color: var(--lens-muted);
	}
	.qname {
		font-family: var(--lens-font-mono);
		color: var(--lens-accent);
		text-decoration: underline;
		text-decoration-style: dotted;
		overflow-wrap: anywhere;
	}
	.qname:hover {
		color: var(--lens-accent-hover);
		text-decoration-style: solid;
	}
	.sid {
		font-size: var(--lens-font-size-2xs);
		color: var(--lens-muted);
	}
	.outcome {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: var(--lens-tracking-caps);
		padding: 1px 6px;
		border-radius: var(--lens-radius-sm);
	}
	.outcome[data-tone='ok'] {
		background: var(--lens-ok-tint);
		color: var(--lens-ok);
	}
	.outcome[data-tone='bad'] {
		background: var(--lens-danger-tint);
		color: var(--lens-danger);
	}
	.outcome[data-tone='warn'] {
		background: var(--lens-warn-tint);
		color: var(--lens-warn);
	}
	.outcome[data-tone='pending'] {
		background: var(--lens-accent-tint);
		color: var(--lens-accent);
	}
	.when {
		margin-left: auto;
		font-size: var(--lens-font-size-2xs);
		color: var(--lens-muted);
		font-family: var(--lens-font-mono);
		white-space: nowrap;
	}
	.tl-meta {
		margin-top: 2px;
		font-size: var(--lens-font-size-2xs);
		color: var(--lens-muted);
	}
	.tl-reason {
		margin-top: 2px;
		font-size: var(--lens-font-size-2xs);
		color: var(--lens-danger);
		font-style: italic;
	}
</style>
