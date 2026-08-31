<!--
	/health — how good is the record?

	/records searches what ASD knows; this page asks whether that knowledge is
	worth trusting. Two halves:

	  · Scorecard — the five capability dimensions, the ledger-density caveat
	    that says whether the scores measure workflow quality or just data
	    sparsity, and the token-economy estimate.
	  · Index — freshness and the ASG↔FTS consistency check, i.e. whether the
	    numbers on every other page describe the codebase as it stands now.

	The drill-down is searchable: at 2,952 symbols, "which ones are the gap?"
	is only answerable if you can filter the list.
-->
<script lang="ts">
	import { BarChart, StatTile } from '@agentstate/lens-core';
	import {
		getScorecard,
		getIndexHealth,
		fmtBytes,
		fmtNum,
		type Scorecard,
		type IndexHealth,
		type ScoreSet
	} from '$lib/metrics';

	let card = $state<Scorecard | null>(null);
	let index = $state<IndexHealth | null>(null);
	let err = $state<string | null>(null);
	let loading = $state(true);

	/** Which dimension's per-symbol gap list to pull, or '' for none. */
	let drill = $state('');
	let drillFilter = $state('');
	let drillLoading = $state(false);

	const DIMENSIONS: { key: keyof ScoreSet; label: string; blurb: string; drillable: boolean }[] = [
		{
			key: 'truth',
			label: 'Truth',
			blurb: 'Symbols with verified effects and an ownership entry.',
			drillable: true
		},
		{
			key: 'feedback',
			label: 'Feedback',
			blurb: 'Recorded search verdicts; 50 entries reads as full marks.',
			drillable: false
		},
		{
			key: 'change',
			label: 'Change',
			blurb: 'Symbols carrying an invariant or a validation scenario.',
			drillable: true
		},
		{
			key: 'uncertainty',
			label: 'Uncertainty',
			blurb: 'Index-health proxy: symbol volume plus effect-verification rate.',
			drillable: true
		},
		{
			key: 'workflow',
			label: 'Workflow',
			blurb: 'Ledger density, weighted with CTX-tagged adoption.',
			drillable: true
		}
	];

	let seq = 0;

	$effect(() => {
		// `drill` is the ONLY reactive read in here. Touching `card` — which
		// this effect also writes — makes it re-trigger itself and refetch
		// forever.
		const which = drill;
		const mine = ++seq;
		drillLoading = which !== '';
		Promise.all([
			getScorecard(which ? { drillDown: which, limit: 2000 } : {}),
			getIndexHealth().catch(() => null)
		])
			.then(([c, i]) => {
				if (mine !== seq) return;
				card = c;
				if (i) index = i;
			})
			.catch((e) => {
				if (mine === seq) err = e instanceof Error ? e.message : String(e);
			})
			.finally(() => {
				if (mine === seq) {
					loading = false;
					drillLoading = false;
				}
			});
	});

	let scores = $derived(card?.capability_scores ?? card?.scores ?? null);

	let scoreBars = $derived(
		scores
			? DIMENSIONS.map((d) => ({
					label: d.label,
					value: scores![d.key],
					// Red below 25, amber below 60 — a scorecard that paints a 1/100
					// the same color as an 80/100 isn't telling you anything.
					color:
						scores![d.key] < 25
							? 'var(--lens-danger)'
							: scores![d.key] < 60
								? 'var(--lens-warn)'
								: 'var(--lens-ok)'
				}))
			: []
	);

	let gaps = $derived(card?.drill_down?.gap_symbols ?? []);
	let filteredGaps = $derived(
		drillFilter.trim()
			? gaps.filter((g) => {
					const n = drillFilter.trim().toLowerCase();
					return g.qname.toLowerCase().includes(n) || g.file.toLowerCase().includes(n);
				})
			: gaps
	);

	function toggleDrill(dim: string) {
		drillFilter = '';
		drill = drill === dim ? '' : dim;
	}
</script>

<svelte:head>
	<title>Health — ASD Lens</title>
</svelte:head>

<header class="page-header">
	<h1>Health</h1>
	<p class="page-desc">
		Whether the record on <a href="/records">Records</a> and <a href="/history">History</a> is worth
		trusting — how completely the codebase is annotated, and whether the index still describes it.
	</p>
</header>

{#if err}
	<div class="state-error">{err}</div>
{:else if loading}
	<div class="state-loading">loading…</div>
{:else if card && scores}
	{#if card.note}
		<div class="banner" data-tone="warn">{card.note}</div>
	{/if}

	<section>
		<h2>Capability scorecard</h2>
		{#if card.data_quality?.sparse_db}
			<div class="banner" data-tone="warn">
				<strong>Scores reflect data density, not workflow quality.</strong>
				{card.data_quality.note}
			</div>
		{/if}

		<div class="tiles">
			<StatTile
				label="Overall"
				value={scores.overall}
				unit="/100"
				accent
				title="Mean of the five dimensions"
			/>
			{#if card.data_quality}
				<StatTile
					label="Ledger coverage"
					value={card.data_quality.coverage_pct}
					unit="%"
					title="{fmtNum(card.data_quality.symbols_with_any_ledger)} of {fmtNum(
						card.data_quality.symbols_scored
					)} symbols carry at least one ledger entry"
				/>
				<StatTile
					label="Ledger density"
					value={card.data_quality.ledger_density.toFixed(2)}
					unit="/sym"
					title="Entries per symbol"
				/>
			{/if}
			{#if card.token_economy}
				<StatTile
					label="Token economy"
					value={card.token_economy.ratio_x.toFixed(1)}
					unit="x"
					title={card.token_economy.note}
				/>
			{/if}
		</div>

		<div class="split">
			<div class="chart-card">
				<BarChart data={scoreBars} max={100} labelWidth={104} ariaLabel="Capability scores" />
			</div>
			<ul class="dimension-list">
				{#each DIMENSIONS as d (d.key)}
					<li>
						<div class="dim-head">
							<span class="dim-label">{d.label}</span>
							<span class="dim-score">{scores[d.key]}<span class="of">/100</span></span>
							{#if d.drillable}
								<button
									type="button"
									class:active={drill === d.key}
									onclick={() => toggleDrill(d.key)}
								>
									{drill === d.key ? 'hide gaps' : 'show gaps'}
								</button>
							{/if}
						</div>
						<p>{d.blurb}</p>
					</li>
				{/each}
			</ul>
		</div>

		{#if card.details}
			<dl class="details">
				<div><dt>Symbols</dt><dd>{fmtNum(card.details.total_symbols)}</dd></div>
				<div><dt>Verified effects</dt><dd>{fmtNum(card.details.verified_effects)}</dd></div>
				<div><dt>Owned</dt><dd>{fmtNum(card.details.owned_symbols)}</dd></div>
				<div><dt>Invariants</dt><dd>{fmtNum(card.details.invariant_symbols)}</dd></div>
				<div><dt>Validation scenarios</dt><dd>{fmtNum(card.details.validation_symbols)}</dd></div>
				<div><dt>Ledger entries</dt><dd>{fmtNum(card.details.total_ledger_entries)}</dd></div>
				<div><dt>CTX-tagged</dt><dd>{fmtNum(card.details.ctx_tagged_ledger_entries)}</dd></div>
				<div><dt>Feedback</dt><dd>{fmtNum(card.details.feedback_entries)}</dd></div>
			</dl>
		{/if}

		{#if card.token_economy}
			<p class="footnote">
				<strong>Token economy</strong> — {fmtNum(card.token_economy.structured_tokens)} index tokens
				against an estimated {fmtNum(card.token_economy.source_read_tokens_est)} to read the same
				files ({card.token_economy.reduction_pct}% less).
				{card.token_economy.note}
			</p>
		{/if}
	</section>

	{#if drill}
		<section>
			<h2>
				{drill} gaps
				{#if card.drill_down}
					<span class="count">
						{fmtNum(card.drill_down.total_gaps)} symbols
						{#if card.drill_down.omitted > 0}
							· showing {fmtNum(card.drill_down.shown)}
						{/if}
					</span>
				{/if}
			</h2>
			<div class="controls">
				<input
					type="search"
					placeholder="filter by qname or file…"
					bind:value={drillFilter}
				/>
				{#if drillLoading}<span class="spinner">loading…</span>{/if}
				<span class="spinner">
					{fmtNum(filteredGaps.length)} match{filteredGaps.length === 1 ? '' : 'es'}
				</span>
			</div>
			{#if card.drill_down && card.drill_down.omitted > 0}
				<p class="footnote">
					{fmtNum(card.drill_down.omitted)} further gap symbols were not returned — the API caps the
					drill-down list. Narrow the scope to see them.
				</p>
			{/if}
			{#if filteredGaps.length === 0}
				<div class="state-empty">No symbols match that filter.</div>
			{:else}
				<!-- Scroll container: SessionDrift qnames reach 199 chars, which
				     pushed this table to 1875px inside a 1280px viewport and put
				     the last three columns out of reach entirely. Wide tables must
				     scroll, not clip. -->
				<div class="table-scroll">
				<table>
					<thead>
						<tr>
							<th>symbol</th>
							<th>file</th>
							<th>effects</th>
							<th>owner</th>
							<th>invariant</th>
							<th>scenario</th>
							<th class="num">ledger</th>
						</tr>
					</thead>
					<tbody>
						{#each filteredGaps.slice(0, 500) as g (g.qname)}
							<tr>
								<td class="qname"><a href="/symbols/{encodeURIComponent(g.qname)}">{g.qname}</a></td>
								<td class="mono faint path">{g.file}</td>
								<td>{g.has_verified_effects ? '✓' : '—'}</td>
								<td>{g.has_ownership ? '✓' : '—'}</td>
								<td>{g.has_invariant ? '✓' : '—'}</td>
								<td>{g.has_validation_scenario ? '✓' : '—'}</td>
								<td class="num mono">{g.ledger_entries}</td>
							</tr>
						{/each}
					</tbody>
				</table>
				</div>
				{#if filteredGaps.length > 500}
					<p class="footnote">
						Showing the first 500 of {fmtNum(filteredGaps.length)} matching rows — narrow the filter
						to see the rest.
					</p>
				{/if}
			{/if}
		</section>
	{/if}
{/if}

{#if index}
	<section>
		<h2>Index</h2>
		{#if index.stale}
			<div class="banner" data-tone={index.stale.severity === 'critical' ? 'bad' : 'warn'}>
				{index.stale.message}
			</div>
		{/if}
		{#if index.consistency}
			<div class="banner" data-tone="warn">
				<strong>ASG and the FTS cache disagree by {fmtNum(Math.abs(index.consistency.delta))}
					symbols.</strong>
				{index.consistency.advice}
			</div>
		{:else}
			<div class="banner" data-tone="ok">
				ASG and the FTS search cache agree — {fmtNum(index.symbols.asg)} symbols on both sides.
			</div>
		{/if}

		<div class="tiles">
			<StatTile
				label="Last indexed"
				value={index.indexed_age?.human ?? '—'}
				title={index.indexed_at ? new Date(index.indexed_at * 1000).toISOString() : 'never indexed'}
			/>
			<StatTile label="Symbols (ASG)" value={fmtNum(index.symbols.asg)} />
			<StatTile label="Symbols (FTS)" value={fmtNum(index.symbols.fts)} />
			<StatTile
				label="Annotated"
				value={fmtNum(index.symbols.annotated)}
				title="Symbols carrying at least one ledger annotation"
			/>
			<StatTile label="Store" value={fmtBytes(index.db_bytes)} />
		</div>

		<dl class="details">
			<div><dt>Database</dt><dd class="mono">{index.db_path}</dd></div>
			<div><dt>Ref</dt><dd class="mono">{index.ref_name}</dd></div>
			<div><dt>Feedback entries</dt><dd>{fmtNum(index.feedback_entries)}</dd></div>
		</dl>
		<p class="footnote">
			Byte-level store shape and what a sweep would reclaim live on
			<a href="/history">History → Store health</a>.
		</p>
	</section>
{/if}

<style>
	.page-header {
		margin-bottom: var(--lens-space-5);
	}
	.page-header h1 {
		margin: 0;
		font-size: var(--lens-font-size-xl);
		color: var(--lens-text-strong);
	}
	.page-desc {
		margin: var(--lens-space-2) 0 0;
		max-width: 74ch;
		color: var(--lens-text-secondary);
		font-size: var(--lens-font-size-sm);
		line-height: 1.55;
	}
	.page-desc a,
	.footnote a {
		color: var(--lens-accent);
	}

	section {
		margin-bottom: var(--lens-space-8);
	}
	section h2 {
		display: flex;
		align-items: baseline;
		gap: var(--lens-space-3);
		margin: 0 0 var(--lens-space-3);
		font-size: var(--lens-font-size-md);
		color: var(--lens-text-strong);
		text-transform: capitalize;
	}
	section h2 .count {
		font-size: var(--lens-font-size-2xs);
		font-weight: 400;
		text-transform: none;
		color: var(--lens-muted);
	}

	.banner {
		margin-bottom: var(--lens-space-3);
		padding: var(--lens-space-3);
		border-radius: var(--lens-radius-md);
		font-size: var(--lens-font-size-2xs);
		line-height: 1.55;
		color: var(--lens-text);
	}
	.banner[data-tone='warn'] {
		background: var(--lens-warn-tint);
		border: 1px solid var(--lens-warn-border);
	}
	.banner[data-tone='bad'] {
		background: var(--lens-danger-tint);
		border: 1px solid var(--lens-danger-border);
	}
	.banner[data-tone='ok'] {
		background: var(--lens-ok-tint);
		border: 1px solid var(--lens-ok-border);
	}

	.tiles {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
		gap: var(--lens-space-3);
		margin-bottom: var(--lens-space-4);
	}

	.split {
		display: grid;
		grid-template-columns: minmax(280px, 1fr) minmax(280px, 1fr);
		gap: var(--lens-space-4);
		align-items: start;
	}
	@media (max-width: 860px) {
		.split {
			grid-template-columns: 1fr;
		}
	}
	.chart-card {
		padding: var(--lens-space-4);
		border: 1px solid var(--lens-border);
		border-radius: var(--lens-radius-md);
		background: var(--lens-surface);
	}

	.dimension-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: var(--lens-space-2);
	}
	.dimension-list li {
		padding: var(--lens-space-2) var(--lens-space-3);
		border: 1px solid var(--lens-border);
		border-radius: var(--lens-radius-md);
		background: var(--lens-surface);
	}
	.dim-head {
		display: flex;
		align-items: baseline;
		gap: var(--lens-space-2);
	}
	.dim-label {
		font-weight: 600;
		color: var(--lens-text-strong);
		font-size: var(--lens-font-size-xs);
	}
	.dim-score {
		font-variant-numeric: tabular-nums;
		color: var(--lens-text);
		font-size: var(--lens-font-size-xs);
	}
	.dim-score .of {
		color: var(--lens-text-faint);
	}
	.dim-head button {
		margin-left: auto;
		appearance: none;
		cursor: pointer;
		border: 1px solid var(--lens-border);
		background: var(--lens-surface-raised);
		color: var(--lens-text-secondary);
		font-family: inherit;
		font-size: var(--lens-font-size-2xs);
		padding: 2px 9px;
		border-radius: var(--lens-radius-full);
	}
	.dim-head button:hover {
		color: var(--lens-text-strong);
		border-color: var(--lens-border-strong);
	}
	.dim-head button.active {
		background: var(--lens-accent-surface);
		border-color: var(--lens-accent-border);
		color: var(--lens-accent);
	}
	.dim-head button:focus-visible {
		outline: 2px solid var(--lens-focus);
		outline-offset: 2px;
	}
	.dimension-list p {
		margin: 4px 0 0;
		font-size: var(--lens-font-size-2xs);
		color: var(--lens-muted);
		line-height: 1.5;
	}

	.details {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
		gap: var(--lens-space-2) var(--lens-space-4);
		margin: var(--lens-space-4) 0 0;
	}
	.details div {
		display: flex;
		flex-direction: column;
		gap: 1px;
		min-width: 0;
	}
	.details dt {
		font-size: var(--lens-font-size-2xs);
		text-transform: uppercase;
		letter-spacing: var(--lens-tracking-caps);
		color: var(--lens-text-faint);
	}
	.details dd {
		margin: 0;
		font-size: var(--lens-font-size-xs);
		color: var(--lens-text);
		font-variant-numeric: tabular-nums;
		overflow-wrap: anywhere;
	}

	.footnote {
		margin: var(--lens-space-3) 0 0;
		max-width: 84ch;
		font-size: var(--lens-font-size-2xs);
		color: var(--lens-text-faint);
		line-height: 1.55;
	}

	.controls {
		display: flex;
		gap: 10px;
		align-items: center;
		margin-bottom: var(--lens-space-3);
		font-size: var(--lens-font-size-2xs);
		color: var(--lens-muted);
	}
	.controls input {
		flex: 1 1 260px;
		max-width: 420px;
		font-family: inherit;
		font-size: var(--lens-font-size-2xs);
		color: var(--lens-text);
		background: var(--lens-surface);
		border: 1px solid var(--lens-border);
		border-radius: var(--lens-radius-sm);
		padding: 5px 9px;
	}
	.controls input:focus-visible {
		outline: 2px solid var(--lens-focus);
		outline-offset: 1px;
	}
	.spinner {
		color: var(--lens-text-faint);
		white-space: nowrap;
	}

	.table-scroll {
		overflow-x: auto;
		max-width: 100%;
	}
	table {
		width: 100%;
		border-collapse: collapse;
		font-size: var(--lens-font-size-2xs);
	}
	/* Cap the two unbounded columns so the common case still fits the
	   viewport; the scroll container handles whatever exceeds it. */
	td.qname,
	td.path {
		max-width: 44ch;
		overflow-wrap: anywhere;
	}
	thead th {
		text-align: left;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: var(--lens-tracking-caps);
		color: var(--lens-text-faint);
		padding: 6px 10px;
		border-bottom: 1px solid var(--lens-border);
		white-space: nowrap;
	}
	tbody td {
		padding: 6px 10px;
		border-bottom: 1px solid var(--lens-border-subtle);
		color: var(--lens-text-secondary);
	}
	tbody tr:hover td {
		background: var(--lens-surface);
	}
	td a {
		color: var(--lens-accent);
	}
	.mono {
		font-family: var(--lens-font-mono);
	}
	.faint {
		color: var(--lens-text-faint);
	}
	.num {
		text-align: right;
		font-variant-numeric: tabular-nums;
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
	}
	.state-error {
		color: var(--lens-danger);
		border-color: var(--lens-danger-border);
		background: var(--lens-danger-tint);
	}
</style>
