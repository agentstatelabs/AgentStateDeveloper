<script lang="ts">
	import { loadTerritory, type TerritoryData } from '$lib/territory/data';

	let data = $state<TerritoryData | null>(null);
	let progress = $state('loading…');
	$effect(() => {
		loadTerritory((m) => (progress = m)).then((d) => (data = d)).catch(() => {});
	});

	const variants = [
		{
			href: '/territory/archipelago',
			name: 'Archipelago',
			tag: 'deterministic 2.5D island map · SVG, zero deps',
			blurb:
				'Every region is an island whose position is a pure function of its name — the same repo yields the same map on every load, so you build spatial memory. Area = symbol count, coastline complexity = kind diversity. Decisions are cairns and a lighthouse, thinking is a confidence-weighted aurora, risk is a reef ring, recent activity glows on the shoreline.'
		},
		{
			href: '/territory/globe',
			name: 'Globe',
			tag: '3D planet · Three.js (prototype-only dep)',
			blurb:
				'The same seeded layout wrapped onto a sphere: feature groups become continents in stable longitude bands. Decision beacons rise from the surface, thinking auroras shimmer above active continents, risk heat tints the crust, and fresh work shows as night-lights. Orbit, hover, click to pin.'
		},
		{
			href: '/territory/strata',
			name: 'Strata',
			tag: 'thinking-first cross-section · the wildcard',
			blurb:
				'The most conceptually honest one: the terrain IS the agent’s reasoning, not the code. Columns are regions (same order as the archipelago); depth is time — each week of work deposits a sediment band holding its judgment as fossils: decision pebbles, hypothesis crystals (opacity = confidence), mental-model veins, failed-attempt shards, open-question gas pockets, and hazard faults that crack down through younger layers. Bedrock is code no judgment has touched yet.'
		}
	];
</script>

<svelte:head><title>Territory — spatial judgment views</title></svelte:head>

<div class="page">
	<h1>Territory</h1>
	<p class="pitch">
		A codebase rendered as stable territory instead of a force-directed hairball. Structure only
		provides the land; the point is the <em>judgment data</em> ASD accumulates — agent decisions,
		captured thinking, declared side-effects, and activity — painted as toggleable layers on a map
		that never rearranges itself. Three competing prototypes against the same live data
		{#if data}
			({data.symbolCount.toLocaleString()} ExampleProj symbols · {data.regions.length} regions ·
			{data.entries.length} ledger entries · aggregated in {(data.loadMs / 1000).toFixed(2)}s)
		{:else}
			({progress})
		{/if}
		— pick the one worth productizing.
	</p>

	<div class="tiles">
		{#each variants as v (v.href)}
			<a class="tile" href={v.href}>
				<div class="tname">{v.name}</div>
				<div class="ttag">{v.tag}</div>
				<p>{v.blurb}</p>
				<span class="go">open →</span>
			</a>
		{/each}
	</div>

	<h2>Evaluation notes</h2>
	<table>
		<thead>
			<tr>
				<th></th>
				<th>Archipelago</th>
				<th>Globe</th>
				<th>Strata</th>
			</tr>
		</thead>
		<tbody>
			<tr>
				<td>Determinism</td>
				<td>Bit-identical across reloads; layout fingerprint shown in the toolbar as proof.</td>
				<td>Same seeded layout, but camera/autorotate state is session-local — the shape is stable, the view isn’t.</td>
				<td>Fully deterministic (column order reuses the archipelago layout; fossil jitter is seeded by entry id).</td>
			</tr>
			<tr>
				<td>Legibility at 9.6k symbols</td>
				<td>Good — 30 regions, labels always readable, zoom for detail. Nothing overlaps by construction.</td>
				<td>Half the planet is always hidden; labels are hard in 3D so identification leans on the hover card.</td>
				<td>Best judgment-per-pixel: sparse regions visibly stay “unexcavated”, which is honest at this scale.</td>
			</tr>
			<tr>
				<td>Layer expressiveness</td>
				<td>All five layers visible simultaneously without mutual occlusion; aurora + reef + shoreline compose well.</td>
				<td>Beacons/aurora/night-lights read beautifully; risk heat weakest (texture tint).</td>
				<td>Thinking is structurally privileged — confidence, failure, and open questions are first-class marks, and TIME exists (no other variant shows it).</td>
			</tr>
			<tr>
				<td>Wow factor</td>
				<td>High; reads instantly as “a map of my code”.</td>
				<td>Highest for a hero shot / landing page loop.</td>
				<td>Highest for the technical audience; needs one sentence of explanation first.</td>
			</tr>
			<tr>
				<td>Implementation cost</td>
				<td>Low — one SVG file, no deps, hand-rolled pan/zoom.</td>
				<td>Highest — Three.js dep (app-only), canvas texture pipeline, picking, disposal.</td>
				<td>Low-medium — one SVG, but the bucketing/fossil vocabulary needs product decisions.</td>
			</tr>
			<tr>
				<td>Honest caveats</td>
				<td rowspan="1" colspan="3" class="caveat">
					Data notes for all three: the golden ExampleProj DB had an index + effects but zero ledger
					entries, so the judgment history (101 entries, Mar–Jul 2026, mirroring the real M18–M25
					field-eval arcs) was seeded into a scratch copy via the sidecar-hydrate path; four
					thinking entries were captured live with <code>asd think</code>. Symbols and per-symbol
					effects are served from setup-time snapshots because <code>/api/v1/symbols</code> resolves
					each qname individually (measured 8m45s per 2000-symbol page at this scale) and
					<code>/api/v1/thinking</code> rescans every qname (~9m). Fixing those two endpoints is a
					prerequisite for productizing any variant.
				</td>
			</tr>
		</tbody>
	</table>
</div>

<style>
	.page {
		max-width: 980px;
		margin: 0 auto;
	}
	h1 {
		font-size: 22px;
		margin: 6px 0 10px;
	}
	.pitch {
		color: var(--fg);
		line-height: 1.55;
		font-size: 13px;
		max-width: 78ch;
	}
	.pitch em {
		color: #8be9c3;
		font-style: normal;
	}
	.tiles {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(280px, 1fr));
		gap: 14px;
		margin: 22px 0 30px;
	}
	.tile {
		display: block;
		border: 1px solid var(--border);
		border-radius: 8px;
		padding: 16px;
		background: var(--bg-alt);
		transition: border-color 120ms, transform 120ms;
	}
	.tile:hover {
		border-color: #3a4150;
		transform: translateY(-2px);
	}
	.tname {
		font-size: 16px;
		font-weight: 700;
	}
	.ttag {
		font-size: 11px;
		color: #8be9c3;
		margin: 3px 0 8px;
	}
	.tile p {
		color: var(--fg-dim);
		font-size: 12px;
		line-height: 1.5;
		margin: 0 0 10px;
	}
	.go {
		color: var(--accent);
		font-size: 12px;
	}
	h2 {
		font-size: 15px;
		margin: 24px 0 10px;
	}
	table {
		width: 100%;
		border-collapse: collapse;
		font-size: 12px;
		margin-bottom: 40px;
	}
	th,
	td {
		text-align: left;
		padding: 8px 10px;
		border: 1px solid var(--border);
		vertical-align: top;
		line-height: 1.45;
	}
	th {
		background: var(--bg-alt);
	}
	td:first-child {
		color: var(--fg-dim);
		white-space: nowrap;
	}
	.caveat {
		color: var(--fg-dim);
	}
</style>
