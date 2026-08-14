<script lang="ts">
	import { ProjectHistory, StoreHealth } from '@agentstate/lens-core';
	import type { HistoryReport, GcDryRun } from '@agentstate/lens-core';
	import { asdClient } from '$lib/api';

	let report = $state<HistoryReport | null>(null);
	let gc = $state<GcDryRun | null>(null);
	let err = $state<string | null>(null);
	let unavailable = $state(false);
	let loading = $state(true);

	let view = $state<'history' | 'store'>('history');

	$effect(() => {
		loading = true;
		err = null;
		unavailable = false;
		// Store shape rides along on the history call; the GC dry run is a
		// separate estimate. Both live behind the same ASG release gate, so a
		// 404 on either means "this build's engine predates Plan A".
		Promise.all([
			asdClient.history({ store: true, granularity: 'day' }),
			asdClient.gcDryRun().catch((e) => {
				// A missing GC endpoint shouldn't blank the history page — keep the
				// report, drop the reclaim panel.
				if (is404(e)) return null;
				throw e;
			})
		])
			.then(([h, g]) => {
				report = h;
				gc = g;
				loading = false;
			})
			.catch((e) => {
				if (is404(e)) unavailable = true;
				else err = e instanceof Error ? e.message : String(e);
				loading = false;
			});
	});

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
</style>
