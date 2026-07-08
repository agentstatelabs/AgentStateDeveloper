<script lang="ts">
	import type { Region } from '$lib/territory/data';
	import { THINKING_COLORS, KIND_COLORS, kindLabel } from '$lib/territory/data';

	let {
		region,
		pinned = false,
		onclose
	}: { region: Region; pinned?: boolean; onclose?: () => void } = $props();

	const fmtDay = (iso: string) => iso.slice(0, 10);
	let recent = $derived(
		[...region.thinking, ...region.hazards, ...region.decisions]
			.sort((a, b) => b.created_at.localeCompare(a.created_at))
			.slice(0, 5)
	);
</script>

<div class="card" class:pinned>
	<div class="head">
		<div>
			<div class="name">{region.name}</div>
			<div class="sub">
				{region.symbolCount} symbols · {region.fileCount} files
				{#if region.recencyDays != null}
					· last activity {region.recencyDays < 1 ? 'today' : `${Math.round(region.recencyDays)}d ago`}
				{/if}
			</div>
		</div>
		{#if pinned && onclose}
			<button class="close" onclick={onclose} aria-label="close">×</button>
		{/if}
	</div>

	<div class="stats">
		<span class="stat" style:color="#7aa2ff">{region.decisions.length} decisions</span>
		<span class="stat" style:color="#8be9c3">
			{region.thinking.length} thinking{region.meanConfidence != null
				? ` · conf ${region.meanConfidence.toFixed(2)}`
				: ''}
		</span>
		<span class="stat" style:color="#e0916c">
			{region.hazards.length} hazards · risk {(region.riskNorm * 100).toFixed(0)}%
		</span>
	</div>

	<div class="kindbar" title="kind mix">
		{#each Object.entries(region.kindMix).sort((a, b) => b[1] - a[1]) as [k, n] (k)}
			<span
				class="kseg"
				style:flex={n}
				style:background={KIND_COLORS[k] ?? '#666'}
				title="{k}: {n}"
			></span>
		{/each}
	</div>

	{#if recent.length}
		<div class="sect">recent judgment</div>
		<ul class="entries">
			{#each recent as e (e.entry_id)}
				<li>
					<span class="ekind" style:color={THINKING_COLORS[e.kind] ?? '#7aa2ff'}>
						{kindLabel(e.kind)}{typeof e.confidence === 'number' ? ` ${e.confidence.toFixed(2)}` : ''}
					</span>
					<span class="edate">{fmtDay(e.created_at)}</span>
					<div class="esum">{e.summary}</div>
				</li>
			{/each}
		</ul>
	{/if}

	<div class="sect">top symbols</div>
	<ul class="syms">
		{#each region.topSymbols as t (t.q)}
			<li>
				<a href="/symbols/{encodeURIComponent(t.q)}">
					<span class="kind" style:color={KIND_COLORS[t.k] ?? '#888'}>{t.k}</span>
					<span class="q">{t.q.split('.').slice(-2).join('.')}</span>
				</a>
			</li>
		{/each}
	</ul>
</div>

<style>
	.card {
		width: 320px;
		max-height: 70vh;
		overflow-y: auto;
		background: color-mix(in srgb, var(--lens-overlay) 96%, transparent);
		border: 1px solid var(--lens-border);
		border-radius: var(--lens-radius-md);
		padding: 12px 14px;
		box-shadow: var(--lens-shadow-lg);
		font-size: 12px;
		pointer-events: auto;
	}
	.card.pinned {
		border-color: var(--lens-border-strong);
	}
	.head {
		display: flex;
		justify-content: space-between;
		align-items: flex-start;
	}
	.name {
		font-weight: 700;
		font-size: 13px;
	}
	.sub {
		color: var(--fg-dim);
		font-size: 11px;
		margin-top: 2px;
	}
	.close {
		background: none;
		border: none;
		color: var(--fg-dim);
		font-size: 16px;
		cursor: pointer;
		padding: 0 2px;
	}
	.close:hover {
		color: var(--fg);
	}
	.stats {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		margin: 8px 0 6px;
		font-size: 11px;
	}
	.kindbar {
		display: flex;
		height: 4px;
		border-radius: 2px;
		overflow: hidden;
		margin-bottom: 4px;
	}
	.kseg {
		min-width: 1px;
	}
	.sect {
		margin: 10px 0 4px;
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: var(--lens-tracking-caps);
		color: var(--lens-muted);
	}
	.entries,
	.syms {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.entries li {
		padding: 5px 0;
		border-top: 1px solid var(--lens-border-subtle);
	}
	.ekind {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.05em;
	}
	.edate {
		float: right;
		color: var(--fg-dim);
		font-size: 10px;
	}
	.esum {
		color: var(--fg);
		margin-top: 2px;
		line-height: 1.35;
	}
	.syms li a {
		display: flex;
		gap: 8px;
		padding: 3px 0;
		align-items: baseline;
	}
	.syms li a:hover .q {
		color: var(--accent);
	}
	.syms .kind {
		font-size: 10px;
		text-transform: uppercase;
		width: 52px;
		flex-shrink: 0;
	}
	.syms .q {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
</style>
