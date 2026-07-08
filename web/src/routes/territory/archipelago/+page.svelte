<script lang="ts">
	import { loadTerritory, freshness, type TerritoryData, type Region } from '$lib/territory/data';
	import { placeRegions, islandPath, hashString, rng, type PlacedRegion } from '$lib/territory/layout';
	import LayerPicker, { type Layers } from '../LayerPicker.svelte';
	import RegionCard from '../RegionCard.svelte';
	import { tick } from 'svelte';

	let data = $state<TerritoryData | null>(null);
	let progress = $state('loading…');
	let error = $state<string | null>(null);
	let renderMs = $state(0);

	let layers = $state<Layers>({
		structure: true,
		decisions: true,
		thinking: true,
		effects: true,
		activity: true
	});

	let placed = $state<PlacedRegion[]>([]);
	let layoutHash = $state('');

	$effect(() => {
		const t0 = performance.now();
		loadTerritory((m) => (progress = m))
			.then((d) => {
				data = d;
				const p = placeRegions(d.regions);
				placed = p;
				// determinism receipt: hash of every island coordinate
				let h = 0x811c9dc5 >>> 0;
				for (const pr of p) {
					h = (h ^ hashString(`${pr.region.id}:${pr.x.toFixed(3)}:${pr.y.toFixed(3)}:${pr.r.toFixed(3)}`)) >>> 0;
					h = Math.imul(h, 0x01000193) >>> 0;
				}
				layoutHash = h.toString(16).padStart(8, '0');
				renderMs = performance.now() - t0;
				void fitView(p);
			})
			.catch((e) => (error = String(e)));
	});

	async function fitView(p: PlacedRegion[]) {
		await tick(); // svg mounts once data is set
		// fit viewBox to content (deterministic — derived from layout)
		const xs = p.map((pr) => pr.x);
				const ys = p.map((pr) => pr.y);
				const rs = p.map((pr) => pr.r * 1.9);
				const minX = Math.min(...xs.map((v, i) => v - rs[i])) - 40;
		const maxX = Math.max(...xs.map((v, i) => v + rs[i])) + 40;
		const minY = Math.min(...ys.map((v, i) => v - rs[i])) - 40;
		const maxY = Math.max(...ys.map((v, i) => v + rs[i])) + 60;
		let w = maxX - minX;
		let hgt = maxY - minY;
		const rect = svgEl?.getBoundingClientRect();
		const aspect = rect && rect.height > 0 ? rect.width / rect.height : 1.55;
		if (w / hgt < aspect) w = hgt * aspect;
		else hgt = w / aspect;
		vb = { x: (minX + maxX) / 2 - w / 2, y: (minY + maxY) / 2 - hgt / 2, w, h: hgt };
	}

	// ---- pan / zoom (hand-rolled viewBox) ---------------------------------
	let vb = $state({ x: -960, y: -640, w: 1920, h: 1280 });
	let svgEl: SVGSVGElement | undefined = $state();
	let dragging = false;
	let dragStart = { x: 0, y: 0, vx: 0, vy: 0 };

	function clientToMap(cx: number, cy: number): { x: number; y: number } {
		const rect = svgEl!.getBoundingClientRect();
		return {
			x: vb.x + ((cx - rect.left) / rect.width) * vb.w,
			y: vb.y + ((cy - rect.top) / rect.height) * vb.h
		};
	}
	function onWheel(ev: WheelEvent) {
		ev.preventDefault();
		const f = Math.exp(ev.deltaY * 0.0012);
		const m = clientToMap(ev.clientX, ev.clientY);
		const w = Math.min(6000, Math.max(240, vb.w * f));
		const s = w / vb.w;
		vb = { x: m.x - (m.x - vb.x) * s, y: m.y - (m.y - vb.y) * s, w, h: vb.h * s };
	}
	function onPointerDown(ev: PointerEvent) {
		dragging = true;
		dragStart = { x: ev.clientX, y: ev.clientY, vx: vb.x, vy: vb.y };
		(ev.target as Element).setPointerCapture?.(ev.pointerId);
	}
	function onPointerMove(ev: PointerEvent) {
		if (!dragging || !svgEl) return;
		const rect = svgEl.getBoundingClientRect();
		vb = {
			...vb,
			x: dragStart.vx - ((ev.clientX - dragStart.x) / rect.width) * vb.w,
			y: dragStart.vy - ((ev.clientY - dragStart.y) / rect.height) * vb.h
		};
	}
	function onPointerUp() {
		dragging = false;
	}

	// ---- hover / pin -------------------------------------------------------
	let hovered = $state<Region | null>(null);
	let pinnedR = $state<Region | null>(null);
	let shown = $derived(pinnedR ?? hovered);

	// ---- per-island render helpers ----------------------------------------
	function groupHue(group: string): number {
		return hashString(group) % 360;
	}
	function landFill(pr: PlacedRegion): string {
		const hue = groupHue(pr.region.group);
		// decision density brightens the land slightly when layer is on
		const dens = layers.decisions
			? Math.min(1, pr.region.decisions.length / Math.max(4, pr.region.symbolCount / 60))
			: 0;
		return `hsl(${hue} ${16 + dens * 10}% ${17 + dens * 8}%)`;
	}
	function highlandFill(pr: PlacedRegion): string {
		return `hsl(${groupHue(pr.region.group)} 18% 24%)`;
	}
	function auroraOpacity(r: Region): number {
		if (!r.thinking.length) return 0;
		const conf = r.meanConfidence ?? 0.5;
		return (0.25 + conf * 0.6) * Math.min(1, r.thinking.length / 3);
	}
	/** cairn positions along the coast for decision markers (seeded) */
	function cairns(pr: PlacedRegion): { x: number; y: number }[] {
		const n = Math.min(9, Math.ceil(Math.sqrt(pr.region.decisions.length)));
		if (pr.region.decisions.length === 0) return [];
		const rand = rng(hashString(pr.region.id + ':cairn'));
		const out: { x: number; y: number }[] = [];
		for (let i = 0; i < n; i++) {
			const a = rand() * Math.PI * 2;
			const rr = pr.r * (0.45 + rand() * 0.35);
			out.push({ x: Math.cos(a) * rr, y: Math.sin(a) * rr * 0.82 });
		}
		return out;
	}
	const fmt = (n: number) => n.toLocaleString();
</script>

<svelte:head><title>Territory · Archipelago</title></svelte:head>

<div class="wrap">
	<div class="bar">
		<div class="crumbs"><a href="/territory">territory</a> / <strong>archipelago</strong></div>
		<LayerPicker bind:layers />
		{#if data}
			<div class="meta">
				{fmt(data.symbolCount)} symbols · {data.regions.length} regions ·
				{data.entries.length} ledger entries · load {(data.loadMs / 1000).toFixed(2)}s ·
				layout <code title="deterministic layout fingerprint — identical on every reload">#{layoutHash}</code>
			</div>
		{:else}
			<div class="meta">{error ?? progress}</div>
		{/if}
	</div>

	<div class="map">
		{#if data}
			<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
			<svg
				bind:this={svgEl}
				viewBox="{vb.x} {vb.y} {vb.w} {vb.h}"
				onwheel={onWheel}
				onpointerdown={onPointerDown}
				onpointermove={onPointerMove}
				onpointerup={onPointerUp}
				onpointerleave={onPointerUp}
				role="img"
				aria-label="Archipelago map of {data.regions.length} code regions"
			>
				<defs>
					<radialGradient id="ocean" cx="50%" cy="45%" r="75%">
						<stop offset="0%" stop-color="#0d1420" />
						<stop offset="100%" stop-color="#080b12" />
					</radialGradient>
					<filter id="soft" x="-60%" y="-60%" width="220%" height="220%">
						<feGaussianBlur stdDeviation="6" />
					</filter>
					<filter id="softer" x="-80%" y="-80%" width="260%" height="260%">
						<feGaussianBlur stdDeviation="14" />
					</filter>
				</defs>

				<rect x={vb.x} y={vb.y} width={vb.w} height={vb.h} fill="url(#ocean)" />
				<!-- graticule -->
				<g stroke="#141a26" stroke-width="1">
					{#each Array.from({ length: 29 }, (_, i) => -1400 + i * 100) as gx (gx)}
						<line x1={gx} y1="-1400" x2={gx} y2="1400" />
					{/each}
					{#each Array.from({ length: 29 }, (_, i) => -1400 + i * 100) as gy (gy)}
						<line x1="-1400" y1={gy} x2="1400" y2={gy} />
					{/each}
				</g>

				{#each placed as pr (pr.region.id)}
					{@const seed = hashString(pr.region.id)}
					{@const fresh = freshness(pr.region.recencyDays)}
					<!-- svelte-ignore a11y_click_events_have_key_events -->
					<g
						class="island"
						transform="translate({pr.x} {pr.y})"
						onpointerenter={() => (hovered = pr.region)}
						onpointerleave={() => (hovered = null)}
						onclick={(e) => {
							e.stopPropagation();
							pinnedR = pinnedR === pr.region ? null : pr.region;
						}}
						role="button"
						tabindex="0"
						aria-label={pr.region.name}
					>
						<!-- thinking aurora (headline layer) -->
						{#if layers.thinking && pr.region.thinking.length}
							<path
								d={islandPath(pr.r, seed, pr.region.kindDiversity, 1.7, -6)}
								fill="none"
								stroke="#8be9c3"
								stroke-width={5 + (pr.region.meanConfidence ?? 0.5) * 7}
								opacity={auroraOpacity(pr.region)}
								filter="url(#softer)"
							/>
							<path
								d={islandPath(pr.r, seed + 7, pr.region.kindDiversity, 1.45, -10)}
								fill="none"
								stroke="#7aa2ff"
								stroke-width="3"
								opacity={auroraOpacity(pr.region) * 0.5}
								filter="url(#softer)"
							/>
						{/if}

						<!-- risk reef -->
						{#if layers.effects && pr.region.riskNorm > 0.02}
							<path
								d={islandPath(pr.r, seed + 3, pr.region.kindDiversity, 1.32)}
								fill="none"
								stroke="#e0916c"
								stroke-width={1.5 + pr.region.riskNorm * 3}
								stroke-dasharray="3 5"
								opacity={0.15 + pr.region.riskNorm * 0.7}
							/>
						{/if}

						<!-- activity shoreline glow -->
						{#if layers.activity && fresh > 0.01}
							<path
								d={islandPath(pr.r, seed, pr.region.kindDiversity, 1.12)}
								fill="none"
								stroke="#ebcb8b"
								stroke-width="4"
								opacity={fresh * 0.75}
								filter="url(#soft)"
							/>
						{/if}

						<!-- structure: shelf / beach / land / highland / peak -->
						{#if layers.structure}
							<ellipse cx="0" cy={pr.r * 0.16} rx={pr.r * 1.16} ry={pr.r * 0.92} fill="#000" opacity="0.35" filter="url(#soft)" />
							<path d={islandPath(pr.r, seed, pr.region.kindDiversity, 1.24)} fill="#12203280" />
							<path d={islandPath(pr.r, seed, pr.region.kindDiversity, 1.07)} fill="#2b2f27" opacity="0.85" />
							<path d={islandPath(pr.r, seed, pr.region.kindDiversity, 1)} fill={landFill(pr)} stroke="#00000055" stroke-width="1" />
							<path d={islandPath(pr.r, seed + 11, pr.region.kindDiversity, 0.56, -pr.r * 0.08)} fill={highlandFill(pr)} />
							{#if pr.region.symbolCount > 400}
								<path d={islandPath(pr.r, seed + 23, pr.region.kindDiversity, 0.28, -pr.r * 0.16)} fill="hsl({groupHue(pr.region.group)} 14% 32%)" />
							{/if}
						{/if}

						<!-- decision cairns + lighthouse -->
						{#if layers.decisions && pr.region.decisions.length}
							{#each cairns(pr) as c, i (i)}
								<circle cx={c.x} cy={c.y} r="2.1" fill="#7aa2ff" opacity="0.9" />
							{/each}
							<g transform="translate(0 {-pr.r * 0.34})">
								<rect x="-2.4" y="-13" width="4.8" height="13" fill="#a8c3ff" rx="1" />
								<circle cx="0" cy="-15" r="3" fill="#d7e3ff" />
								{#if fresh > 0.3 && layers.activity}
									<circle cx="0" cy="-15" r="7" fill="#d7e3ff" opacity="0.35" filter="url(#soft)" />
								{/if}
								<text y="-22" text-anchor="middle" class="count" fill="#a8c3ff">{pr.region.decisions.length}</text>
							</g>
						{/if}

						<!-- hazard faults -->
						{#if layers.effects && pr.region.hazards.length}
							{#each pr.region.hazards.slice(0, 4) as hz, i (hz.entry_id)}
								{@const hr = rng(hashString(hz.entry_id))}
								{@const hx = (hr() - 0.5) * pr.r * 1.1}
								{@const hy = (hr() - 0.5) * pr.r * 0.8}
								<path
									d="M {hx} {hy} l 3.4 4.4 l -2.2 3.2 l 4 4.6"
									stroke="#e06c75"
									stroke-width="1.8"
									fill="none"
									opacity="0.95"
								/>
							{/each}
						{/if}

						<!-- open-question motes -->
						{#if layers.thinking}
							{#each pr.region.thinking.filter((t) => t.kind === 'open_question').slice(0, 3) as oq, i (oq.entry_id)}
								<text
									x={(i - 1) * 13}
									y={-pr.r * 0.7 - 6}
									text-anchor="middle"
									class="mote"
									fill="#ebcb8b"
									opacity="0.85">?</text
								>
							{/each}
						{/if}

						<text y={pr.r + 16} text-anchor="middle" class="label">{pr.region.name}</text>
						<text y={pr.r + 28} text-anchor="middle" class="sublabel">{pr.region.symbolCount} symbols</text>
					</g>
				{/each}
			</svg>

			{#if shown}
				<div class="cardpos">
					<RegionCard region={shown} pinned={pinnedR === shown} onclose={() => (pinnedR = null)} />
				</div>
			{/if}

			<div class="legend">
				<span><i style:background="#8be9c3"></i> thinking aurora (opacity = confidence)</span>
				<span><i style:background="#7aa2ff"></i> decision cairns / lighthouse</span>
				<span><i style:background="#e0916c"></i> risk reef · <i style:background="#e06c75"></i> hazard fault</span>
				<span><i style:background="#ebcb8b"></i> activity shoreline (14-day fade)</span>
				<span class="hint">scroll to zoom · drag to pan · click island to pin</span>
			</div>
		{:else}
			<div class="loading">{error ?? progress}</div>
		{/if}
	</div>
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
	.map {
		position: relative;
		flex: 1;
		min-height: 0;
	}
	svg {
		width: 100%;
		height: 100%;
		display: block;
		cursor: grab;
		touch-action: none;
	}
	svg:active {
		cursor: grabbing;
	}
	.island {
		cursor: pointer;
	}
	.island:hover {
		filter: brightness(1.18);
	}
	.label {
		font-size: 11px;
		fill: #aab4c4;
		paint-order: stroke;
		stroke: #080b12;
		stroke-width: 3px;
		pointer-events: none;
	}
	.sublabel {
		font-size: 9px;
		fill: #5c6675;
		paint-order: stroke;
		stroke: #080b12;
		stroke-width: 3px;
		pointer-events: none;
	}
	.count {
		font-size: 9px;
		font-weight: 700;
	}
	.mote {
		font-size: 11px;
		font-weight: 700;
	}
	.cardpos {
		position: absolute;
		top: 12px;
		right: 12px;
		pointer-events: none;
	}
	.legend {
		position: absolute;
		left: 12px;
		bottom: 10px;
		display: flex;
		flex-direction: column;
		gap: 3px;
		font-size: 10px;
		color: var(--fg-dim);
		background: rgba(10, 13, 19, 0.8);
		padding: 8px 10px;
		border: 1px solid var(--border);
		border-radius: 6px;
	}
	.legend i {
		display: inline-block;
		width: 8px;
		height: 8px;
		border-radius: 50%;
		margin-right: 4px;
		vertical-align: -1px;
	}
	.legend .hint {
		margin-top: 3px;
		color: #4e5866;
	}
	.loading {
		display: flex;
		height: 100%;
		align-items: center;
		justify-content: center;
		color: var(--fg-dim);
		font-size: 13px;
	}
</style>
