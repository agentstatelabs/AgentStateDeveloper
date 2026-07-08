<script lang="ts">
	/**
	 * EntryTip — tiny fixed-position tooltip for a single judgment marker
	 * (cairn / beacon / aurora / fossil / fault). Shared by all three
	 * territory views so markers read identically everywhere.
	 */
	import type { Entry } from '$lib/territory/data';
	import { THINKING_COLORS, kindLabel } from '$lib/territory/data';

	let {
		entry,
		count = 1,
		x,
		y
	}: { entry: Entry; count?: number; x: number; y: number } = $props();

	const left = $derived(Math.min(x + 16, window.innerWidth - 380));
	const top = $derived(Math.min(y + 14, window.innerHeight - 190));
</script>

<div class="tip" style:left="{left}px" style:top="{top}px">
	<div class="tkind" style:color={THINKING_COLORS[entry.kind] ?? '#7aa2ff'}>
		{kindLabel(entry.kind)}
		{#if typeof entry.confidence === 'number'}· confidence {entry.confidence.toFixed(2)}{/if}
		· {entry.created_at.slice(0, 10)}
	</div>
	<div class="tsum">{entry.summary}</div>
	{#if entry.qname}
		<div class="tq">{entry.qname}</div>
	{/if}
	<div class="thint">
		{#if count > 1}latest of {count} · {/if}click to drill down
	</div>
</div>

<style>
	.tip {
		position: fixed;
		z-index: 30;
		max-width: 360px;
		background: color-mix(in srgb, var(--lens-overlay) 97%, transparent);
		border: 1px solid var(--lens-border);
		border-radius: var(--lens-radius-md);
		padding: 9px 11px;
		font-size: 12px;
		pointer-events: none;
		box-shadow: var(--lens-shadow-lg);
	}
	.tkind {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		margin-bottom: 4px;
	}
	.tsum {
		line-height: 1.4;
		color: var(--fg);
	}
	.tq {
		margin-top: 5px;
		font-family: var(--lens-font-mono);
		color: var(--lens-accent);
		font-size: 11px;
		overflow-wrap: anywhere;
	}
	.thint {
		margin-top: 5px;
		font-size: 10px;
		color: var(--lens-text-faint);
	}
</style>
