/**
 * Territory view — shared data module (Plan T t-007 prototype).
 *
 * Fetches the workspace once and aggregates client-side into "regions":
 * stable feature-territories derived from directory structure. Regions are
 * the spatial unit all three territory prototypes render; the judgment data
 * (decisions / thinking / effects / activity) is what gets painted on them.
 *
 * Data sources
 * - `/territory-symbols.json` — static snapshot of the live by-qname index
 *   (9.8k symbols). Generated at demo-setup time directly from the ASD
 *   object store because `/api/v1/symbols` resolves each qname individually
 *   (~53ms each → 8m45s per 2000-row page at ExampleProj scale). Falls back to
 *   the paginated API when the snapshot is missing.
 * - `/api/v1/ledger?limit=1000` — every ledger entry, including Plan G
 *   thinking kinds (hypothesis / mental_model / failed_attempt /
 *   open_question) with confidence. `/api/v1/thinking` re-scans every qname
 *   (9 minutes at this scale) so the thinking layer is derived from the
 *   ledger listing instead.
 * - `/territory-effects.json` — static per-symbol effect categories
 *   (declared + transitive), aggregated from the effects store at setup
 *   time; there is no bulk per-symbol effects endpoint yet.
 * - `/api/v1/health`, `/api/v1/effects/overview` — global context.
 */

export interface SnapSymbol {
	id: string;
	q: string; // qname
	f: string; // file
	k: string; // kind
	l: number; // start line
}

export interface Entry {
	entry_id: string;
	symbol_id: string;
	kind: string;
	summary: string;
	body?: string;
	author: { kind: string; id: string };
	confidence?: number;
	created_at: string;
	tags?: string[];
	role?: string;
	command?: string;
	// enriched:
	qname?: string;
	file?: string;
	region?: string;
}

export interface Region {
	id: string;
	/** display name (same as id) */
	name: string;
	/** top-level territory group, e.g. "AudioEngine" — continents */
	group: string;
	symbolCount: number;
	fileCount: number;
	kindMix: Record<string, number>;
	/** normalized shannon entropy of kindMix, 0..1 — coastline complexity */
	kindDiversity: number;
	/** non-thinking, non-hazard ledger entries (newest first) */
	decisions: Entry[];
	hazards: Entry[];
	/** thinking entries (hypothesis/mental_model/failed_attempt/open_question) */
	thinking: Entry[];
	/** mean confidence across thinking entries that carry one */
	meanConfidence: number | null;
	/** raw weighted effect risk */
	risk: number;
	/** risk density normalized across regions, 0..1 */
	riskNorm: number;
	effectMix: Record<string, number>;
	lastActivityAt: string | null;
	recencyDays: number | null;
	topSymbols: { q: string; k: string; f: string; score: number }[];
	symbols: SnapSymbol[];
}

export interface EffectOverviewRow {
	effect: string;
	symbol_count: number;
	top_symbols: { qname: string; blast_radius: number }[];
}

export interface TerritoryData {
	regions: Region[];
	regionById: Map<string, Region>;
	/** all ledger entries, enriched with qname/file/region, newest first */
	entries: Entry[];
	effectsOverview: EffectOverviewRow[];
	symbolCount: number;
	dbPath: string;
	loadMs: number;
}

export const THINKING_KINDS = new Set([
	'hypothesis',
	'mental_model',
	'failed_attempt',
	'open_question'
]);
export const HAZARD_KINDS = new Set(['hazard', 'known_bug']);

/** Weighted risk per declared effect category. Writes/spawns dominate. */
const EFFECT_WEIGHTS: Record<string, number> = {
	'proc.spawn': 5,
	'io.db.write': 5,
	'io.net.out': 4.5,
	'state.global.write': 4,
	'io.fs.write': 3,
	'io.db.read': 2,
	'io.net.in': 2,
	'time.sleep': 2,
	'io.fs.read': 1.5,
	throw: 0.8,
	random: 0.4,
	'time.read': 0.2,
	log: 0.1
};
const TRANSITIVE_FACTOR = 0.35;

/** Directory tokens that carry no feature meaning at any depth. */
const NOISE_TOKENS = new Set(['Packages', 'App', 'Sources', 'Source', 'Tools']);
/** A 2-token region bigger than this splits one level deeper. */
const SPLIT_MAX = 900;
/** Regions smaller than this merge up into their top-level token. */
const MERGE_MIN = 15;

function regionTokens(file: string): string[] {
	const parts = file.split('/').slice(0, -1).filter((t) => !NOISE_TOKENS.has(t));
	const toks: string[] = [];
	for (const t of parts) if (t !== toks[toks.length - 1]) toks.push(t);
	return toks.length ? toks : ['root'];
}

function shannonDiversity(mix: Record<string, number>): number {
	const total = Object.values(mix).reduce((a, b) => a + b, 0);
	if (total === 0) return 0;
	const kinds = Object.keys(mix).length;
	if (kinds <= 1) return 0;
	let h = 0;
	for (const n of Object.values(mix)) {
		if (n === 0) continue;
		const p = n / total;
		h -= p * Math.log(p);
	}
	return h / Math.log(kinds);
}

async function getJson<T>(path: string): Promise<T> {
	const res = await fetch(path);
	if (!res.ok) throw new Error(`${res.status} ${res.statusText} — ${path}`);
	return (await res.json()) as T;
}

async function fetchSymbols(onProgress: (msg: string) => void): Promise<SnapSymbol[]> {
	try {
		onProgress('loading symbol snapshot…');
		const snap = await getJson<SnapSymbol[]>('/territory-symbols.json');
		if (Array.isArray(snap) && snap.length > 0) return snap;
	} catch {
		/* fall through to the live API */
	}
	// Fallback: paginated live API. Slow at ExampleProj scale (see module doc).
	const out: SnapSymbol[] = [];
	const LIMIT = 2000;
	for (let offset = 0; ; offset += LIMIT) {
		onProgress(`loading symbols ${offset}… (live API — this is slow at 9.6k symbols)`);
		const page = await getJson<
			{ symbol_id: string; qname: string; file: string; kind: string; start: { line: number } }[]
		>(`/api/v1/symbols?limit=${LIMIT}&offset=${offset}`);
		for (const s of page)
			out.push({ id: s.symbol_id, q: s.qname, f: s.file, k: s.kind, l: s.start.line });
		if (page.length < LIMIT) break;
	}
	return out;
}

let cached: TerritoryData | null = null;
let inflight: Promise<TerritoryData> | null = null;

export function loadTerritory(
	onProgress: (msg: string) => void = () => {}
): Promise<TerritoryData> {
	if (cached) return Promise.resolve(cached);
	if (inflight) return inflight;
	inflight = doLoad(onProgress).then((d) => {
		cached = d;
		inflight = null;
		return d;
	});
	return inflight;
}

async function doLoad(onProgress: (msg: string) => void): Promise<TerritoryData> {
	const t0 = performance.now();

	onProgress('loading…');
	const [symbols, health] = await Promise.all([
		fetchSymbols(onProgress),
		getJson<{ symbol_count: number; db_path: string }>('/api/v1/health')
	]);

	onProgress(`aggregating ${symbols.length} symbols…`);
	const [ledger, effectsBySymbol] = await Promise.all([
		getJson<Entry[]>('/api/v1/ledger?limit=1000'),
		getJson<Record<string, { d?: string[]; t?: string[] }>>('/territory-effects.json').catch(
			() => ({}) as Record<string, { d?: string[]; t?: string[] }>
		)
	]);
	// Context-only; ~3.6s at ExampleProj scale, so it must not block first paint.
	const effectsOverview: EffectOverviewRow[] = [];
	getJson<EffectOverviewRow[]>('/api/v1/effects/overview')
		.then((rows) => effectsOverview.push(...rows))
		.catch(() => {});

	// ---- region keys (two-pass split/merge, fully deterministic) ----------
	const count2 = new Map<string, number>();
	for (const s of symbols) {
		const k2 = regionTokens(s.f).slice(0, 2).join('/');
		count2.set(k2, (count2.get(k2) ?? 0) + 1);
	}
	const keyOf = new Map<string, string>(); // symbol_id -> provisional region key
	const count3 = new Map<string, number>();
	for (const s of symbols) {
		const toks = regionTokens(s.f);
		const k2 = toks.slice(0, 2).join('/');
		const k = (count2.get(k2) ?? 0) > SPLIT_MAX ? toks.slice(0, 3).join('/') : k2;
		keyOf.set(s.id, k);
		count3.set(k, (count3.get(k) ?? 0) + 1);
	}
	const finalKey = (s: SnapSymbol): string => {
		const k = keyOf.get(s.id)!;
		if ((count3.get(k) ?? 0) < MERGE_MIN) return regionTokens(s.f)[0];
		return k;
	};

	// ---- assemble regions --------------------------------------------------
	const regionById = new Map<string, Region>();
	const symById = new Map<string, SnapSymbol>();
	const regionOfSymbol = new Map<string, string>();
	for (const s of symbols) {
		symById.set(s.id, s);
		const key = finalKey(s);
		regionOfSymbol.set(s.id, key);
		let r = regionById.get(key);
		if (!r) {
			r = {
				id: key,
				name: key,
				group: key.split('/')[0],
				symbolCount: 0,
				fileCount: 0,
				kindMix: {},
				kindDiversity: 0,
				decisions: [],
				hazards: [],
				thinking: [],
				meanConfidence: null,
				risk: 0,
				riskNorm: 0,
				effectMix: {},
				lastActivityAt: null,
				recencyDays: null,
				topSymbols: [],
				symbols: []
			};
			regionById.set(key, r);
		}
		r.symbolCount++;
		r.kindMix[s.k] = (r.kindMix[s.k] ?? 0) + 1;
		r.symbols.push(s);
	}

	// files + effects per region
	const symbolRisk = new Map<string, number>();
	for (const r of regionById.values()) {
		r.fileCount = new Set(r.symbols.map((s) => s.f)).size;
		for (const s of r.symbols) {
			const eff = effectsBySymbol[s.id];
			if (!eff) continue;
			let risk = 0;
			for (const cat of eff.d ?? []) {
				const w = EFFECT_WEIGHTS[cat] ?? 1;
				risk += w;
				r.effectMix[cat] = (r.effectMix[cat] ?? 0) + 1;
			}
			for (const cat of eff.t ?? []) {
				risk += (EFFECT_WEIGHTS[cat] ?? 1) * TRANSITIVE_FACTOR;
			}
			r.risk += risk;
			if (risk > 0) symbolRisk.set(s.id, risk);
		}
		r.kindDiversity = shannonDiversity(r.kindMix);
	}

	// ledger entries → regions
	const entries: Entry[] = [];
	const entryCountBySymbol = new Map<string, number>();
	for (const e of ledger) {
		const sym = symById.get(e.symbol_id);
		const enriched: Entry = {
			...e,
			qname: sym?.q,
			file: sym?.f,
			region: sym ? regionOfSymbol.get(sym.id) : undefined
		};
		entries.push(enriched);
		if (!sym) continue;
		entryCountBySymbol.set(e.symbol_id, (entryCountBySymbol.get(e.symbol_id) ?? 0) + 1);
		const r = regionById.get(regionOfSymbol.get(sym.id)!);
		if (!r) continue;
		if (THINKING_KINDS.has(e.kind)) r.thinking.push(enriched);
		else if (HAZARD_KINDS.has(e.kind)) r.hazards.push(enriched);
		else r.decisions.push(enriched);
		if (!r.lastActivityAt || e.created_at > r.lastActivityAt) r.lastActivityAt = e.created_at;
	}
	entries.sort((a, b) => b.created_at.localeCompare(a.created_at));

	// derived per-region stats
	const now = Date.now();
	let maxRiskDensity = 0;
	for (const r of regionById.values()) {
		const sortNewest = (a: Entry, b: Entry) => b.created_at.localeCompare(a.created_at);
		r.decisions.sort(sortNewest);
		r.hazards.sort(sortNewest);
		r.thinking.sort(sortNewest);
		const confs = r.thinking
			.map((e) => e.confidence)
			.filter((c): c is number => typeof c === 'number');
		r.meanConfidence = confs.length
			? confs.reduce((a, b) => a + b, 0) / confs.length
			: null;
		if (r.lastActivityAt)
			r.recencyDays = (now - Date.parse(r.lastActivityAt)) / 86_400_000;
		const density = r.risk / Math.max(1, r.symbolCount);
		maxRiskDensity = Math.max(maxRiskDensity, density);
		// top symbols: judgment involvement first, then risk, then size proxy
		r.topSymbols = r.symbols
			.map((s) => ({
				q: s.q,
				k: s.k,
				f: s.f,
				score:
					(entryCountBySymbol.get(s.id) ?? 0) * 10 +
					(symbolRisk.get(s.id) ?? 0) +
					(s.k === 'class' || s.k === 'module' ? 0.5 : 0)
			}))
			.sort((a, b) => b.score - a.score || a.q.localeCompare(b.q))
			.slice(0, 6);
	}
	for (const r of regionById.values()) {
		const density = r.risk / Math.max(1, r.symbolCount);
		r.riskNorm = maxRiskDensity > 0 ? density / maxRiskDensity : 0;
	}

	const regions = [...regionById.values()].sort((a, b) => a.id.localeCompare(b.id));
	const loadMs = performance.now() - t0;
	onProgress(`ready — ${regions.length} regions, ${entries.length} ledger entries`);
	return {
		regions,
		regionById,
		entries,
		effectsOverview,
		symbolCount: health.symbol_count,
		dbPath: health.db_path,
		loadMs
	};
}

/** Age → 0..1 freshness (1 = today, fades to 0 over `days`). */
export function freshness(recencyDays: number | null, days = 14): number {
	if (recencyDays == null) return 0;
	return Math.max(0, 1 - recencyDays / days);
}

export const KIND_COLORS: Record<string, string> = {
	function: '#88c0d0',
	method: '#a3be8c',
	class: '#d08770',
	module: '#b48ead',
	variable: '#ebcb8b'
};

export const THINKING_COLORS: Record<string, string> = {
	hypothesis: '#8be9c3',
	mental_model: '#7aa2ff',
	failed_attempt: '#e0916c',
	open_question: '#ebcb8b'
};

export function kindLabel(kind: string): string {
	return kind.replace(/_/g, ' ');
}
