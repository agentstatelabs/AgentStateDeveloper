<script lang="ts">
	import type { EffectsOverviewRow } from '@agentstate/lens-core';
	import { asdClient } from '$lib/api';
	import { symbols } from '$lib/stores.svelte';

	let rows = $state<EffectsOverviewRow[]>([]);
	let err = $state<string | null>(null);
	let loading = $state(true);

	$effect(() => {
		loading = true;
		err = null;
		asdClient
			.effectsOverview()
			.then((r) => {
				rows = r;
				loading = false;
			})
			.catch((e) => {
				err = e instanceof Error ? e.message : String(e);
				loading = false;
			});
	});

	// Bars are scaled to the busiest category so relative weight is readable
	// at a glance; the absolute count sits next to the bar.
	const maxCount = $derived(rows.reduce((m, r) => Math.max(m, r.symbol_count), 0));
	const totalDeclarers = $derived(rows.reduce((s, r) => s + r.symbol_count, 0));

	function barWidth(count: number): string {
		if (maxCount === 0) return '0%';
		// Floor at 2% so even 1-declarer categories render a visible sliver.
		return `${Math.max(2, (count / maxCount) * 100)}%`;
	}

	/** Rough family for the category (io / state / env / …) — drives bar tint. */
	function family(effect: string): string {
		const head = effect.split('.')[0];
		return ['io', 'state', 'proc', 'env'].includes(head) ? head : 'other';
	}

	function symbolHref(q: string): string {
		return `/symbols/${encodeURIComponent(q)}`;
	}

	// Stale decls fall back to raw symbol ids server-side — don't link those
	// (they'd 404 on /symbols/{qname}). Checked against the sidebar's symbol
	// index; before it loads we link optimistically.
	const knownQnames = $derived(new Set(symbols.list.map((s) => s.qname)));
	function isLinkable(qname: string): boolean {
		return !symbols.loaded || knownQnames.has(qname);
	}
</script>

<svelte:head>
	<title>Effects — ASD Lens</title>
</svelte:head>

<header class="page-header">
	<h1>Effect distribution</h1>
	<p class="page-desc">
		What the codebase touches, per declared effect category — and the declarers whose effects
		reach the most callers (transitive blast radius). Categories are ranked busiest-first.
	</p>
</header>

{#if loading}
	<div class="state-loading">loading…</div>
{:else if err}
	<div class="state-error">{err}</div>
{:else if rows.length === 0}
	<div class="state-empty">
		No effects declared yet — run <code>asd index .</code> with effect inference, or declare
		effects via <code>effect_declare</code>.
	</div>
{:else}
	<div class="summary-row">
		{rows.length} categor{rows.length === 1 ? 'y' : 'ies'} · {totalDeclarers} declaring symbol{totalDeclarers ===
		1
			? ''
			: 's'}
	</div>
	<table class="dist">
		<thead>
			<tr>
				<th class="col-effect">effect</th>
				<th class="col-count">symbols</th>
				<th class="col-bar" aria-hidden="true"></th>
				<th class="col-top">top declarers by blast radius</th>
			</tr>
		</thead>
		<tbody>
			{#each rows as row (row.effect)}
				<tr>
					<td class="col-effect">
						<code class="effect-name" data-family={family(row.effect)}>{row.effect}</code>
					</td>
					<td class="col-count">{row.symbol_count}</td>
					<td class="col-bar">
						<div class="bar-track">
							<div
								class="bar"
								data-family={family(row.effect)}
								style="width: {barWidth(row.symbol_count)}"
								role="img"
								aria-label="{row.symbol_count} of {maxCount} symbols"
							></div>
						</div>
					</td>
					<td class="col-top">
						{#if row.top_symbols.length === 0}
							<span class="muted">—</span>
						{:else}
							<ul class="top-symbols">
								{#each row.top_symbols as t (t.qname)}
									<li>
										{#if isLinkable(t.qname)}
											<a class="qname" href={symbolHref(t.qname)}>{t.qname}</a>
										{:else}
											<code class="qname stale" title="declarer no longer indexed">{t.qname}</code>
										{/if}
										<span class="radius" title="symbols transitively inheriting this effect">
											{t.blast_radius}
										</span>
									</li>
								{/each}
							</ul>
						{/if}
					</td>
				</tr>
			{/each}
		</tbody>
	</table>
{/if}

<style>
	.summary-row {
		color: var(--lens-muted);
		font-size: var(--lens-font-size-2xs);
		margin-bottom: var(--lens-space-3);
	}
	.muted {
		color: var(--lens-muted);
	}
	.dist {
		width: 100%;
		border-collapse: collapse;
		font-size: var(--lens-font-size-xs);
	}
	.dist th {
		text-align: left;
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: var(--lens-tracking-caps);
		color: var(--lens-muted);
		font-weight: 600;
		padding: 6px 10px;
		border-bottom: 1px solid var(--lens-border);
	}
	.dist td {
		padding: 8px 10px;
		border-bottom: 1px solid var(--lens-border-subtle);
		vertical-align: top;
	}
	.dist tbody tr {
		transition: background var(--lens-dur-fast) var(--lens-ease);
	}
	.dist tbody tr:hover {
		background: var(--lens-surface);
	}
	.col-effect {
		width: 160px;
		white-space: nowrap;
	}
	.effect-name {
		background: transparent;
		padding: 0;
		font-size: var(--lens-font-size-xs);
	}
	.effect-name[data-family='io'] {
		color: var(--lens-kind-function);
	}
	.effect-name[data-family='state'] {
		color: var(--lens-kind-variable);
	}
	.effect-name[data-family='proc'] {
		color: var(--lens-kind-class);
	}
	.effect-name[data-family='env'] {
		color: var(--lens-kind-module);
	}
	.effect-name[data-family='other'] {
		color: var(--lens-muted);
	}
	.col-count {
		width: 60px;
		text-align: right;
		font-variant-numeric: tabular-nums;
		font-family: var(--lens-font-mono);
		color: var(--lens-text);
	}
	.col-bar {
		width: 30%;
		min-width: 140px;
	}
	.bar-track {
		height: 10px;
		margin-top: 3px;
		background: var(--lens-surface);
		border: 1px solid var(--lens-border);
		border-radius: 2px;
		overflow: hidden;
	}
	.bar {
		height: 100%;
		background: var(--lens-accent);
		opacity: 0.75;
		border-radius: 1px;
	}
	.bar[data-family='io'] {
		background: var(--lens-kind-function);
	}
	.bar[data-family='state'] {
		background: var(--lens-kind-variable);
	}
	.bar[data-family='proc'] {
		background: var(--lens-kind-class);
	}
	.bar[data-family='env'] {
		background: var(--lens-kind-module);
	}
	.top-symbols {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 4px 14px;
	}
	.top-symbols li {
		display: inline-flex;
		align-items: baseline;
		gap: 5px;
		min-width: 0;
	}
	.qname {
		font-family: var(--lens-font-mono);
		color: var(--lens-accent);
		overflow-wrap: anywhere;
	}
	a.qname:hover {
		color: var(--lens-accent-hover);
		text-decoration: underline;
	}
	.qname.stale {
		color: var(--lens-muted);
		background: transparent;
		padding: 0;
	}
	.radius {
		font-size: 10px;
		font-variant-numeric: tabular-nums;
		font-family: var(--lens-font-mono);
		color: var(--lens-muted);
		border: 1px solid var(--lens-border);
		border-radius: var(--lens-radius-full);
		padding: 0 6px;
	}
</style>
