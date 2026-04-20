<script lang="ts">
	import { getAudit, getAuditVerify } from '$lib/api';
	import type { AuditEvent, AuditResponse, AuditVerifyReport } from '$lib/types';
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

<header class="a-header">
	<h1>Audit log</h1>
	<p class="muted">
		Append-only, hash-chained JSONL event stream. Every ledger mutation and policy evaluation, across CLI / MCP / HTTP.
	</p>
	{#if response?.configured === false}
		<div class="banner">
			<strong>No audit log configured.</strong>
			<span>
				Start <code>asd-serve</code> with <code>ASD_AUDIT_LOG=/path/to/audit.jsonl</code> to begin capturing events.
			</span>
		</div>
	{:else if verify}
		{#if verify.verified}
			<div class="verify ok">
				<span class="dot"></span>
				<strong>Chain verified</strong>
				<span class="v-detail">
					{verify.signed_events} signed{#if verify.unsigned_events > 0}, {verify.unsigned_events} unsigned{/if}
				</span>
			</div>
		{:else if verify.chain_breaks.length > 0}
			<div class="verify bad">
				<span class="dot"></span>
				<strong>{verify.chain_breaks.length} chain break{verify.chain_breaks.length === 1 ? '' : 's'}</strong>
				<details class="breaks">
					<summary>inspect</summary>
					<ul>
						{#each verify.chain_breaks as b (b.event_id)}
							<li>
								<code>#{b.index}</code>
								<code>{b.event_id}</code>
								<span class="reason">{b.reason}</span>
							</li>
						{/each}
					</ul>
				</details>
			</div>
		{:else if verify.signed_events === 0 && verify.total_events > 0}
			<div class="verify warn">
				<span class="dot"></span>
				<strong>Unsigned log</strong>
				<span class="v-detail">
					{verify.unsigned_events} legacy event{verify.unsigned_events === 1 ? '' : 's'} — hash chain starts with the next emit
				</span>
			</div>
		{/if}
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
	<div class="error">{err}</div>
{:else if loading && !response}
	<div class="muted">loading…</div>
{:else if response && response.events.length === 0}
	<div class="muted empty">No matching events.</div>
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
	.a-header {
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
	.a-header .muted {
		margin: 4px 0 0 0;
		font-size: 12px;
	}
	.banner {
		margin-top: 10px;
		padding: 8px 12px;
		background: rgba(235, 203, 139, 0.08);
		border: 1px solid rgba(235, 203, 139, 0.3);
		color: #ebcb8b;
		border-radius: 4px;
		font-size: 12px;
	}
	.banner strong { margin-right: 6px; }
	.filters {
		display: flex;
		flex-wrap: wrap;
		gap: 10px;
		align-items: center;
		margin-bottom: 12px;
		font-size: 11px;
		color: var(--fg-dim);
	}
	.filters label {
		display: flex;
		gap: 4px;
		align-items: center;
	}
	.filters input {
		background: var(--bg);
		color: var(--fg);
		border: 1px solid var(--border);
		border-radius: 3px;
		padding: 3px 6px;
		font-family: inherit;
		font-size: 11px;
	}
	.filters input[type='text'] {
		width: 180px;
	}
	.filters input[type='number'] {
		width: 70px;
	}
	.filters button {
		background: var(--bg-alt);
		color: var(--fg);
		border: 1px solid var(--border);
		border-radius: 3px;
		padding: 3px 10px;
		font-size: 11px;
		cursor: pointer;
		font-family: inherit;
	}
	.filters button:hover:not(:disabled) {
		background: var(--bg-hover);
	}
	.path-row {
		color: var(--fg-dim);
		font-size: 11px;
		margin-bottom: 12px;
	}
	.events {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.events li {
		padding: 10px 14px;
		margin-bottom: 6px;
		background: var(--bg-alt);
		border: 1px solid var(--border);
		border-radius: 4px;
	}
	.row-head {
		display: flex;
		gap: 10px;
		align-items: baseline;
		flex-wrap: wrap;
		font-size: 12px;
	}
	.type {
		color: var(--accent);
		font-weight: 600;
		font-family: 'SF Mono', ui-monospace, monospace;
	}
	.outcome {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		padding: 1px 6px;
		border-radius: 3px;
	}
	.outcome.ok {
		background: rgba(111, 207, 151, 0.15);
		color: var(--ok);
	}
	.outcome.bad {
		background: rgba(224, 108, 117, 0.15);
		color: var(--bad);
	}
	.outcome.warn {
		background: rgba(235, 203, 139, 0.15);
		color: #ebcb8b;
	}
	.actor {
		color: var(--fg-dim);
	}
	.time {
		color: var(--fg-dim);
		margin-left: auto;
		font-size: 11px;
		font-family: 'SF Mono', ui-monospace, monospace;
	}
	.row-ids {
		margin-top: 4px;
		color: var(--fg-dim);
		font-size: 11px;
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
	}
	.reason {
		margin-top: 6px;
		color: var(--bad);
		font-size: 11px;
		font-style: italic;
	}
	.payload {
		margin-top: 6px;
	}
	.payload summary {
		cursor: pointer;
		color: var(--fg-dim);
		font-size: 11px;
	}
	.payload pre {
		margin: 4px 0 0 0;
		padding: 6px 10px;
		background: var(--bg);
		border: 1px solid var(--border);
		border-radius: 3px;
		font-size: 11px;
		overflow-x: auto;
	}
	.error {
		color: var(--bad);
	}
	.empty {
		padding: 24px 0;
	}
	.verify {
		display: flex;
		align-items: center;
		gap: 10px;
		margin-top: 10px;
		padding: 8px 12px;
		border-radius: 4px;
		font-size: 12px;
	}
	.verify.ok {
		background: rgba(111, 207, 151, 0.10);
		border: 1px solid rgba(111, 207, 151, 0.4);
		color: var(--ok);
	}
	.verify.bad {
		background: rgba(224, 108, 117, 0.10);
		border: 1px solid rgba(224, 108, 117, 0.4);
		color: var(--bad);
	}
	.verify.warn {
		background: rgba(235, 203, 139, 0.08);
		border: 1px solid rgba(235, 203, 139, 0.35);
		color: #ebcb8b;
	}
	.verify .dot {
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: currentColor;
	}
	.verify .v-detail {
		color: var(--fg-dim);
		font-weight: 400;
	}
	.verify .breaks summary {
		cursor: pointer;
		color: inherit;
	}
	.verify .breaks ul {
		list-style: none;
		padding: 6px 0 0 0;
		margin: 0;
	}
	.verify .breaks li {
		display: flex;
		gap: 8px;
		padding: 3px 0;
		font-size: 11px;
	}
	.verify .breaks .reason {
		color: var(--bad);
	}
	.live-toggle {
		display: flex;
		gap: 4px;
		align-items: center;
		padding-left: 8px;
		border-left: 1px solid var(--border);
		margin-left: 4px;
	}
	.live-toggle input {
		margin: 0;
	}
</style>
