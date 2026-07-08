/**
 * Territory view — deterministic seeded layout.
 *
 * The core promise: the SAME repo yields the SAME map every run, so
 * developers build spatial memory. No force simulation, no randomness that
 * isn't derived from region names. Related regions (same top-level group)
 * cluster into the same angular sector — continents emerge from naming.
 */

import type { Region } from './data';

/** FNV-1a 32-bit string hash. */
export function hashString(s: string): number {
	let h = 0x811c9dc5;
	for (let i = 0; i < s.length; i++) {
		h ^= s.charCodeAt(i);
		h = Math.imul(h, 0x01000193);
	}
	return h >>> 0;
}

/** mulberry32 — tiny deterministic PRNG. */
export function rng(seed: number): () => number {
	let a = seed >>> 0;
	return () => {
		a |= 0;
		a = (a + 0x6d2b79f5) | 0;
		let t = Math.imul(a ^ (a >>> 15), 1 | a);
		t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
		return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
	};
}

export interface PlacedRegion {
	region: Region;
	x: number;
	y: number;
	/** island body radius (px in map space) */
	r: number;
	/** angular sector of its group, radians — used by globe too */
	groupAngle: number;
}

/** Radius from symbol count: area ~ count, softened. */
export function regionRadius(symbolCount: number): number {
	return 20 + Math.sqrt(symbolCount) * 3.0;
}

/**
 * Seeded polar placement + deterministic collision relaxation.
 * Groups (top-level tokens) get stable angular sectors from their name hash;
 * regions scatter within their sector. Relaxation iterates in sorted region
 * order with fixed step count — bit-identical output for identical input.
 */
export function placeRegions(regions: Region[]): PlacedRegion[] {
	const groups = [...new Set(regions.map((r) => r.group))].sort();
	// Spread group sectors evenly around the circle, order seeded by name
	// hash so it's stable regardless of which groups exist.
	const sorted = [...groups].sort((a, b) => hashString(a) - hashString(b));
	const sector = new Map<string, number>();
	sorted.forEach((g, i) => sector.set(g, (i / sorted.length) * Math.PI * 2));

	const placed: PlacedRegion[] = regions.map((region) => {
		const rand = rng(hashString(region.id));
		const base = sector.get(region.group)!;
		const angle = base + (rand() - 0.5) * ((Math.PI * 2) / sorted.length) * 1.35;
		// Bigger regions gravitate inward — capitals near the center.
		const dist = 230 + rand() * 380 - Math.min(140, Math.sqrt(region.symbolCount) * 2.2);
		return {
			region,
			x: Math.cos(angle) * dist,
			y: Math.sin(angle) * dist * 0.86, // slight vertical squash, map-like
			r: regionRadius(region.symbolCount),
			groupAngle: base
		};
	});

	// Deterministic circle-separation relaxation.
	const PAD = 38;
	for (let step = 0; step < 240; step++) {
		let moved = false;
		for (let i = 0; i < placed.length; i++) {
			for (let j = i + 1; j < placed.length; j++) {
				const a = placed[i];
				const b = placed[j];
				const dx = b.x - a.x;
				const dy = b.y - a.y;
				const d = Math.hypot(dx, dy) || 0.001;
				const min = a.r + b.r + PAD;
				if (d < min) {
					const push = (min - d) / 2;
					const ux = dx / d;
					const uy = dy / d;
					a.x -= ux * push;
					a.y -= uy * push;
					b.x += ux * push;
					b.y += uy * push;
					moved = true;
				}
			}
		}
		if (!moved) break;
	}
	return placed;
}

/**
 * Island coastline polygon: radial blob whose lobe count and roughness come
 * from kind diversity (more kinds → more complex coastline). Returns closed
 * SVG path (Catmull-Rom smoothed).
 */
export function islandPath(
	placedR: number,
	seed: number,
	diversity: number,
	scale = 1,
	offsetY = 0
): string {
	const rand = rng(seed);
	const lobes = 3 + Math.round(diversity * 4);
	const rough = 0.07 + diversity * 0.15;
	const phase = rand() * Math.PI * 2;
	const phase2 = rand() * Math.PI * 2;
	const n = 28;
	const pts: [number, number][] = [];
	for (let i = 0; i < n; i++) {
		const a = (i / n) * Math.PI * 2;
		const wobble =
			1 +
			rough * Math.sin(a * lobes + phase) +
			rough * 0.4 * Math.sin(a * (lobes * 2 + 1) + phase2);
		const rr = placedR * scale * wobble;
		pts.push([Math.cos(a) * rr, Math.sin(a) * rr * 0.82 + offsetY]);
	}
	return catmullRomPath(pts);
}

/** Closed Catmull-Rom → cubic Bezier SVG path. */
export function catmullRomPath(pts: [number, number][]): string {
	const n = pts.length;
	let d = `M ${pts[0][0].toFixed(2)} ${pts[0][1].toFixed(2)} `;
	for (let i = 0; i < n; i++) {
		const p0 = pts[(i - 1 + n) % n];
		const p1 = pts[i];
		const p2 = pts[(i + 1) % n];
		const p3 = pts[(i + 2) % n];
		const c1x = p1[0] + (p2[0] - p0[0]) / 6;
		const c1y = p1[1] + (p2[1] - p0[1]) / 6;
		const c2x = p2[0] - (p3[0] - p1[0]) / 6;
		const c2y = p2[1] - (p3[1] - p1[1]) / 6;
		d += `C ${c1x.toFixed(2)} ${c1y.toFixed(2)}, ${c2x.toFixed(2)} ${c2y.toFixed(2)}, ${p2[0].toFixed(2)} ${p2[1].toFixed(2)} `;
	}
	return d + 'Z';
}

/**
 * Spherical placement for the globe: same group sectors become longitude
 * bands; hash-seeded latitude within ±55°. Deterministic.
 */
export interface SphericalRegion {
	region: Region;
	lat: number; // degrees
	lon: number; // degrees
	r: number; // angular radius, degrees
}

export function placeRegionsOnSphere(regions: Region[]): SphericalRegion[] {
	const groups = [...new Set(regions.map((r) => r.group))].sort();
	const sorted = [...groups].sort((a, b) => hashString(a) - hashString(b));
	const lonOf = new Map<string, number>();
	sorted.forEach((g, i) => lonOf.set(g, (i / sorted.length) * 360));

	const placed: SphericalRegion[] = regions.map((region) => {
		const rand = rng(hashString(region.id));
		const lon =
			lonOf.get(region.group)! + (rand() - 0.5) * (360 / sorted.length) * 1.4;
		const lat = (rand() - 0.5) * 110; // ±55°
		const r = 4 + Math.sqrt(region.symbolCount) * 0.42;
		return { region, lat, lon, r };
	});

	// Relax on the sphere (approximate, in degree space with cos-lat metric).
	for (let step = 0; step < 200; step++) {
		let moved = false;
		for (let i = 0; i < placed.length; i++) {
			for (let j = i + 1; j < placed.length; j++) {
				const a = placed[i];
				const b = placed[j];
				const midLat = ((a.lat + b.lat) / 2) * (Math.PI / 180);
				let dLon = b.lon - a.lon;
				if (dLon > 180) dLon -= 360;
				if (dLon < -180) dLon += 360;
				const dx = dLon * Math.cos(midLat);
				const dy = b.lat - a.lat;
				const d = Math.hypot(dx, dy) || 0.001;
				const min = a.r + b.r + 3.5;
				if (d < min) {
					const push = (min - d) / 2;
					const ux = dx / d;
					const uy = dy / d;
					const cos = Math.max(0.3, Math.cos(midLat));
					a.lon -= (ux * push) / cos;
					b.lon += (ux * push) / cos;
					a.lat = clampLat(a.lat - uy * push);
					b.lat = clampLat(b.lat + uy * push);
					moved = true;
				}
			}
		}
		if (!moved) break;
	}
	return placed;
}

function clampLat(l: number): number {
	return Math.max(-72, Math.min(72, l));
}
