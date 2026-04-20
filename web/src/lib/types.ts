export type SymbolKind = 'module' | 'function' | 'method' | 'class' | 'variable';

export interface Position {
	line: number;
	col: number;
}

export interface Symbol {
	symbol_id: string;
	symbol_fp: string;
	qname: string;
	language: string;
	kind: SymbolKind;
	file: string;
	start: Position;
	end: Position;
	signature: string | null;
}

export type EffectCategory =
	| 'io.fs.read'
	| 'io.fs.write'
	| 'io.net.in'
	| 'io.net.out'
	| 'io.db.read'
	| 'io.db.write'
	| 'state.global.read'
	| 'state.global.write'
	| 'state.process'
	| 'env.read'
	| 'time.read'
	| 'time.sleep'
	| 'random'
	| 'proc.spawn'
	| 'throw'
	| 'log'
	| 'pure';

export interface Effect {
	effect: EffectCategory;
	qualifiers: unknown;
	note: string | null;
}

export interface TransitiveEffect {
	effect: EffectCategory;
	via: string[];
	qualifiers: unknown;
}

export interface Verification {
	by: 'static-checker' | 'runtime-tracer' | 'test-observed';
	at: string;
	status: 'ok' | 'mismatch' | 'unverified';
	mismatches: unknown[];
}

export interface EffectDecl {
	symbol_id: string;
	declared: Effect[];
	transitive: TransitiveEffect[];
	verification: Verification | null;
	confidence: number | null;
	matched_policy: string | null;
}

export type LedgerKind =
	| 'decision'
	| 'assumption'
	| 'constraint'
	| 'rationale'
	| 'hazard'
	| 'tradeoff';

export interface Author {
	kind: 'agent' | 'human';
	id: string;
}

export interface LedgerEntry {
	entry_id: string;
	symbol_id: string;
	kind: LedgerKind;
	summary: string;
	body?: string;
	author: Author;
	confidence?: number;
	evidence?: unknown[];
	supersedes?: string[];
	created_at: string;
	tags?: string[];
	matched_policy?: string;
}

export interface Health {
	status: string;
	db_path: string;
	symbol_count: number;
}

export interface SymbolSummary {
	qname: string;
	symbol_id: string;
	kind: SymbolKind;
	language: string;
	file: string;
	start: Position;
	end: Position;
	signature: string | null;
}

export interface SymbolDetail {
	symbol: Symbol;
	effects: EffectDecl | null;
	ledger: LedgerEntry[];
}

export interface AuditEvent {
	event_id: string;
	event_type: string;
	subject_id?: string;
	secondary_id?: string;
	actor_id: string;
	actor_kind: string;
	timestamp: string;
	outcome: string;
	matched_policy?: string;
	reason?: string;
	payload?: unknown;
}

export interface AuditResponse {
	configured: boolean;
	path?: string;
	count: number;
	events: AuditEvent[];
}

export interface AuditFilters {
	eventType?: string;
	since?: string;
	actor?: string;
	outcome?: string;
	limit?: number;
}

export interface ChainBreak {
	index: number;
	event_id: string;
	reason: string;
}

export interface AuditVerifyReport {
	configured: boolean;
	path?: string;
	total_events: number;
	signed_events: number;
	unsigned_events: number;
	verified: boolean;
	chain_breaks: ChainBreak[];
}
