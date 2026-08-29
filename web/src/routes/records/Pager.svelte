<!--
	Pager — offset/limit paging for the record tables.

	`total` is the size of the whole filtered set, not the page, so the
	"n–m of total" readout is honest even when only `limit` rows are on
	screen. `scanned` is surfaced separately by callers when the underlying
	read was itself bounded.
-->
<script lang="ts">
	let {
		total,
		offset,
		limit,
		onpage
	}: {
		total: number;
		offset: number;
		limit: number;
		onpage: (offset: number) => void;
	} = $props();

	let first = $derived(total === 0 ? 0 : offset + 1);
	let last = $derived(Math.min(offset + limit, total));
	let canPrev = $derived(offset > 0);
	let canNext = $derived(offset + limit < total);
</script>

<div class="pager">
	<span class="readout">
		{#if total === 0}
			no rows
		{:else}
			<strong>{first.toLocaleString()}–{last.toLocaleString()}</strong>
			of {total.toLocaleString()}
		{/if}
	</span>
	<div class="buttons">
		<button type="button" disabled={!canPrev} onclick={() => onpage(0)} title="first page">
			«
		</button>
		<button
			type="button"
			disabled={!canPrev}
			onclick={() => onpage(Math.max(0, offset - limit))}
			title="previous page"
		>
			‹ prev
		</button>
		<button type="button" disabled={!canNext} onclick={() => onpage(offset + limit)} title="next page">
			next ›
		</button>
	</div>
</div>

<style>
	.pager {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--lens-space-3);
		flex-wrap: wrap;
		margin-top: var(--lens-space-3);
		font-size: var(--lens-font-size-2xs);
		color: var(--lens-muted);
	}
	.readout strong {
		color: var(--lens-text);
		font-variant-numeric: tabular-nums;
	}
	.buttons {
		display: flex;
		gap: 4px;
	}
	button {
		appearance: none;
		cursor: pointer;
		border: 1px solid var(--lens-border);
		background: var(--lens-surface);
		color: var(--lens-text-secondary);
		font-family: inherit;
		font-size: var(--lens-font-size-2xs);
		padding: 3px 10px;
		border-radius: var(--lens-radius-sm);
	}
	button:hover:not(:disabled) {
		color: var(--lens-text-strong);
		border-color: var(--lens-border-strong);
	}
	button:disabled {
		opacity: 0.4;
		cursor: not-allowed;
	}
	button:focus-visible {
		outline: 2px solid var(--lens-focus);
		outline-offset: 2px;
	}
</style>
