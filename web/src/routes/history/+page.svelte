<script lang="ts">
	import { ProjectHistory, StoreHealth } from '@agentstate/lens-core';
	import type { HistoryReport } from '@agentstate/lens-core';
	import { asdClient } from '$lib/api';
	import { getGcDryRun, isGcUncomputed, type GcEstimate } from '$lib/metrics';

	let report = $state<HistoryReport | null>(null);
	let gc = $state<GcEstimate | null>(null);
	let gcState = $state<'loading' | 'computing' | 'ready' | 'unavailable'>('loading');
	let err = $state<string | null>(null);
	let unavailable = $state(false);
	let loading = $state(true);

	let view = $state<'history' | 'store'>('history');

	$effect(() => {
		loading = true;
		err = null;
		unavailable = false;
		// Store shape rides along on the history call. The GC dry run does NOT:
		// computing one walks the whole object DAG (~26s on a large store), and
		// awaiting it alongside the report held this entire page blank for that
		// long. Fetch the report on its own; let the reclaim panel fill in
		// behind it.
		asdClient
			.history({ store: true, granularity: 'day' })
			.then((h) => {
				report = h;
				loading = false;
			})
			.catch((e) => {
				if (is404(e)) unavailable = true;
				else err = e instanceof Error ? e.message : String(e);
				loading = false;
			});
		loadGc();
	});

	/**
	 * Reclaim estimate, fetched independently of the report.
	 *
	 * Ask for the server's memo first — that answers in milliseconds whether
	 * warm or cold. Only a cold cache starts the real walk, and by then the
	 * page has already rendered. The rest of the API stays responsive
	 * throughout because the server runs the walk on a private connection
	 * rather than the shared engine.
	 */
	function loadGc() {
		gcState = 'loading';
		getGcDryRun({ cachedOnly: true })
			.then((g) => {
				if (!isGcUncomputed(g)) {
					gc = g;
					gcState = 'ready';
					return;
				}
				gcState = 'computing';
				return getGcDryRun().then((fresh) => {
					if (!isGcUncomputed(fresh)) gc = fresh;
					gcState = 'ready';
				});
			})
			.catch(() => {
				// A missing GC endpoint shouldn't blank the history page — keep the
				// report, drop the reclaim panel.
				gc = null;
				gcState = 'unavailable';
			});
	}

	function is404(e: unknown): boolean {
		const m = e instanceof Error ? e.message : String(e);
		return m.includes(' 404 ') || m.includes('Not Found');
	}
</script>

<svelte:head>
	<title>History — ASD Lens</title>
</svelte:head>

<header class="page-header">
	<div class="title-row">
		<h1>Project history</h1>
		{#if report}
			<div class="seg" role="tablist" aria-label="History views">
				<button
					role="tab"
					aria-selected={view === 'history'}
					class:active={view === 'history'}
					onclick={() => (view = 'history')}
				>
					History
				</button>
				<button
					role="tab"
					aria-selected={view === 'store'}
					class:active={view === 'store'}
					onclick={() => (view = 'store')}
					disabled={!report.store_shape}
					title={report.store_shape ? undefined : 'Store shape needs ?store=1 support'}
				>
					Store health
				</button>
			</div>
		{/if}
	</div>
	<p class="page-desc">
		The commit chain distilled — velocity, intent mix, authorship and the milestone spine —
		plus where the store's bytes went and what a garbage collection would reclaim.
	</p>
</header>

{#if loading}
	<div class="state-loading">loading…</div>
{:else if unavailable}
	<div class="state-empty">
		<p>
			This server's engine predates the history/GC surface. The project-history and
			store-health metrics arrive with an <strong>AgentStateGraph release carrying Plan A</strong>
			— once <code>asd-serve</code> is built against it, <code>/api/v1/history</code> and
			<code>/api/v1/gc/dry-run</code> light up and this page fills in.
		</p>
	</div>
{:else if err}
	<div class="state-error">{err}</div>
{:else if report}
	{#if view === 'history'}
		<ProjectHistory {report} heading="" />
	{:else if report.store_shape}
		{#if gcState === 'computing'}
			<p class="gc-note">
				Computing the reclaim estimate — it walks the whole object graph, so it can take tens of
				seconds on a large store. The rest of this page (and the rest of the API) stays usable while
				it runs; the panel fills in when it lands.
			</p>
		{:else if gcState === 'unavailable'}
			<p class="gc-note">
				No reclaim estimate — this server's engine predates the GC surface.
			</p>
		{/if}
		<StoreHealth shape={report.store_shape} gc={gc ?? undefined} heading="" />
	{/if}
{/if}

<style>
	.page-header {
		margin-bottom: var(--lens-space-5);
	}
	.title-row {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--lens-space-4);
		flex-wrap: wrap;
	}
	.page-header h1 {
		margin: 0;
		font-size: var(--lens-font-size-xl);
		color: var(--lens-text-strong);
	}
	.page-desc {
		margin: var(--lens-space-2) 0 0;
		max-width: 68ch;
		color: var(--lens-text-secondary);
		font-size: var(--lens-font-size-sm);
		line-height: 1.5;
	}
	.seg {
		display: flex;
		gap: 2px;
		background: var(--lens-surface-raised);
		border: 1px solid var(--lens-border);
		border-radius: var(--lens-radius-md);
		padding: 2px;
	}
	.seg button {
		appearance: none;
		border: 0;
		background: transparent;
		color: var(--lens-muted);
		font-family: inherit;
		font-size: var(--lens-font-size-xs);
		font-weight: 600;
		padding: 5px 14px;
		border-radius: var(--lens-radius-sm);
		cursor: pointer;
		transition:
			color var(--lens-dur-fast) var(--lens-ease),
			background var(--lens-dur-fast) var(--lens-ease);
	}
	.seg button:hover:not(:disabled) {
		color: var(--lens-text);
	}
	.seg button.active {
		background: var(--lens-surface);
		color: var(--lens-accent);
	}
	.seg button:disabled {
		opacity: 0.45;
		cursor: not-allowed;
	}
	.seg button:focus-visible {
		outline: 2px solid var(--lens-focus);
		outline-offset: 2px;
	}
	.state-loading,
	.state-error,
	.state-empty {
		padding: var(--lens-space-5);
		border: 1px solid var(--lens-border);
		border-radius: var(--lens-radius-md);
		background: var(--lens-surface);
		color: var(--lens-text-secondary);
		font-size: var(--lens-font-size-sm);
		line-height: 1.55;
	}
	.state-error {
		color: var(--lens-danger);
		border-color: var(--lens-danger-border);
		background: var(--lens-danger-tint);
	}
	.state-empty :global(code) {
		font-family: var(--lens-font-mono);
		font-size: 0.9em;
	}
	.state-empty p {
		margin: 0;
		max-width: 72ch;
	}
	.gc-note {
		margin: 0 0 var(--lens-space-3);
		max-width: 80ch;
		padding: var(--lens-space-2) var(--lens-space-3);
		border: 1px solid var(--lens-border);
		border-radius: var(--lens-radius-md);
		background: var(--lens-surface);
		color: var(--lens-muted);
		font-size: var(--lens-font-size-2xs);
		line-height: 1.55;
	}
</style>
