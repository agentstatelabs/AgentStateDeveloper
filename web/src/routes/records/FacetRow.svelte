<!--
	FacetRow — one facet axis as a row of toggle chips.

	Counts come from the API's facet block, which is computed over the set
	matching the text + date filters but NOT the categorical ones. That's
	deliberate: it keeps every chip's count meaningful while you toggle
	between values on the same axis, instead of zeroing out the siblings of
	whatever you just selected.
-->
<script lang="ts">
	import type { FacetValue } from '$lib/metrics';

	let {
		label,
		values,
		selected,
		onselect
	}: {
		/** Axis name shown before the chips (e.g. "agent"). */
		label: string;
		values: FacetValue[];
		/** Currently active value, or undefined for "all". */
		selected: string | undefined;
		/** Called with the new value, or undefined when the chip is toggled off. */
		onselect: (value: string | undefined) => void;
	} = $props();

	function toggle(v: string) {
		onselect(selected === v ? undefined : v);
	}
</script>

{#if values.length > 0}
	<div class="facet">
		<span class="axis">{label}</span>
		<div class="chips">
			{#each values as f (f.value)}
				<button
					type="button"
					class:active={selected === f.value}
					aria-pressed={selected === f.value}
					onclick={() => toggle(f.value)}
					title={selected === f.value ? `clear ${label} filter` : `filter by ${f.value}`}
				>
					<span class="val">{f.value}</span>
					<span class="count">{f.count.toLocaleString()}</span>
				</button>
			{/each}
		</div>
	</div>
{/if}

<style>
	.facet {
		display: flex;
		align-items: baseline;
		gap: var(--lens-space-2);
		min-width: 0;
	}
	.axis {
		flex: 0 0 auto;
		font-size: var(--lens-font-size-2xs);
		text-transform: uppercase;
		letter-spacing: var(--lens-tracking-caps);
		color: var(--lens-text-faint);
	}
	.chips {
		display: flex;
		flex-wrap: wrap;
		gap: 4px;
		min-width: 0;
	}
	button {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		appearance: none;
		cursor: pointer;
		border: 1px solid var(--lens-border);
		background: var(--lens-surface);
		color: var(--lens-text-secondary);
		font-family: inherit;
		font-size: var(--lens-font-size-2xs);
		padding: 2px 8px;
		border-radius: var(--lens-radius-full);
		transition:
			color var(--lens-dur-fast) var(--lens-ease),
			background var(--lens-dur-fast) var(--lens-ease),
			border-color var(--lens-dur-fast) var(--lens-ease);
	}
	button:hover {
		color: var(--lens-text-strong);
		border-color: var(--lens-border-strong);
	}
	button.active {
		background: var(--lens-accent-surface);
		border-color: var(--lens-accent-border);
		color: var(--lens-accent);
	}
	button:focus-visible {
		outline: 2px solid var(--lens-focus);
		outline-offset: 2px;
	}
	.val {
		max-width: 22ch;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.count {
		font-variant-numeric: tabular-nums;
		color: var(--lens-text-faint);
	}
	button.active .count {
		color: var(--lens-accent);
	}
</style>
