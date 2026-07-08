<script lang="ts">
	/**
	 * DrillDown — slide-in panel shared by all three territory views.
	 *
	 * Clicking a region (island / continent / column) opens it; it shows the
	 * region's sub-structure: top symbols by judgment activity, then the
	 * region's decisions / thinking / hazards as clickable lists. Clicking an
	 * entry expands it into the full lens-core AccountabilityCard
	 * (what / why / reasoning / touches / proof), fed from the live API.
	 *
	 * `focusEntryId` lets a marker click land with its entry pre-expanded.
	 * Purely presentational: never touches layout state, so determinism holds.
	 */
	import type { Entry, Region } from '$lib/territory/data';
	import { KIND_COLORS, THINKING_COLORS, kindLabel } from '$lib/territory/data';
	import { AccountabilityCard, type ActivityEvent, type AsdClient } from '@agentstate/lens-core';
	import { asdClient } from '$lib/api';

	/**
	 * `/api/v1/symbols/{qname}/callers` re-scans the index per call at ExampleProj
	 * scale — measured minutes at 100% CPU while holding the engine mutex, the
	 * same per-qname pathology as `/symbols` and `/thinking` documented in
	 * $lib/territory/data.ts. One expanded card would starve every other API
	 * consumer, so the drill-down client declines that endpoint up front;
	 * AccountabilityCard already degrades to its explicit "absence is
	 * information" rendering (blast radius simply isn't shown). Drop this
	 * override once the server answers caller lookups from the reverse-edge
	 * index.
	 */
	const drillClient: AsdClient = {
		...asdClient,
		callers: () =>
			Promise.reject(new Error('callers lookup disabled in territory drill-down (server scan too slow)'))
	};

	let {
		region,
		focusEntryId = null,
		onclose
	}: { region: Region | null; focusEntryId?: string | null; onclose: () => void } = $props();

	let expandedId = $state<string | null>(null);
	// Re-seed the expanded entry whenever the panel retargets.
	$effect(() => {
		void region?.id;
		expandedId = focusEntryId ?? null;
	});

	function toEvent(e: Entry): ActivityEvent {
		return {
			at: e.created_at,
			kind: e.kind,
			symbol_id: e.symbol_id,
			qname: e.qname ?? null,
			summary: e.summary,
			entry_id: e.entry_id
		};
	}

	function toggle(id: string) {
		expandedId = expandedId === id ? null : id;
	}

	/** Scroll a just-expanded card into view (used for marker-click focus). */
	function scrollInto(node: HTMLElement) {
		requestAnimationFrame(() => node.scrollIntoView({ block: 'nearest', behavior: 'smooth' }));
	}

	const sections = $derived(
		region
			? [
					{ title: 'decisions', color: '#7aa2ff', entries: region.decisions },
					{ title: 'thinking', color: '#8be9c3', entries: region.thinking },
					{ title: 'hazards', color: '#e06c75', entries: region.hazards }
				].filter((s) => s.entries.length > 0)
			: []
	);

	function entryColor(e: Entry): string {
		return THINKING_COLORS[e.kind] ?? (e.kind === 'hazard' || e.kind === 'known_bug' ? '#e06c75' : '#7aa2ff');
	}
</script>

<svelte:window
	onkeydown={(e) => {
		if (e.key === 'Escape' && region) onclose();
	}}
/>

{#if region}
	{#key region.id}
		<aside class="drill" aria-label="Region drill-down: {region.name}">
			<header>
				<div class="htext">
					<div class="hname">{region.name}</div>
					<div class="hsub">
						{region.symbolCount} symbols · {region.fileCount} files
						{#if region.recencyDays != null}
							· last activity {region.recencyDays < 1 ? 'today' : `${Math.round(region.recencyDays)}d ago`}
						{/if}
					</div>
				</div>
				<button class="close" onclick={onclose} aria-label="close drill-down">×</button>
			</header>

			<div class="stats">
				<span style:color="#7aa2ff">{region.decisions.length} decisions</span>
				<span style:color="#8be9c3">
					{region.thinking.length} thinking{region.meanConfidence != null
						? ` · conf ${region.meanConfidence.toFixed(2)}`
						: ''}
				</span>
				<span style:color="#e0916c">{region.hazards.length} hazards · risk {(region.riskNorm * 100).toFixed(0)}%</span>
			</div>

			<div class="sect">top symbols by judgment activity</div>
			<ul class="syms">
				{#each region.topSymbols as t (t.q)}
					<li>
						<a href="/symbols/{encodeURIComponent(t.q)}">
							<span class="skind" style:color={KIND_COLORS[t.k] ?? '#888'}>{t.k}</span>
							<span class="sq" title={t.q}>{t.q.split('.').slice(-2).join('.')}</span>
							<span class="sfile">{t.f.split('/').pop()}</span>
						</a>
					</li>
				{/each}
			</ul>

			{#each sections as s (s.title)}
				<div class="sect" style:color={s.color}>{s.title} · {s.entries.length}</div>
				<ul class="entries">
					{#each s.entries as e (e.entry_id)}
						<li>
							<button class="erow" class:open={expandedId === e.entry_id} onclick={() => toggle(e.entry_id)}>
								<span class="edot" style:background={entryColor(e)}></span>
								<span class="ekind" style:color={entryColor(e)}>
									{kindLabel(e.kind)}{typeof e.confidence === 'number' ? ` ${e.confidence.toFixed(2)}` : ''}
								</span>
								<span class="edate">{e.created_at.slice(0, 10)}</span>
								<span class="esum">{e.summary}</span>
							</button>
							{#if expandedId === e.entry_id}
								<div class="card" use:scrollInto>
									<AccountabilityCard
										client={drillClient}
										event={toEvent(e)}
										symbolHref={(q) => `/symbols/${encodeURIComponent(q)}`}
									/>
								</div>
							{/if}
						</li>
					{/each}
				</ul>
			{/each}
			{#if sections.length === 0}
				<div class="none">no recorded judgment in this region yet</div>
			{/if}
		</aside>
	{/key}
{/if}

<style>
	.drill {
		position: absolute;
		top: 0;
		right: 0;
		bottom: 0;
		width: min(440px, 92%);
		z-index: 25;
		overflow-y: auto;
		background: rgb(13, 16, 22);
		border-left: 1px solid var(--border);
		box-shadow: -12px 0 40px rgba(0, 0, 0, 0.5);
		padding: 14px 16px 24px;
		box-sizing: border-box;
		animation: drill-in 180ms ease-out;
	}
	@keyframes drill-in {
		from {
			transform: translateX(24px);
			opacity: 0;
		}
		to {
			transform: translateX(0);
			opacity: 1;
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.drill {
			animation: none;
		}
	}
	header {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		gap: 10px;
	}
	.hname {
		font-weight: 700;
		font-size: 14px;
	}
	.hsub {
		color: var(--fg-dim);
		font-size: 11px;
		margin-top: 2px;
	}
	.close {
		background: none;
		border: none;
		color: var(--fg-dim);
		font-size: 18px;
		cursor: pointer;
		padding: 0 4px;
		line-height: 1;
	}
	.close:hover {
		color: var(--fg);
	}
	.stats {
		display: flex;
		flex-wrap: wrap;
		gap: 8px 12px;
		margin: 10px 0 4px;
		font-size: 11px;
	}
	.sect {
		margin: 14px 0 5px;
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		color: var(--fg-dim);
	}
	.syms,
	.entries {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.syms li a {
		display: flex;
		gap: 8px;
		padding: 3px 0;
		align-items: baseline;
		font-size: 12px;
	}
	.syms li a:hover .sq {
		color: var(--accent);
	}
	.skind {
		font-size: 10px;
		text-transform: uppercase;
		width: 52px;
		flex-shrink: 0;
	}
	.sq {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.sfile {
		margin-left: auto;
		color: var(--fg-dim);
		font-size: 10px;
		flex-shrink: 0;
	}
	.entries li {
		border-top: 1px solid rgba(255, 255, 255, 0.04);
	}
	.erow {
		display: grid;
		grid-template-columns: 8px auto auto 1fr;
		align-items: baseline;
		gap: 7px;
		width: 100%;
		text-align: left;
		font: inherit;
		font-size: 12px;
		color: var(--fg);
		background: none;
		border: none;
		padding: 6px 4px;
		cursor: pointer;
		border-radius: 4px;
		transition: background 120ms;
	}
	.erow:hover {
		background: rgba(255, 255, 255, 0.045);
	}
	.erow.open {
		background: rgba(122, 162, 255, 0.08);
	}
	.edot {
		width: 6px;
		height: 6px;
		border-radius: 50%;
		align-self: center;
	}
	.ekind {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.05em;
		white-space: nowrap;
	}
	.edate {
		color: var(--fg-dim);
		font-size: 10px;
		white-space: nowrap;
	}
	.esum {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		min-width: 0;
	}
	.card {
		margin: 4px 0 10px;
	}
	.none {
		color: var(--fg-dim);
		font-size: 12px;
		font-style: italic;
		margin-top: 8px;
	}
</style>
