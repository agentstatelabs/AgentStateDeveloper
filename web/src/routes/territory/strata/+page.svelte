<script lang="ts">
	/**
	 * STRATA — geological cross-section where the agent's REASONING is the
	 * terrain. Columns are regions (same seeded order as the archipelago, so
	 * spatial memory transfers); depth is time. Each week of work deposits a
	 * sediment band containing that period's judgment as fossils:
	 *
	 *   decisions        → pebbles (settled sediment)
	 *   hypotheses       → crystals, opacity = confidence
	 *   mental models    → bright mineral veins (they span symbols)
	 *   failed attempts  → dark ore shards (negative evidence, load-bearing)
	 *   open questions   → gas pockets (unresolved — they stay hollow)
	 *   hazards / bugs   → fault lines cutting DOWN through younger layers
	 *
	 * The surface is today. Bedrock is code no judgment has touched yet —
	 * deliberately visible: at 9.6k symbols, what the agent HASN'T thought
	 * about is part of the picture.
	 */
	import {
		loadTerritory,
		freshness,
		THINKING_KINDS,
		HAZARD_KINDS,
		THINKING_COLORS,
		type TerritoryData,
		type Region,
		type Entry
	} from '$lib/territory/data';
	import { placeRegions, hashString, rng } from '$lib/territory/layout';
	import LayerPicker, { type Layers } from '../LayerPicker.svelte';
	import RegionCard from '../RegionCard.svelte';
	import EntryTip from '../EntryTip.svelte';
	import DrillDown from '../DrillDown.svelte';

	let data = $state<TerritoryData | null>(null);
	let progress = $state('loading…');
	let error = $state<string | null>(null);

	let layers = $state<Layers>({
		structure: true,
		decisions: true,
		thinking: true,
		effects: true,
		activity: true
	});

	interface Col {
		region: Region;
		x: number;
		w: number;
	}
	interface Week {
		start: number; // ms epoch
		label: string;
		y: number;
		h: number;
		total: number;
	}
	interface Fossil {
		entry: Entry;
		x: number;
		y: number;
		col: Col;
	}

	let cols = $state<Col[]>([]);
	let weeks = $state<Week[]>([]);
	let fossils = $state<Fossil[]>([]);
	let faults = $state<{ entry: Entry; x: number; y0: number; y1: number; path: string }[]>([]);
	let veins = $state<{ entry: Entry; x0: number; x1: number; y: number }[]>([]);
	let totalW = $state(1200);
	let totalH = $state(600);
	const MARGIN_L = 86; // left gutter for the time axis
	// Header must hold the full rise of the rotated column labels: the longest
	// names climb ~90px above their anchor at -24°, and the old 74px header
	// clipped them against the toolbar. Labels anchor at HEADER_H - 44.
	const HEADER_H = 148;

	let hoverEntry = $state<Entry | null>(null);
	let hoverRegion = $state<Region | null>(null);
	let hoverWeek = $state(-1);
	let mouse = $state({ x: 0, y: 0 });
	let svgEl: SVGSVGElement | undefined = $state();

	// drill-down (shared panel across all three territory views)
	let selected = $state<Region | null>(null);
	let focusEntry = $state<string | null>(null);
	function openEntry(e: Entry) {
		const r = e.region ? data?.regionById.get(e.region) : undefined;
		if (!r) return;
		selected = r;
		focusEntry = e.entry_id;
	}
	function openRegion(r: Region) {
		selected = r;
		focusEntry = null;
	}
	const hoverCol = $derived(hoverRegion ? (cols.find((c) => c.region === hoverRegion) ?? null) : null);

	const WEEK = 7 * 86_400_000;

	$effect(() => {
		loadTerritory((m) => (progress = m))
			.then((d) => {
				data = d;
				build(d);
			})
			.catch((e) => (error = String(e)));
	});

	function build(d: TerritoryData) {
		// Columns in archipelago x-order — spatial memory carries over.
		const order = placeRegions(d.regions)
			.sort((a, b) => a.x - b.x || a.region.id.localeCompare(b.region.id))
			.map((p) => p.region);
		let x = MARGIN_L;
		const cs: Col[] = [];
		for (const r of order) {
			const w = 26 + Math.sqrt(r.symbolCount) * 1.35;
			cs.push({ region: r, x, w });
			x += w + 3;
		}
		totalW = x + 20;

		// Weeks, newest first (surface at top).
		const dated = d.entries.filter((e) => e.region);
		const oldest = dated.length
			? Math.min(...dated.map((e) => Date.parse(e.created_at)))
			: Date.now() - WEEK;
		const now = Date.now();
		const nWeeks = Math.max(1, Math.ceil((now - oldest) / WEEK) + 1);
		const weekIdx = (t: number) => Math.min(nWeeks - 1, Math.floor((now - t) / WEEK));

		const perWeek: Entry[][] = Array.from({ length: nWeeks }, () => []);
		for (const e of dated) perWeek[weekIdx(Date.parse(e.created_at))].push(e);

		let y = HEADER_H;
		const ws: Week[] = [];
		for (let i = 0; i < nWeeks; i++) {
			const total = perWeek[i].length;
			const h = total === 0 ? 10 : Math.min(64, 22 + total * 2.4);
			const start = now - (i + 1) * WEEK;
			ws.push({
				start,
				label: new Date(start + WEEK).toISOString().slice(0, 10),
				y,
				h,
				total
			});
			y += h + 2;
		}
		totalH = y + 60;

		// Fossils / faults / veins.
		const fs: Fossil[] = [];
		const fl: typeof faults = [];
		const vn: typeof veins = [];
		const colOf = new Map(cs.map((c) => [c.region.id, c]));
		const cellCount = new Map<string, number>();
		for (const e of dated) {
			const col = colOf.get(e.region!);
			if (!col) continue;
			const wi = weekIdx(Date.parse(e.created_at));
			const wk = ws[wi];
			const key = `${e.region}|${wi}`;
			const k = cellCount.get(key) ?? 0;
			cellCount.set(key, k + 1);
			const rand = rng(hashString(e.entry_id));
			const fx = col.x + 8 + ((k * 13 + rand() * 9) % Math.max(10, col.w - 16));
			const fy = wk.y + 7 + rand() * Math.max(4, wk.h - 14);
			if (HAZARD_KINDS.has(e.kind)) {
				// fault line cuts down through the two younger (upper) bands
				const yTop = ws[Math.max(0, wi - 2)].y + 4;
				const yBot = wk.y + wk.h - 2;
				let p = `M ${fx.toFixed(1)} ${yTop.toFixed(1)}`;
				const segs = 5;
				for (let s = 1; s <= segs; s++) {
					const yy = yTop + ((yBot - yTop) * s) / segs;
					const xx = fx + (rand() - 0.5) * 10;
					p += ` L ${xx.toFixed(1)} ${yy.toFixed(1)}`;
				}
				fl.push({ entry: e, x: fx, y0: yTop, y1: yBot, path: p });
			} else if (e.kind === 'mental_model') {
				vn.push({ entry: e, x0: col.x + 3, x1: col.x + col.w - 3, y: fy });
			} else {
				fs.push({ entry: e, x: fx, y: fy, col });
			}
		}
		cols = cs;
		weeks = ws;
		fossils = fs;
		faults = fl;
		veins = vn;
	}

	function fossilOpacity(e: Entry): number {
		if (typeof e.confidence === 'number') return 0.25 + e.confidence * 0.75;
		return 0.9;
	}
	function onMove(ev: MouseEvent) {
		mouse = { x: ev.clientX, y: ev.clientY };
		// hovered time-band: svg renders 1:1 (width/height attrs == viewBox),
		// so client offsets map straight to strata coordinates.
		if (svgEl) {
			const r = svgEl.getBoundingClientRect();
			const y = ev.clientY - r.top;
			hoverWeek = weeks.findIndex((w) => y >= w.y && y <= w.y + w.h);
		}
	}
	const isThinking = (e: Entry) => THINKING_KINDS.has(e.kind);
	const fmt = (n: number) => n.toLocaleString();
</script>

<svelte:head><title>Territory · Strata</title></svelte:head>

<div class="wrap">
	<div class="bar">
		<div class="crumbs"><a href="/territory">territory</a> / <strong>strata</strong></div>
		<LayerPicker bind:layers />
		{#if data}
			<div class="meta">
				{fmt(data.symbolCount)} symbols · {cols.length} regions · {weeks.length} weeks of judgment ·
				load {(data.loadMs / 1000).toFixed(2)}s
			</div>
		{:else}
			<div class="meta">{error ?? progress}</div>
		{/if}
	</div>

	{#if data}
		<div class="stage">
		<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
		<div class="scroller" onmousemove={onMove} onmouseleave={() => (hoverWeek = -1)} role="presentation">
			<svg bind:this={svgEl} width={totalW} height={totalH} viewBox="0 0 {totalW} {totalH}" role="img" aria-label="Strata cross-section">
				<defs>
					<linearGradient id="sky" x1="0" y1="0" x2="0" y2="1">
						<stop offset="0%" stop-color="#0b1522" />
						<stop offset="100%" stop-color="#080b12" />
					</linearGradient>
					<filter id="sglow" x="-60%" y="-60%" width="220%" height="220%">
						<feGaussianBlur stdDeviation="4" />
					</filter>
				</defs>
				<rect width={totalW} height={totalH} fill="url(#sky)" />

				<!-- surface line = today -->
				<text x={MARGIN_L} y={HEADER_H - 26} class="axis">SURFACE — today</text>
				<line x1="0" y1={HEADER_H - 2} x2={totalW} y2={HEADER_H - 2} stroke="#33405288" stroke-width="1.5" />

				<!-- column headers + activity surface glow + effects bedrock heat -->
				{#each cols as col (col.region.id)}
					{@const fresh = freshness(col.region.recencyDays)}
					<!-- svelte-ignore a11y_click_events_have_key_events -->
					<g
						class="col"
						onpointerenter={() => (hoverRegion = col.region)}
						onpointerleave={() => (hoverRegion = null)}
						onclick={() => openRegion(col.region)}
						role="button"
						tabindex="0"
						aria-label="drill into {col.region.name}"
					>
						<text
							x={col.x + col.w / 2}
							y={HEADER_H - 44}
							class="colname"
							transform="rotate(-24 {col.x + col.w / 2} {HEADER_H - 44})">{col.region.name}</text
						>
						<rect
							x={col.x}
							y={HEADER_H - 2}
							width={col.w}
							height={totalH - HEADER_H - 40}
							fill="hsl({hashString(col.region.group) % 360} 12% {layers.structure ? 13 : 9}%)"
							stroke="#00000088"
							stroke-width="1"
						/>
						{#if layers.activity && fresh > 0.02}
							<rect x={col.x} y={HEADER_H - 4} width={col.w} height="5" fill="#ebcb8b" opacity={fresh * 0.9} filter="url(#sglow)" />
						{/if}
						{#if layers.effects && col.region.riskNorm > 0.02}
							<rect
								x={col.x}
								y={totalH - 58}
								width={col.w}
								height="18"
								fill="#e0916c"
								opacity={0.1 + col.region.riskNorm * 0.65}
							/>
						{/if}
					</g>
				{/each}
				<text x={MARGIN_L} y={totalH - 44} class="axis">BEDROCK — declared side-effect heat; unexcavated code below</text>

				<!-- sediment bands -->
				{#each weeks as wk, wi (wk.start)}
					<g>
						{#if wi % 2 === 0}
							<rect x={MARGIN_L - 8} y={wk.y} width={totalW - MARGIN_L} height={wk.h} fill="#ffffff" opacity="0.022" />
						{/if}
						<line x1={MARGIN_L - 8} y1={wk.y - 1} x2={totalW} y2={wk.y - 1} stroke="#000" opacity="0.6" />
						<text x="8" y={wk.y + Math.min(14, wk.h - 2)} class="wklabel">{wk.label}</text>
						{#if wk.total > 0 && wk.h >= 26}
							<text x="8" y={wk.y + 26} class="wkcount">{wk.total} entries</text>
						{/if}
					</g>
				{/each}

				<!-- hover highlights: time-band across all columns + hovered column -->
				{#if hoverWeek >= 0 && weeks[hoverWeek]}
					<rect
						class="hl"
						x={MARGIN_L - 8}
						y={weeks[hoverWeek].y}
						width={totalW - MARGIN_L}
						height={weeks[hoverWeek].h}
						fill="#8be9c3"
						opacity="0.055"
					/>
				{/if}
				{#if hoverCol}
					<rect
						class="hl"
						x={hoverCol.x}
						y={HEADER_H - 2}
						width={hoverCol.w}
						height={totalH - HEADER_H - 40}
						fill="#ffffff"
						opacity="0.045"
					/>
				{/if}

				<!-- mental-model veins -->
				{#if layers.thinking}
					{#each veins as v (v.entry.entry_id)}
						<!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
						<line
							class="fossil"
							x1={v.x0}
							y1={v.y}
							x2={v.x1}
							y2={v.y}
							stroke={THINKING_COLORS.mental_model}
							stroke-width="3"
							opacity={fossilOpacity(v.entry)}
							filter="url(#sglow)"
							onpointerenter={() => (hoverEntry = v.entry)}
							onpointerleave={() => (hoverEntry = null)}
							onclick={() => openEntry(v.entry)}
						/>
					{/each}
				{/if}

				<!-- fossils -->
				{#each fossils as f (f.entry.entry_id)}
					{#if (isThinking(f.entry) && layers.thinking) || (!isThinking(f.entry) && layers.decisions)}
						<!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
						<g
							class="fossil"
							transform="translate({f.x} {f.y})"
							opacity={fossilOpacity(f.entry)}
							onpointerenter={() => (hoverEntry = f.entry)}
							onpointerleave={() => (hoverEntry = null)}
							onclick={() => openEntry(f.entry)}
						>
							{#if f.entry.kind === 'hypothesis'}
								<path d="M 0 -4.6 L 4 0 L 0 4.6 L -4 0 Z" fill={THINKING_COLORS.hypothesis} />
							{:else if f.entry.kind === 'failed_attempt'}
								<path d="M -3.6 -3.6 L 3.6 3.6 M -3.6 3.6 L 3.6 -3.6" stroke={THINKING_COLORS.failed_attempt} stroke-width="2.4" />
							{:else if f.entry.kind === 'open_question'}
								<circle r="4.2" fill="none" stroke={THINKING_COLORS.open_question} stroke-width="1.6" />
								<circle r="1.1" fill={THINKING_COLORS.open_question} />
							{:else if f.entry.kind === 'proof' || f.entry.kind === 'validation_scenario'}
								<rect x="-3.2" y="-3.2" width="6.4" height="6.4" fill="#a3be8c" rx="1" />
							{:else}
								<circle r="3.4" fill="#7aa2ff" />
							{/if}
						</g>
					{/if}
				{/each}

				<!-- hazard faults -->
				{#if layers.effects || layers.decisions}
					{#each faults as f (f.entry.entry_id)}
						<!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
						<path
							d={f.path}
							stroke="#e06c75"
							stroke-width="2"
							fill="none"
							class="fossil"
							onpointerenter={() => (hoverEntry = f.entry)}
							onpointerleave={() => (hoverEntry = null)}
							onclick={() => openEntry(f.entry)}
						/>
					{/each}
				{/if}
			</svg>
		</div>

		<DrillDown region={selected} focusEntryId={focusEntry} onclose={() => (selected = null)} />
		</div>

		{#if hoverEntry}
			<EntryTip entry={hoverEntry} x={mouse.x} y={mouse.y} />
		{:else if hoverRegion && !selected}
			<div class="tip cardtip" style:left="{Math.min(mouse.x + 16, window.innerWidth - 360)}px" style:top="60px">
				<RegionCard region={hoverRegion} />
			</div>
		{/if}

		<div class="legend">
			<span><i class="sq round" style:background="#7aa2ff"></i> decision pebble</span>
			<span><i class="di"></i> hypothesis crystal (opacity = confidence)</span>
			<span><i class="sq" style:background={THINKING_COLORS.mental_model}></i> mental-model vein</span>
			<span style:color={THINKING_COLORS.failed_attempt}>✕ failed attempt</span>
			<span style:color={THINKING_COLORS.open_question}>◌ open question (gas pocket)</span>
			<span style:color="#e06c75">⚡ hazard fault (cuts younger layers)</span>
			<span class="hint">surface = today · each band = one week · hover anything · click to drill down</span>
		</div>
	{:else}
		<div class="loading">{error ?? progress}</div>
	{/if}
</div>

<style>
	.wrap {
		margin: -24px;
		height: calc(100vh - 42px);
		display: flex;
		flex-direction: column;
		background: #080b12;
	}
	.bar {
		display: flex;
		align-items: center;
		gap: 18px;
		padding: 8px 14px;
		border-bottom: 1px solid var(--border);
		background: var(--bg-alt);
		flex-wrap: wrap;
	}
	.crumbs {
		font-size: 12px;
		color: var(--fg-dim);
	}
	.crumbs a:hover {
		color: var(--accent);
	}
	.crumbs strong {
		color: var(--fg);
	}
	.meta {
		margin-left: auto;
		font-size: 11px;
		color: var(--fg-dim);
	}
	.stage {
		position: relative;
		display: flex;
		flex-direction: column;
		flex: 1;
		min-height: 0;
	}
	.scroller {
		flex: 1;
		overflow: auto;
		min-height: 0;
	}
	.hl {
		pointer-events: none;
		transition: y 100ms ease, height 100ms ease;
	}
	.col {
		cursor: pointer;
	}
	.axis {
		font-size: 10px;
		fill: #5c6675;
		letter-spacing: 0.12em;
	}
	.colname {
		font-size: 10px;
		fill: #8b95a6;
		text-anchor: start;
	}
	.wklabel {
		font-size: 9px;
		fill: #5c6675;
	}
	.wkcount {
		font-size: 8px;
		fill: #3d4654;
	}
	.fossil {
		cursor: pointer;
		transition: filter 120ms ease;
	}
	.fossil:hover {
		filter: brightness(1.5);
	}
	.tip {
		position: fixed;
		z-index: 30;
		max-width: 360px;
		pointer-events: none;
	}
	.cardtip {
		background: none;
		border: none;
		padding: 0;
		box-shadow: none;
	}
	.legend {
		display: flex;
		gap: 16px;
		flex-wrap: wrap;
		padding: 7px 14px;
		border-top: 1px solid var(--border);
		background: var(--bg-alt);
		font-size: 10px;
		color: var(--fg-dim);
	}
	.legend .sq {
		display: inline-block;
		width: 8px;
		height: 8px;
		margin-right: 4px;
		vertical-align: -1px;
	}
	.legend .sq.round {
		border-radius: 50%;
	}
	.legend .di {
		display: inline-block;
		width: 7px;
		height: 7px;
		background: #8be9c3;
		transform: rotate(45deg);
		margin-right: 5px;
	}
	.legend .hint {
		margin-left: auto;
		color: var(--lens-text-faint);
	}
	.loading {
		display: flex;
		flex: 1;
		align-items: center;
		justify-content: center;
		color: var(--fg-dim);
	}
</style>
