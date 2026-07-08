<script lang="ts">
	import { getAudit, getAuditVerify } from '$lib/api';
	import type { AuditEvent, AuditResponse, AuditVerifyReport } from '$lib/types';
	import { VerifyBadge } from '@agentstate/lens-core';
	import { onDestroy } from 'svelte';

	let response = $state<AuditResponse | null>(null);
	let verify = $state<AuditVerifyReport | null>(null);
	let err = $state<string | null>(null);
	let loading = $state(true);

	// Filters — free-typed so operators can tail `ledger.` or a specific
	// event_type without a dropdown. Empty string = unfiltered.
	let eventType = $state('');
	let actor = $state('');
	let outcome = $state('');
	let limit = $state(200);

	// Live streaming — opt-in polling with a `since` cursor so only new
	// events come over the wire.
	let live = $state(false);
	let intervalSec = $state(3);
	let timer: ReturnType<typeof setInterval> | null = null;
	let lastSeenId = $state<string | null>(null);

	async function refresh() {
		loading = true;
		err = null;
		try {
			const [r, v] = await Promise.all([
				getAudit({
					eventType: eventType.trim() || undefined,
					actor: actor.trim() || undefined,
					outcome: outcome.trim() || undefined,
					limit
				}),
				getAuditVerify().catch(() => null)
			]);
			response = r;
			verify = v;
			lastSeenId = r.events.length > 0 ? r.events[r.events.length - 1].event_id : null;
		} catch (e) {
			err = String(e);
		} finally {
			loading = false;
		}
	}

	async function tick() {
		// Incremental poll: ask only for events after lastSeenId, then
		// prepend or append (API returns newest... actually we sort
		// ascending in the log order, so new events come after the
		// cursor). Also re-verify occasionally so breaks surface fast.
		if (!live) return;
		try {
			const [r, v] = await Promise.all([
				getAudit({
					eventType: eventType.trim() || undefined,
					actor: actor.trim() || undefined,
					outcome: outcome.trim() || undefined,
					limit,
					since: lastSeenId ?? undefined
				}),
				getAuditVerify().catch(() => null)
			]);
			if (v) verify = v;
			if (r.events.length > 0 && response) {
				response = {
					...response,
					count: response.count + r.events.length,
					events: [...response.events, ...r.events]
				};
				lastSeenId = r.events[r.events.length - 1].event_id;
			}
		} catch (e) {
			err = String(e);
		}
	}

	$effect(() => {
		// Non-reactive initial load — $effect re-runs when filters
		// change, which is the desired behaviour for manual refresh.
		refresh();
	});

	$effect(() => {
		// Start / stop the polling timer whenever `live` or
		// `intervalSec` changes.
		if (timer) {
			clearInterval(timer);
			timer = null;
		}
		if (live) {
			const ms = Math.max(1, intervalSec) * 1000;
			timer = setInterval(tick, ms);
		}
	});

	onDestroy(() => {
		if (timer) clearInterval(timer);
	});

	function ts(s: string): string {
		try {
			return new Date(s).toISOString().replace('T', ' ').replace(/\..*$/, 'Z');
		} catch {
			return s;
		}
	}

	function outcomeClass(o: string): string {
		if (o === 'denied' || o === 'error' || o === 'unauthorized' || o === 'rejected') return 'bad';
		if (o === 'awaiting-approval' || o === 'already-resolved' || o === 'already-approved'
			|| o === 'already-rejected' || o === 'already-withdrawn' || o === 'no-policy-match') {
			return 'warn';
		}
		return 'ok';
	}

	function fmtPayload(v: unknown): string | null {
		if (v == null || v === undefined) return null;
		if (typeof v === 'object' && Object.keys(v as object).length === 0) return null;
		try {
			return JSON.stringify(v);
		} catch {
			return null;
		}
	}
</script>

<header class="page-header">
	<h1>Audit log</h1>
	<p class="page-desc">
		Append-only, hash-chained JSONL event stream. Every ledger mutation and policy evaluation, across CLI / MCP / HTTP.
	</p>
	{#if response?.configured === false}
		<div class="banner" data-tone="warn">
			<strong>No audit log configured.</strong>
			<span>
				Start <code>asd-serve</code> with <code>ASD_AUDIT_LOG=/path/to/audit.jsonl</code> to begin capturing events.
			</span>
		</div>
	{:else if verify}
		<!-- Shared VerifyBadge (lens-core) — same chain-status pill the
		     /activity AccountabilityCards use. `report` keeps it in sync
		     with this page's live polling instead of self-fetching. -->
		<div class="verify-row">
			<VerifyBadge report={verify} />
			{#if (verify.chain_breaks?.length ?? 0) > 1}
				<details class="breaks">
					<summary>all {verify.chain_breaks?.length} breaks</summary>
					<ul>
						{#each verify.chain_breaks ?? [] as b (b.event_id)}
							<li>
								<code>#{b.index}</code>
								<code>{b.event_id}</code>
								<span class="reason">{b.reason}</span>
							</li>
						{/each}
					</ul>
				</details>
			{/if}
		</div>
	{/if}
</header>

<div class="filters">
	<label>
		event type
		<input type="text" placeholder="ledger. / ledger.approve" bind:value={eventType} onchange={refresh} />
	</label>
	<label>
		actor
		<input type="text" placeholder="alice" bind:value={actor} onchange={refresh} />
	</label>
	<label>
		outcome
		<input type="text" placeholder="denied / approved / ..." bind:value={outcome} onchange={refresh} />
	</label>
	<label>
		limit
		<input type="number" min="1" max="1000" bind:value={limit} onchange={refresh} />
	</label>
	<button onclick={refresh} disabled={loading}>{loading ? 'loading…' : 'refresh'}</button>
	<label class="live-toggle">
		<input type="checkbox" bind:checked={live} />
		live
	</label>
	{#if live}
		<label>
			every
			<input type="number" min="1" max="60" bind:value={intervalSec} />s
		</label>
	{/if}
</div>

{#if err}
	<div class="state-error">{err}</div>
{:else if loading && !response}
	<div class="state-loading">loading…</div>
{:else if response && response.events.length === 0}
	<div class="state-empty">No matching events — loosen the filters or raise the limit.</div>
{:else if response}
	<div class="path-row">
		{response.count} event{response.count === 1 ? '' : 's'}
		{#if response.path}
			· <code>{response.path}</code>
		{/if}
	</div>
	<ul class="events">
		{#each response.events as ev (ev.event_id)}
			{@const payload = fmtPayload(ev.payload)}
			<li>
				<div class="row-head">
					<span class="type">{ev.event_type}</span>
					<span class="outcome {outcomeClass(ev.outcome)}">{ev.outcome}</span>
					<span class="actor">{ev.actor_kind}:{ev.actor_id}</span>
					<span class="time">{ts(ev.timestamp)}</span>
				</div>
				<div class="row-ids">
					{#if ev.subject_id}
						<span>subject: <code>{ev.subject_id}</code></span>
					{/if}
					{#if ev.secondary_id}
						<span>· <code>{ev.secondary_id}</code></span>
					{/if}
					{#if ev.matched_policy}
						<span>· policy: <code>{ev.matched_policy}</code></span>
					{/if}
				</div>
				{#if ev.reason}
					<div class="reason">{ev.reason}</div>
				{/if}
				{#if payload}
					<details class="payload">
						<summary>payload</summary>
						<pre>{payload}</pre>
					</details>
				{/if}
			</li>
		{/each}
	</ul>
{/if}

<style>
	.filters {
		display: flex;
		flex-wrap: wrap;
		gap: 10px;
		align-items: center;
		margin-bottom: var(--lens-space-3);
		font-size: var(--lens-font-size-2xs);
		color: var(--lens-muted);
	}
	.filters label {
		display: flex;
		gap: 4px;
		align-items: center;
	}
	.filters input {
		background: var(--lens-bg);
		color: var(--lens-text);
		border: 1px solid var(--lens-border);
		border-radius: var(--lens-radius-sm);
		padding: 3px 6px;
		font-family: inherit;
		font-size: var(--lens-font-size-2xs);
	}
	.filters input::placeholder {
		color: var(--lens-text-faint);
	}
	.filters input:focus {
		outline: none;
		border-color: var(--lens-accent);
		box-shadow: 0 0 0 3px var(--lens-accent-tint);
	}
	.filters input[type='text'] {
		width: 180px;
	}
	.filters input[type='number'] {
		width: 70px;
	}
	.filters button {
		background: var(--lens-surface);
		color: var(--lens-text);
		border: 1px solid var(--lens-border);
		border-radius: var(--lens-radius-sm);
		padding: 3px 10px;
		font-size: var(--lens-font-size-2xs);
		cursor: pointer;
		font-family: inherit;
		transition: background var(--lens-dur-fast) var(--lens-ease);
	}
	.filters button:hover:not(:disabled) {
		background: var(--lens-surface-raised);
	}
	.path-row {
		color: var(--lens-muted);
		font-size: var(--lens-font-size-2xs);
		margin-bottom: var(--lens-space-3);
	}
	.events {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.events li {
		padding: 10px 14px;
		margin-bottom: 6px;
		background: var(--lens-surface);
		border: 1px solid var(--lens-border);
		border-radius: var(--lens-radius-sm);
	}
	.row-head {
		display: flex;
		gap: 10px;
		align-items: baseline;
		flex-wrap: wrap;
		font-size: var(--lens-font-size-xs);
	}
	.type {
		color: var(--lens-accent);
		font-weight: 600;
		font-family: var(--lens-font-mono);
	}
	.outcome {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: var(--lens-tracking-caps);
		padding: 1px 6px;
		border-radius: var(--lens-radius-sm);
	}
	.outcome.ok {
		background: var(--lens-ok-tint);
		color: var(--lens-ok);
	}
	.outcome.bad {
		background: var(--lens-danger-tint);
		color: var(--lens-danger);
	}
	.outcome.warn {
		background: var(--lens-warn-tint);
		color: var(--lens-warn);
	}
	.actor {
		color: var(--lens-muted);
	}
	.time {
		color: var(--lens-muted);
		margin-left: auto;
		font-size: var(--lens-font-size-2xs);
		font-family: var(--lens-font-mono);
	}
	.row-ids {
		margin-top: 4px;
		color: var(--lens-muted);
		font-size: var(--lens-font-size-2xs);
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.reason {
		margin-top: 6px;
		color: var(--lens-danger);
		font-size: var(--lens-font-size-2xs);
		font-style: italic;
	}
	.payload {
		margin-top: 6px;
	}
	.payload summary {
		cursor: pointer;
		color: var(--lens-muted);
		font-size: var(--lens-font-size-2xs);
	}
	.payload pre {
		margin: 4px 0 0 0;
		padding: 6px 10px;
		background: var(--lens-bg);
		border: 1px solid var(--lens-border);
		border-radius: var(--lens-radius-sm);
		font-family: var(--lens-font-mono);
		font-size: var(--lens-font-size-2xs);
		overflow-x: auto;
	}
	.verify-row {
		display: flex;
		align-items: baseline;
		gap: var(--lens-space-3);
		margin-top: 10px;
		font-size: var(--lens-font-size-xs);
	}
	.breaks summary {
		cursor: pointer;
		color: var(--lens-danger);
	}
	.breaks ul {
		list-style: none;
		padding: 6px 0 0 0;
		margin: 0;
	}
	.breaks li {
		display: flex;
		gap: var(--lens-space-2);
		padding: 3px 0;
		font-size: var(--lens-font-size-2xs);
	}
	.breaks .reason {
		color: var(--lens-danger);
	}
	.live-toggle {
		display: flex;
		gap: 4px;
		align-items: center;
		padding-left: var(--lens-space-2);
		border-left: 1px solid var(--lens-border);
		margin-left: 4px;
	}
	.live-toggle input {
		margin: 0;
	}
</style>
