/**
 * Client for the distilled-metrics API (`crates/agentstatedeveloper-mcp/src/metrics.rs`).
 *
 * These payloads are ASD-specific and have no CTXone counterpart, so unlike
 * the shapes in `$lib/types` they are declared here rather than in lens-core.
 *
 * Every list endpoint answers the same envelope — `total` / `offset` /
 * `limit` / `items` / `facets` — so the /records explorer can drive four
 * different record types through one search-and-page shell.
 */

import type { GcDryRun } from '@agentstate/lens-core';

import { API_BASE } from './api';

// -----------------------------------------------------------------------------
// Envelope
// -----------------------------------------------------------------------------

/** One facet value and how many records carry it. */
export interface FacetValue {
	value: string;
	count: number;
}

export interface Paged<T> {
	total: number;
	offset: number;
	limit: number;
	/** Rows read out of the store before filtering. */
	scanned: number;
	items: T[];
	facets: Record<string, FacetValue[]>;
}

// -----------------------------------------------------------------------------
// Record shapes
// -----------------------------------------------------------------------------

export interface MilestoneRecord {
	commit: string;
	commit_id: string;
	kind: string;
	timestamp: string;
	day: string;
	namespace: string;
	agent: string;
	description: string;
	/** Short id of the snapshot this milestone pins, or null for pre-t-005 rows. */
	state_root: string | null;
	/** False = survives as a description but no longer names a snapshot. */
	pins_state: boolean;
}

export interface MilestonePage extends Paged<MilestoneRecord> {
	/** Milestones with no `state_root` — nothing for GC to hold reachable. */
	unpinned: number;
}

export interface RollupRecord {
	day: string;
	namespace: string;
	agent: string;
	intent: string;
	commits: number;
	first_ts: string;
	last_ts: string;
}

export interface RollupPage extends Paged<RollupRecord> {
	totals: {
		commits: number;
		days: { first: string | null; last: string | null; count: number };
	};
}

export interface CommitRecord {
	commit: string;
	commit_id: string;
	timestamp: string;
	day: string;
	agent: string;
	intent: string;
	description: string;
	reasoning: string | null;
	confidence: number | null;
	parents: number;
	state_root: string;
	/** True = a milestone names this commit, so a sweep keeps it reachable. */
	on_spine: boolean;
}

export interface CommitPage extends Paged<CommitRecord> {
	/** True when the DAG walk hit `scan` before the frontier emptied. */
	capped: boolean;
	scan: number;
	/**
	 * Commits the rollup accounts for. Larger than `scanned` means the store
	 * holds commits the ref head no longer reaches — already-garbage.
	 */
	distilled: number;
	on_spine: number;
}

export interface FeedbackRecord {
	entry_id: string;
	symbol_id: string;
	symbol_qname: string;
	query: string;
	verdict: string;
	author: string;
	created_at: string;
	note: string | null;
	file_scope: string | null;
	expires_at: string | null;
	expired: boolean;
}

export type FeedbackPage = Paged<FeedbackRecord>;

// -----------------------------------------------------------------------------
// Health shapes
// -----------------------------------------------------------------------------

export interface IndexHealth {
	db_path: string;
	db_bytes: number | null;
	ref_name: string;
	indexed_at: number | null;
	indexed_age: { secs: number; human: string } | null;
	symbols: { asg: number; fts: number; fts_rows: number; annotated: number };
	feedback_entries: number;
	/** Null when the two indexes agree — absence is the healthy case. */
	consistency: {
		asg_symbols: number;
		fts_symbols: number;
		delta: number;
		consistent: boolean;
		advice: string;
	} | null;
	stale: { message: string; severity: string; age_secs: number } | null;
}

export interface ScoreSet {
	truth: number;
	feedback: number;
	change: number;
	uncertainty: number;
	workflow: number;
	overall: number;
}

export interface GapSymbol {
	qname: string;
	file: string;
	has_verified_effects: boolean;
	has_ownership: boolean;
	has_invariant: boolean;
	has_validation_scenario: boolean;
	ledger_entries: number;
	ctx_tagged: boolean;
}

export interface Scorecard {
	timestamp?: string;
	/** Present instead of the score blocks when nothing is indexed. */
	note?: string;
	capability_scores: ScoreSet;
	scores: ScoreSet;
	data_quality?: {
		ledger_density: number;
		symbols_scored: number;
		symbols_with_any_ledger: number;
		coverage_pct: number;
		sparse_db: boolean;
		note: string;
		scope: string[] | null;
	};
	details?: {
		total_symbols: number;
		verified_effects: number;
		owned_symbols: number;
		invariant_symbols: number;
		validation_symbols: number;
		feedback_entries: number;
		total_ledger_entries: number;
		ctx_tagged_ledger_entries: number;
	};
	token_economy?: {
		note: string;
		structured_tokens: number;
		source_read_tokens_est: number;
		reduction_pct: number;
		ratio_x: number;
	};
	drill_down?: {
		dimension: string;
		total_gaps: number;
		shown: number;
		omitted: number;
		gap_symbols: GapSymbol[];
	};
}

// -----------------------------------------------------------------------------
// Fetchers
// -----------------------------------------------------------------------------

/** Drop empty / undefined params so the URL reflects only active filters. */
function qs(params: Record<string, string | number | boolean | undefined | null>): string {
	const sp = new URLSearchParams();
	for (const [k, v] of Object.entries(params)) {
		if (v === undefined || v === null || v === '') continue;
		sp.set(k, String(v));
	}
	const s = sp.toString();
	return s ? `?${s}` : '';
}

async function getJson<T>(path: string): Promise<T> {
	const res = await fetch(`${API_BASE}${path}`);
	if (!res.ok) {
		// ApiError bodies are `{"error": "..."}` — unwrap so the page can show
		// the engine's own message instead of a bare status line.
		let detail = `${res.status} ${res.statusText}`;
		try {
			const body = (await res.json()) as { error?: string };
			if (body?.error) detail = `${detail} — ${body.error}`;
		} catch {
			/* non-JSON body; the status line is all we have */
		}
		throw new Error(`${detail} — ${path}`);
	}
	return (await res.json()) as T;
}

export interface RecordFilters {
	q?: string;
	from?: string;
	to?: string;
	limit?: number;
	offset?: number;
	kind?: string;
	namespace?: string;
	agent?: string;
	intent?: string;
	verdict?: string;
	author?: string;
	symbol?: string;
	milestone?: string;
	scan?: number;
}

export function getMilestones(f: RecordFilters = {}): Promise<MilestonePage> {
	return getJson<MilestonePage>(
		`/api/v1/history/milestones${qs({
			q: f.q,
			kind: f.kind,
			namespace: f.namespace,
			agent: f.agent,
			from: f.from,
			to: f.to,
			limit: f.limit,
			offset: f.offset
		})}`
	);
}

export function getRollup(f: RecordFilters = {}): Promise<RollupPage> {
	return getJson<RollupPage>(
		`/api/v1/history/rollup${qs({
			q: f.q,
			namespace: f.namespace,
			agent: f.agent,
			intent: f.intent,
			from: f.from,
			to: f.to,
			limit: f.limit,
			offset: f.offset
		})}`
	);
}

export function getCommits(f: RecordFilters = {}): Promise<CommitPage> {
	return getJson<CommitPage>(
		`/api/v1/commits${qs({
			q: f.q,
			agent: f.agent,
			intent: f.intent,
			milestone: f.milestone,
			from: f.from,
			to: f.to,
			limit: f.limit,
			offset: f.offset,
			scan: f.scan
		})}`
	);
}

export function getFeedback(f: RecordFilters = {}): Promise<FeedbackPage> {
	return getJson<FeedbackPage>(
		`/api/v1/feedback${qs({
			q: f.q,
			verdict: f.verdict,
			author: f.author,
			symbol: f.symbol,
			limit: f.limit,
			offset: f.offset
		})}`
	);
}

export function getIndexHealth(): Promise<IndexHealth> {
	return getJson<IndexHealth>('/api/v1/index-health');
}

// -----------------------------------------------------------------------------
// GC dry run
// -----------------------------------------------------------------------------

/** A computed estimate, plus the server's cache provenance. */
export type GcEstimate = GcDryRun & {
	cached?: boolean;
	computed_at?: string;
	age_secs?: number;
};

/** What `cached_only` returns when nothing is memoized for the current head. */
export interface GcUncomputed {
	status: 'uncomputed';
	reason: string;
}

export function isGcUncomputed(v: GcEstimate | GcUncomputed): v is GcUncomputed {
	return (v as GcUncomputed).status === 'uncomputed';
}

/**
 * Reclaim estimate.
 *
 * The server memoizes this on the ref head because computing it walks the
 * whole object DAG — ~26s on a large store. Call with `cachedOnly` to get the
 * memo if it's warm and a cheap `uncomputed` marker if it isn't, so a page can
 * render immediately either way and only start a walk when someone asks for
 * one.
 */
export function getGcDryRun(
	opts: { cachedOnly?: boolean; refresh?: boolean } = {}
): Promise<GcEstimate | GcUncomputed> {
	return getJson<GcEstimate | GcUncomputed>(
		`/api/v1/gc/dry-run${qs({
			cached_only: opts.cachedOnly ? '1' : undefined,
			refresh: opts.refresh ? '1' : undefined
		})}`
	);
}

export function getScorecard(
	opts: { scope?: string; paths?: string; drillDown?: string; limit?: number } = {}
): Promise<Scorecard> {
	return getJson<Scorecard>(
		`/api/v1/scorecard${qs({
			scope: opts.scope,
			paths: opts.paths,
			drill_down: opts.drillDown,
			limit: opts.limit
		})}`
	);
}

// -----------------------------------------------------------------------------
// Formatting helpers shared by the metrics pages
// -----------------------------------------------------------------------------

export function fmtBytes(n: number | null | undefined): string {
	if (n == null) return '—';
	const units = ['B', 'KB', 'MB', 'GB', 'TB'];
	let v = n;
	let i = 0;
	while (v >= 1024 && i < units.length - 1) {
		v /= 1024;
		i++;
	}
	return `${v >= 100 || i === 0 ? Math.round(v) : v.toFixed(1)} ${units[i]}`;
}

export function fmtNum(n: number | null | undefined): string {
	return n == null ? '—' : n.toLocaleString();
}

/** `2026-08-29T20:27:58Z` → `2026-08-29 20:27` — dense enough for a table. */
export function fmtTime(s: string | null | undefined): string {
	if (!s) return '—';
	try {
		return new Date(s).toISOString().replace('T', ' ').slice(0, 16);
	} catch {
		return s;
	}
}
