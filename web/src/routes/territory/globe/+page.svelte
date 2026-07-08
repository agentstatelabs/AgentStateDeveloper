<script lang="ts">
	/**
	 * GLOBE — regions as continents on a planet. Prototype-only Three.js dep
	 * (app-level, NOT lens-core). Continents are painted onto an
	 * equirectangular canvas texture from the seeded spherical layout;
	 * judgment layers are 3D objects above the surface:
	 *
	 *   decisions → light beacons (height = count)
	 *   thinking  → aurora ring, opacity = mean confidence (headline)
	 *   effects   → heat tint painted around risky continents
	 *   activity  → night-lights that fade over 14 days
	 */
	import {
		loadTerritory,
		freshness,
		type TerritoryData,
		type Region,
		type Entry
	} from '$lib/territory/data';
	import { placeRegionsOnSphere, hashString, rng, type SphericalRegion } from '$lib/territory/layout';
	import LayerPicker, { type Layers } from '../LayerPicker.svelte';
	import RegionCard from '../RegionCard.svelte';
	import EntryTip from '../EntryTip.svelte';
	import DrillDown from '../DrillDown.svelte';
	import { untrack } from 'svelte';
	import * as THREE from 'three';
	import { OrbitControls } from 'three/examples/jsm/controls/OrbitControls.js';

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

	let hovered = $state<Region | null>(null);
	let selected = $state<Region | null>(null);
	let focusEntry = $state<string | null>(null);
	let mouse = $state({ x: 0, y: 0 });
	let markerTip = $state<{ entry: Entry; count: number } | null>(null);
	// hover card yields to the marker tooltip and to the drill-down panel
	let shown = $derived(selected || markerTip ? null : hovered);

	let container: HTMLDivElement | undefined = $state();
	let placed: SphericalRegion[] = [];

	const R = 100;

	function latLonToVec(lat: number, lon: number, radius: number): THREE.Vector3 {
		const phi = ((90 - lat) * Math.PI) / 180;
		const theta = ((lon + 180) * Math.PI) / 180;
		return new THREE.Vector3(
			-radius * Math.sin(phi) * Math.cos(theta),
			radius * Math.cos(phi),
			radius * Math.sin(phi) * Math.sin(theta)
		);
	}

	// ---- texture painting ---------------------------------------------------
	const TEX_W = 2048;
	const TEX_H = 1024;
	function paintTexture(canvas: HTMLCanvasElement, l: Layers) {
		const ctx = canvas.getContext('2d')!;
		// ocean
		const grad = ctx.createLinearGradient(0, 0, 0, TEX_H);
		grad.addColorStop(0, '#0a0f18');
		grad.addColorStop(0.5, '#0d1420');
		grad.addColorStop(1, '#0a0f18');
		ctx.fillStyle = grad;
		ctx.fillRect(0, 0, TEX_W, TEX_H);
		// graticule
		ctx.strokeStyle = '#151c29';
		ctx.lineWidth = 1;
		for (let i = 0; i <= 24; i++) {
			ctx.beginPath();
			ctx.moveTo((i / 24) * TEX_W, 0);
			ctx.lineTo((i / 24) * TEX_W, TEX_H);
			ctx.stroke();
		}
		for (let i = 0; i <= 12; i++) {
			ctx.beginPath();
			ctx.moveTo(0, (i / 12) * TEX_H);
			ctx.lineTo(TEX_W, (i / 12) * TEX_H);
			ctx.stroke();
		}
		const px = (lon: number) => ((((lon + 180) % 360) + 360) % 360) * (TEX_W / 360);
		const py = (lat: number) => ((90 - lat) / 180) * TEX_H;
		const degX = (lat: number) => TEX_W / 360 / Math.max(0.25, Math.cos((lat * Math.PI) / 180));
		const degY = TEX_H / 180;

		for (const p of placed) {
			const hue = hashString(p.region.group) % 360;
			const rand = rng(hashString(p.region.id));
			// risk halo first (under the land)
			if (l.effects && p.region.riskNorm > 0.02) {
				ctx.fillStyle = `hsla(18 65% 45% / ${0.08 + p.region.riskNorm * 0.3})`;
				ctx.beginPath();
				ctx.ellipse(px(p.lon), py(p.lat), p.r * 1.5 * degX(p.lat), p.r * 1.5 * degY, 0, 0, Math.PI * 2);
				ctx.fill();
			}
			if (!l.structure) continue;
			// continent: cluster of seeded blobs
			const blobs = 5 + Math.round(p.region.kindDiversity * 6);
			for (let b = 0; b < blobs; b++) {
				const a = rand() * Math.PI * 2;
				const d = rand() * p.r * 0.62;
				const br = p.r * (0.38 + rand() * 0.5);
				const cx = px(p.lon + Math.cos(a) * d);
				const cy = py(p.lat + Math.sin(a) * d * 0.8);
				const dens = l.decisions
					? Math.min(1, p.region.decisions.length / Math.max(4, p.region.symbolCount / 60))
					: 0;
				ctx.fillStyle = `hsl(${hue} ${15 + dens * 12}% ${16 + dens * 8}%)`;
				ctx.beginPath();
				ctx.ellipse(cx, cy, br * degX(p.lat), br * degY, 0, 0, Math.PI * 2);
				ctx.fill();
			}
			// highland core
			ctx.fillStyle = `hsl(${hue} 17% 24%)`;
			ctx.beginPath();
			ctx.ellipse(px(p.lon), py(p.lat), p.r * 0.45 * degX(p.lat), p.r * 0.45 * degY, 0, 0, Math.PI * 2);
			ctx.fill();
		}
	}

	// ---- scene ---------------------------------------------------------------
	$effect(() => {
		loadTerritory((m) => (progress = m))
			.then((d) => {
				data = d;
				placed = placeRegionsOnSphere(d.regions);
			})
			.catch((e) => (error = String(e)));
	});

	$effect(() => {
		if (!data || !container) return;
		const el = container;
		const scene = new THREE.Scene();
		scene.background = new THREE.Color('#06080d');
		const camera = new THREE.PerspectiveCamera(45, el.clientWidth / el.clientHeight, 1, 2000);
		camera.position.set(0, 70, 430);
		const renderer = new THREE.WebGLRenderer({ antialias: true });
		renderer.setPixelRatio(Math.min(2, window.devicePixelRatio));
		renderer.setSize(el.clientWidth, el.clientHeight);
		el.appendChild(renderer.domElement);

		const controls = new OrbitControls(camera, renderer.domElement);
		controls.enableDamping = true;
		controls.dampingFactor = 0.08;
		controls.minDistance = 140;
		controls.maxDistance = 700;
		controls.autoRotate = true;
		controls.autoRotateSpeed = 0.35;
		controls.addEventListener('start', () => (controls.autoRotate = false));

		// globe
		const canvas = document.createElement('canvas');
		canvas.width = TEX_W;
		canvas.height = TEX_H;
		// untracked: layer toggles must repaint in place (applyLayers below),
		// NOT re-run this effect — a rebuild would reset the camera.
		paintTexture(canvas, untrack(() => $state.snapshot(layers)) as Layers);
		const texture = new THREE.CanvasTexture(canvas);
		texture.colorSpace = THREE.SRGBColorSpace;
		const globe = new THREE.Mesh(
			new THREE.SphereGeometry(R, 96, 64),
			new THREE.MeshBasicMaterial({ map: texture })
		);
		scene.add(globe);

		// atmosphere
		const atmo = new THREE.Mesh(
			new THREE.SphereGeometry(R * 1.045, 64, 48),
			new THREE.MeshBasicMaterial({
				color: '#3a5a8a',
				transparent: true,
				opacity: 0.07,
				side: THREE.BackSide
			})
		);
		scene.add(atmo);

		// stars
		{
			const starGeo = new THREE.BufferGeometry();
			const srand = rng(42);
			const pos = new Float32Array(600 * 3);
			for (let i = 0; i < 600; i++) {
				const v = latLonToVec((srand() - 0.5) * 180, srand() * 360, 900 + srand() * 400);
				pos.set([v.x, v.y, v.z], i * 3);
			}
			starGeo.setAttribute('position', new THREE.BufferAttribute(pos, 3));
			scene.add(
				new THREE.Points(starGeo, new THREE.PointsMaterial({ color: '#5c6675', size: 1.2, sizeAttenuation: false, transparent: true, opacity: 0.7 }))
			);
		}

		// ---- judgment layers as 3D groups ----
		const beacons = new THREE.Group();
		const auroras = new THREE.Group();
		const lights = new THREE.Group();
		scene.add(beacons, auroras, lights);
		const auroraMats: { mat: THREE.MeshBasicMaterial; base: number }[] = [];
		/** attached to pickable marker meshes for hover tooltips / drill-down */
		interface MarkerData {
			region: Region;
			entry: Entry;
			count: number;
		}

		for (const p of placed) {
			const surface = latLonToVec(p.lat, p.lon, R);
			const normal = surface.clone().normalize();

			// decision beacon
			if (p.region.decisions.length) {
				const marker: MarkerData = {
					region: p.region,
					entry: p.region.decisions[0],
					count: p.region.decisions.length
				};
				const h = 6 + Math.log2(1 + p.region.decisions.length) * 7;
				const beam = new THREE.Mesh(
					new THREE.CylinderGeometry(0.7, 1.6, h, 6),
					new THREE.MeshBasicMaterial({ color: '#7aa2ff', transparent: true, opacity: 0.85 })
				);
				beam.position.copy(normal.clone().multiplyScalar(R + h / 2));
				beam.quaternion.setFromUnitVectors(new THREE.Vector3(0, 1, 0), normal);
				beam.userData.marker = marker;
				const tip = new THREE.Mesh(
					new THREE.SphereGeometry(1.9, 10, 8),
					new THREE.MeshBasicMaterial({ color: '#d7e3ff' })
				);
				tip.position.copy(normal.clone().multiplyScalar(R + h + 1.5));
				tip.userData.marker = marker;
				beacons.add(beam, tip);
			}

			// thinking aurora ring
			if (p.region.thinking.length) {
				const marker: MarkerData = {
					region: p.region,
					entry: p.region.thinking[0],
					count: p.region.thinking.length
				};
				const conf = p.region.meanConfidence ?? 0.5;
				const ringR = p.r * 1.15 * (Math.PI / 180) * R;
				const mat = new THREE.MeshBasicMaterial({
					color: '#8be9c3',
					transparent: true,
					opacity: 0.14 + conf * 0.4,
					side: THREE.DoubleSide,
					blending: THREE.AdditiveBlending,
					depthWrite: false
				});
				const ring = new THREE.Mesh(new THREE.TorusGeometry(ringR, 0.9 + conf * 1.6, 8, 48), mat);
				ring.position.copy(normal.clone().multiplyScalar(R + 4.5));
				ring.quaternion.setFromUnitVectors(new THREE.Vector3(0, 0, 1), normal);
				ring.userData.marker = marker;
				auroras.add(ring);
				auroraMats.push({ mat, base: 0.14 + conf * 0.4 });
				const ring2 = new THREE.Mesh(
					new THREE.TorusGeometry(ringR * 0.7, 0.7, 8, 40),
					new THREE.MeshBasicMaterial({
						color: '#7aa2ff',
						transparent: true,
						opacity: (0.14 + conf * 0.4) * 0.5,
						blending: THREE.AdditiveBlending,
						depthWrite: false
					})
				);
				ring2.position.copy(normal.clone().multiplyScalar(R + 7));
				ring2.quaternion.setFromUnitVectors(new THREE.Vector3(0, 0, 1), normal);
				ring2.userData.marker = marker;
				auroras.add(ring2);
			}

			// activity night-lights
			const fresh = freshness(p.region.recencyDays);
			if (fresh > 0.02) {
				const nrand = rng(hashString(p.region.id + ':lights'));
				const n = 4 + Math.round(fresh * 14);
				const geo = new THREE.BufferGeometry();
				const pos = new Float32Array(n * 3);
				for (let i = 0; i < n; i++) {
					const v = latLonToVec(
						p.lat + (nrand() - 0.5) * p.r * 1.3,
						p.lon + (nrand() - 0.5) * p.r * 1.6,
						R + 0.8
					);
					pos.set([v.x, v.y, v.z], i * 3);
				}
				geo.setAttribute('position', new THREE.BufferAttribute(pos, 3));
				lights.add(
					new THREE.Points(
						geo,
						new THREE.PointsMaterial({
							color: '#ebcb8b',
							size: 2.4,
							sizeAttenuation: false,
							transparent: true,
							opacity: 0.25 + fresh * 0.7
						})
					)
				);
			}
		}

		// spotlight ring for the hovered region (restrained — soft additive ring)
		const spot = new THREE.Mesh(
			new THREE.RingGeometry(0.94, 1.06, 56),
			new THREE.MeshBasicMaterial({
				color: '#d7e3ff',
				transparent: true,
				opacity: 0.3,
				side: THREE.DoubleSide,
				blending: THREE.AdditiveBlending,
				depthWrite: false
			})
		);
		spot.visible = false;
		scene.add(spot);
		const sphOf = new Map(placed.map((p) => [p.region.id, p]));
		function updateSpot() {
			const r = hovered ?? selected;
			const p = r ? sphOf.get(r.id) : undefined;
			if (!p) {
				spot.visible = false;
				return;
			}
			const normal = latLonToVec(p.lat, p.lon, R).normalize();
			const ringR = p.r * 1.35 * (Math.PI / 180) * R;
			spot.position.copy(normal.clone().multiplyScalar(R + 2.5));
			spot.quaternion.setFromUnitVectors(new THREE.Vector3(0, 0, 1), normal);
			spot.scale.set(ringR, ringR, 1);
			spot.visible = true;
		}

		// hover picking: judgment markers first, then nearest region center
		const raycaster = new THREE.Raycaster();
		const pointer = new THREE.Vector2();
		const centers = placed.map((p) => ({
			p,
			v: latLonToVec(p.lat, p.lon, R).normalize()
		}));
		function setPointer(cx: number, cy: number) {
			const rect = renderer.domElement.getBoundingClientRect();
			pointer.x = ((cx - rect.left) / rect.width) * 2 - 1;
			pointer.y = -((cy - rect.top) / rect.height) * 2 + 1;
			raycaster.setFromCamera(pointer, camera);
		}
		function pickMarker(cx: number, cy: number): MarkerData | null {
			setPointer(cx, cy);
			const groups = [beacons, auroras].filter((g) => g.visible);
			if (!groups.length) return null;
			for (const hit of raycaster.intersectObjects(groups, true)) {
				const m = hit.object.userData.marker as MarkerData | undefined;
				if (m) return m;
			}
			return null;
		}
		function pickAt(cx: number, cy: number): Region | null {
			setPointer(cx, cy);
			const hit = raycaster.intersectObject(globe, false)[0];
			if (!hit) return null;
			const n = hit.point.clone().normalize();
			let best: SphericalRegion | null = null;
			let bestD = Infinity;
			for (const c of centers) {
				const d = n.angleTo(c.v) * (180 / Math.PI);
				if (d < bestD) {
					bestD = d;
					best = c.p;
				}
			}
			return best && bestD < best.r * 1.5 ? best.region : null;
		}
		function onMove(ev: PointerEvent) {
			mouse = { x: ev.clientX, y: ev.clientY };
			const m = pickMarker(ev.clientX, ev.clientY);
			if (m) {
				markerTip = { entry: m.entry, count: m.count };
				hovered = m.region;
			} else {
				markerTip = null;
				hovered = pickAt(ev.clientX, ev.clientY);
			}
			updateSpot();
			renderer.domElement.style.cursor = hovered ? 'pointer' : 'grab';
		}
		function onLeave() {
			markerTip = null;
			hovered = null;
			updateSpot();
		}
		let downAt = { x: 0, y: 0 };
		function onDown(ev: PointerEvent) {
			downAt = { x: ev.clientX, y: ev.clientY };
		}
		function onUp(ev: PointerEvent) {
			if (Math.hypot(ev.clientX - downAt.x, ev.clientY - downAt.y) > 5) return;
			const m = pickMarker(ev.clientX, ev.clientY);
			if (m) {
				selected = m.region;
				focusEntry = m.entry.entry_id;
				return;
			}
			// click a continent → drill down; click open ocean → close
			selected = pickAt(ev.clientX, ev.clientY);
			focusEntry = null;
			updateSpot();
		}
		renderer.domElement.addEventListener('pointermove', onMove);
		renderer.domElement.addEventListener('pointerleave', onLeave);
		renderer.domElement.addEventListener('pointerdown', onDown);
		renderer.domElement.addEventListener('pointerup', onUp);

		// layer reactivity — applied IN PLACE each frame so the camera/orbit
		// state survives toggles (rebuilding the scene here was the old bug).
		const applyLayers = (l: Layers) => {
			beacons.visible = l.decisions;
			auroras.visible = l.thinking;
			lights.visible = l.activity;
			paintTexture(canvas, l);
			texture.needsUpdate = true;
		};
		// untracked: this effect must NOT re-run on layer toggles.
		let lastLayers = untrack(() => JSON.stringify($state.snapshot(layers)));

		let raf = 0;
		const clock = new THREE.Clock();
		function tick() {
			raf = requestAnimationFrame(tick);
			// untracked defensively: the first tick() runs synchronously inside
			// this $effect, and a tracked read here would rebuild on toggle.
			const cur = untrack(() => JSON.stringify($state.snapshot(layers)));
			if (cur !== lastLayers) {
				lastLayers = cur;
				applyLayers(JSON.parse(cur));
			}
			const t = clock.getElapsedTime();
			// aurora shimmer
			auroraMats.forEach(({ mat, base }, i) => {
				mat.opacity = base * (0.8 + 0.2 * Math.sin(t * 1.4 + i * 1.7));
			});
			controls.update();
			renderer.render(scene, camera);
		}
		tick();

		const onResize = () => {
			camera.aspect = el.clientWidth / el.clientHeight;
			camera.updateProjectionMatrix();
			renderer.setSize(el.clientWidth, el.clientHeight);
		};
		window.addEventListener('resize', onResize);

		return () => {
			cancelAnimationFrame(raf);
			window.removeEventListener('resize', onResize);
			renderer.domElement.remove();
			renderer.dispose();
			controls.dispose();
		};
	});

	const fmt = (n: number) => n.toLocaleString();
</script>

<svelte:head><title>Territory · Globe</title></svelte:head>

<div class="wrap">
	<div class="bar">
		<div class="crumbs"><a href="/territory">territory</a> / <strong>globe</strong></div>
		<LayerPicker bind:layers />
		{#if data}
			<div class="meta">
				{fmt(data.symbolCount)} symbols · {data.regions.length} continents ·
				{data.entries.length} ledger entries · load {(data.loadMs / 1000).toFixed(2)}s
			</div>
		{:else}
			<div class="meta">{error ?? progress}</div>
		{/if}
	</div>

	<div class="stage">
		<div class="canvas" bind:this={container}></div>
		{#if !data}
			<div class="loading">{error ?? progress}</div>
		{/if}
		{#if shown}
			<div class="cardpos">
				<RegionCard region={shown} />
			</div>
		{/if}
		{#if markerTip}
			<EntryTip entry={markerTip.entry} count={markerTip.count} x={mouse.x} y={mouse.y} />
		{/if}
		<DrillDown region={selected} focusEntryId={focusEntry} onclose={() => (selected = null)} />
		{#if data}
			<div class="legend">
				<span><i style:background="#8be9c3"></i> thinking aurora (opacity = confidence)</span>
				<span><i style:background="#7aa2ff"></i> decision beacon (height = count)</span>
				<span><i style:background="#e0916c"></i> risk heat</span>
				<span><i style:background="#ebcb8b"></i> activity night-lights (14-day fade)</span>
				<span class="hint">drag to orbit · scroll to zoom · hover beacons/auroras · click continent to drill down</span>
			</div>
		{/if}
	</div>
</div>

<style>
	.wrap {
		margin: -24px;
		height: calc(100vh - 42px);
		display: flex;
		flex-direction: column;
		background: #06080d;
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
		flex: 1;
		min-height: 0;
	}
	.canvas {
		position: absolute;
		inset: 0;
	}
	.canvas :global(canvas) {
		display: block;
	}
	.loading {
		position: absolute;
		inset: 0;
		display: flex;
		align-items: center;
		justify-content: center;
		color: var(--fg-dim);
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
		background: rgba(8, 10, 15, 0.8);
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
</style>
