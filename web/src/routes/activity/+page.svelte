<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import {
		ActivityFeed,
		VerifyBadge,
		activityEventKey,
		activityKindMeta,
		type ActivityEvent,
		type ActivityGroup
	} from '@agentstate/lens-core';
	import { asdClient } from '$lib/api';

	// -- feed state ------------------------------------------------------
	// Seeded once from /timeline, then appended from the /events SSE
	// stream. Deduped by activityEventKey: the stream baseline is taken at
	// subscribe time, so the only overlap risk is the short window between
	// opening the stream and the seed responding.
	let events = $state<ActivityEvent[]>([]);
	let seen = new Set<string>();
	let seedError = $state<string | null>(null);
	let seeded = false;
	const MAX_EVENTS = 500;

	function seed() {
		asdClient
			.timeline({ limit: 300 })
			.then((batch) => {
				ingest(batch);
				seeded = true;
				seedError = null;
			})
			.catch((e) => (seedError = e instanceof Error ? e.message : String(e)));
	}

	function ingest(batch: ActivityEvent[]) {
		const fresh = batch.filter((e) => {
			const k = activityEventKey(e);
			if (seen.has(k)) return false;
			seen.add(k);
			return true;
		});
		if (fresh.length === 0) return;
		events = [...fresh, ...events]
			.sort((a, b) => new Date(b.at).getTime() - new Date(a.at).getTime())
			.slice(0, MAX_EVENTS);
	}

	// -- SSE lifecycle -----------------------------------------------------
	// Browser-only (this SPA is ssr=false, and everything lives in
	// onMount/onDestroy regardless). EventSource auto-reconnects while in
	// CONNECTING; if the browser gives up (CLOSED) we recreate it ourselves
	// after a short backoff.
	type ConnState = 'connecting' | 'live' | 'reconnecting';
	let conn = $state<ConnState>('connecting');
	let es: EventSource | null = null;
	let retryTimer: ReturnType<typeof setTimeout> | null = null;
	const RETRY_MS = 3000;

	function connect() {
		es?.close();
		es = new EventSource(asdClient.eventsUrl());
		es.onopen = () => (conn = 'live');
		es.onmessage = (m) => {
			try {
				ingest([JSON.parse(m.data) as ActivityEvent]);
			} catch {
				// keep-alive comments never reach onmessage; a malformed
				// frame is not worth killing the stream over.
			}
		};
		es.onerror = () => {
			conn = 'reconnecting';
			if (es?.readyState === EventSource.CLOSED) {
				if (retryTimer) clearTimeout(retryTimer);
				retryTimer = setTimeout(connect, RETRY_MS);
			}
		};
	}

	// Watchdog: an EventSource behind a proxy can stay "open" after the
	// upstream dies (the proxy holds the client socket), so onerror alone
	// can leave the pill lying. A cheap periodic /health probe keeps the
	// indicator honest and force-reconnects a wedged stream.
	let watchdog: ReturnType<typeof setInterval> | null = null;
	const WATCHDOG_MS = 20_000;
	async function checkAlive() {
		try {
			await asdClient.health();
			if (es?.readyState === EventSource.OPEN) {
				conn = 'live';
			} else if (es?.readyState === EventSource.CLOSED) {
				connect();
			}
			// Server was down when the page loaded: backfill history now.
			if (!seeded) seed();
		} catch {
			conn = 'reconnecting';
			connect(); // upstream died under the proxy: recycle the stream
		}
	}

	onMount(() => {
		connect();
		watchdog = setInterval(checkAlive, WATCHDOG_MS);
		seed();
	});

	onDestroy(() => {
		if (retryTimer) clearTimeout(retryTimer);
		if (watchdog) clearInterval(watchdog);
		es?.close();
		es = null;
	});

	// -- filtering ---------------------------------------------------------
	type Filter = 'all' | ActivityGroup;
	const FILTERS: { id: Filter; label: string }[] = [
		{ id: 'all', label: 'All' },
		{ id: 'ledger', label: 'Decisions' },
		{ id: 'thinking', label: 'Thinking' },
		{ id: 'effects', label: 'Effects' },
		{ id: 'system', label: 'System' }
	];
	let filter = $state<Filter>('all');
	const visible = $derived(
		filter === 'all' ? events : events.filter((e) => activityKindMeta(e.kind).group === filter)
	);
	function countOf(f: Filter): number {
		return f === 'all'
			? events.length
			: events.filter((e) => activityKindMeta(e.kind).group === f).length;
	}
</script>

<svelte:head>
	<title>Activity — ASD Lens</title>
</svelte:head>

<header class="page-header">
	<div class="title-row">
		<h1>Activity</h1>
		<span class="conn" data-state={conn}>
			<span class="conn-dot"></span>
			{conn === 'live' ? 'live' : conn}
		</span>
		<div class="header-badge">
			<VerifyBadge client={asdClient} />
		</div>
	</div>
	<p class="page-desc">
		Everything the agent records, as it happens — decisions and their reasoning, declared
		effects, index runs — each one traceable to the hash-chained audit log. Click any event to
		see what, why, and the proof.
	</p>
</header>

<div class="toolbar">
	<div class="chips" role="group" aria-label="filter activity by family">
		{#each FILTERS as f (f.id)}
			<button
				type="button"
				class="chip"
				class:active={filter === f.id}
				onclick={() => (filter = f.id)}
			>
				{f.label}
				<span class="chip-n">{countOf(f.id)}</span>
			</button>
		{/each}
	</div>
	<span class="cap-note muted">
		{#if events.length >= MAX_EVENTS}showing latest {MAX_EVENTS}{:else}{events.length} event{events.length === 1 ? '' : 's'}{/if}
	</span>
</div>

{#if seedError}
	<div class="banner" data-tone="danger" style="margin: 0 0 12px;">
		timeline unavailable: {seedError} — showing live events only
	</div>
{/if}

<ActivityFeed
	events={visible}
	client={asdClient}
	symbolHref={(q) => `/symbols/${encodeURIComponent(q)}`}
	emptyText={filter !== 'all' && events.length > 0
		? `no ${FILTERS.find((f) => f.id === filter)?.label.toLowerCase()} events in the last ${events.length}`
		: conn === 'live'
			? 'no recorded activity yet — waiting for the next agent write…'
			: 'no recorded activity yet'}
/>

<style>
	.header-badge {
		margin-left: auto;
		min-width: 0;
	}

	/* connection pill */
	.conn {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		font-size: 10px;
		font-weight: 700;
		text-transform: uppercase;
		letter-spacing: 0.12em;
		padding: 2px 9px;
		border-radius: var(--lens-radius-full);
		border: 1px solid var(--lens-border);
		color: var(--lens-muted);
	}
	.conn-dot {
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: currentColor;
	}
	.conn[data-state='live'] {
		color: var(--lens-ok);
		border-color: var(--lens-ok-border);
		background: var(--lens-ok-tint);
	}
	.conn[data-state='live'] .conn-dot {
		animation: pulse 2s ease-in-out infinite;
	}
	.conn[data-state='reconnecting'] {
		color: var(--lens-warn);
		border-color: var(--lens-warn-border);
		background: var(--lens-warn-tint);
	}
	@keyframes pulse {
		0%,
		100% {
			box-shadow: 0 0 0 0 color-mix(in srgb, var(--lens-ok) 55%, transparent);
		}
		50% {
			box-shadow: 0 0 0 5px transparent;
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.conn[data-state='live'] .conn-dot {
			animation: none;
		}
	}

	/* toolbar */
	.toolbar {
		display: flex;
		align-items: center;
		gap: var(--lens-space-3);
		margin-bottom: var(--lens-space-4);
		flex-wrap: wrap;
	}
	.chips {
		display: flex;
		gap: 6px;
		flex-wrap: wrap;
	}
	.chip {
		appearance: none;
		font: inherit;
		font-size: var(--lens-font-size-2xs);
		display: inline-flex;
		align-items: baseline;
		gap: 6px;
		padding: 3px 10px;
		border-radius: var(--lens-radius-full);
		border: 1px solid var(--lens-border);
		background: var(--lens-surface);
		color: var(--lens-muted);
		cursor: pointer;
		transition:
			color var(--lens-dur-fast) var(--lens-ease),
			background var(--lens-dur-fast) var(--lens-ease),
			border-color var(--lens-dur-fast) var(--lens-ease);
	}
	.chip:hover {
		background: var(--lens-surface-raised);
		color: var(--lens-text);
	}
	.chip.active {
		border-color: var(--lens-accent-border);
		color: var(--lens-accent);
		background: var(--lens-accent-tint);
	}
	.chip-n {
		font-size: 10px;
		font-family: var(--lens-font-mono);
		opacity: 0.75;
	}
	.cap-note {
		margin-left: auto;
		font-size: var(--lens-font-size-2xs);
		color: var(--lens-muted);
	}
</style>
