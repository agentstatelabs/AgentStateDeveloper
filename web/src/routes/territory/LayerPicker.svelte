<script module lang="ts">
	export interface Layers {
		structure: boolean;
		decisions: boolean;
		thinking: boolean;
		effects: boolean;
		activity: boolean;
	}
</script>

<script lang="ts">
	let {
		layers = $bindable(),
		disabled = []
	}: { layers: Layers; disabled?: (keyof Layers)[] } = $props();

	const DEFS: { key: keyof Layers; label: string; dot: string; hint: string }[] = [
		{ key: 'structure', label: 'Structure', dot: '#8690a0', hint: 'regions, size, kind mix' },
		{ key: 'decisions', label: 'Decisions', dot: '#7aa2ff', hint: 'ledger density & markers' },
		{ key: 'thinking', label: 'Thinking', dot: '#8be9c3', hint: 'hypotheses, models, questions — confidence-weighted' },
		{ key: 'effects', label: 'Effects / Risk', dot: '#e0916c', hint: 'declared side-effect heat' },
		{ key: 'activity', label: 'Activity', dot: '#ebcb8b', hint: 'recency — fades over 14 days' }
	];
</script>

<div class="layer-picker" role="group" aria-label="Data layers">
	<span class="lp-title">Layers</span>
	{#each DEFS as def (def.key)}
		<button
			class="lp-pill"
			class:on={layers[def.key]}
			class:headline={def.key === 'thinking'}
			disabled={disabled.includes(def.key)}
			title={def.hint}
			onclick={() => (layers[def.key] = !layers[def.key])}
		>
			<span class="dot" style:background={layers[def.key] ? def.dot : 'transparent'} style:border-color={def.dot}></span>
			{def.label}
		</button>
	{/each}
</div>

<style>
	.layer-picker {
		display: flex;
		align-items: center;
		gap: 6px;
		flex-wrap: wrap;
	}
	.lp-title {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.1em;
		color: var(--fg-dim);
		margin-right: 4px;
	}
	.lp-pill {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		font: inherit;
		font-size: 11px;
		color: var(--fg-dim);
		background: var(--bg-alt);
		border: 1px solid var(--border);
		border-radius: 12px;
		padding: 3px 10px;
		cursor: pointer;
		transition: color 120ms, border-color 120ms;
	}
	.lp-pill.on {
		color: var(--fg);
		border-color: #3a4150;
	}
	.lp-pill.headline.on {
		border-color: rgba(139, 233, 195, 0.45);
		box-shadow: 0 0 10px rgba(139, 233, 195, 0.12);
	}
	.lp-pill:disabled {
		opacity: 0.4;
		cursor: default;
	}
	.dot {
		width: 7px;
		height: 7px;
		border-radius: 50%;
		border: 1px solid;
		box-sizing: border-box;
	}
</style>
