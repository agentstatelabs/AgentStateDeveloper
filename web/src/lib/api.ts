import type {
	Health,
	SymbolSummary,
	SymbolDetail,
	LedgerEntry,
	EffectDecl,
	AuditResponse,
	AuditFilters
} from './types';

// In dev we proxy /api/* via vite.config.ts → same-origin works.
// In prod the Rust HTTP server serves both the API and the built Lens.
export const API_BASE = '';

async function getJson<T>(path: string): Promise<T> {
	const res = await fetch(`${API_BASE}${path}`);
	if (!res.ok) {
		throw new Error(`${res.status} ${res.statusText} — ${path}`);
	}
	return (await res.json()) as T;
}

export function getHealth(): Promise<Health> {
	return getJson<Health>('/api/v1/health');
}

export function getSymbols(): Promise<SymbolSummary[]> {
	return getJson<SymbolSummary[]>('/api/v1/symbols');
}

export function getSymbolDetail(qname: string): Promise<SymbolDetail> {
	return getJson<SymbolDetail>(`/api/v1/symbols/${encodeURIComponent(qname)}`);
}

export function getLedger(qname: string): Promise<LedgerEntry[]> {
	return getJson<LedgerEntry[]>(`/api/v1/symbols/${encodeURIComponent(qname)}/ledger`);
}

export function getEffects(qname: string): Promise<EffectDecl | null> {
	return getJson<EffectDecl | null>(`/api/v1/symbols/${encodeURIComponent(qname)}/effects`);
}

export function getCallers(qname: string): Promise<SymbolSummary[]> {
	return getJson<SymbolSummary[]>(`/api/v1/symbols/${encodeURIComponent(qname)}/callers`);
}

export function getCallees(qname: string): Promise<SymbolSummary[]> {
	return getJson<SymbolSummary[]>(`/api/v1/symbols/${encodeURIComponent(qname)}/callees`);
}

/// Flat cross-symbol ledger listing. Optional tag filter (e.g. "awaiting-approval").
export function getLedgerByTag(tag?: string): Promise<LedgerEntry[]> {
	const q = tag ? `?tag=${encodeURIComponent(tag)}` : '';
	return getJson<LedgerEntry[]>(`/api/v1/ledger${q}`);
}

/// Convenience wrapper for the common "awaiting approval" queue view.
export function getAwaitingApproval(): Promise<LedgerEntry[]> {
	return getLedgerByTag('awaiting-approval');
}

export interface ApproveResponse {
	status: 'approved' | 'already-approved';
	entry: LedgerEntry;
}

export interface RejectResponse {
	status: 'rejected' | 'already-rejected';
	entry: LedgerEntry;
}

/// Approve a ledger entry currently tagged `awaiting-approval`.
export async function approveEntry(
	entryId: string,
	approver: string,
	approverKind: string = 'human',
	message?: string
): Promise<ApproveResponse> {
	const res = await fetch(
		`${API_BASE}/api/v1/approvals/${encodeURIComponent(entryId)}/approve`,
		{
			method: 'POST',
			headers: { 'Content-Type': 'application/json' },
			body: JSON.stringify({
				approver,
				approver_kind: approverKind,
				agent_id: 'asd-lens',
				...(message ? { message } : {})
			})
		}
	);
	if (!res.ok) {
		const text = await res.text();
		throw new Error(`${res.status} ${res.statusText} — ${text}`);
	}
	return (await res.json()) as ApproveResponse;
}

export function getAudit(filters: AuditFilters = {}): Promise<AuditResponse> {
	const params = new URLSearchParams();
	if (filters.eventType) params.set('event_type', filters.eventType);
	if (filters.since) params.set('since', filters.since);
	if (filters.actor) params.set('actor', filters.actor);
	if (filters.outcome) params.set('outcome', filters.outcome);
	if (filters.limit) params.set('limit', String(filters.limit));
	const qs = params.toString();
	return getJson<AuditResponse>(`/api/v1/audit${qs ? '?' + qs : ''}`);
}

/// Reject an awaiting-approval ledger entry.
export async function rejectEntry(
	entryId: string,
	reviewer: string,
	reason: string,
	reviewerKind: string = 'human'
): Promise<RejectResponse> {
	const res = await fetch(
		`${API_BASE}/api/v1/approvals/${encodeURIComponent(entryId)}/reject`,
		{
			method: 'POST',
			headers: { 'Content-Type': 'application/json' },
			body: JSON.stringify({
				reviewer,
				reviewer_kind: reviewerKind,
				reason,
				agent_id: 'asd-lens'
			})
		}
	);
	if (!res.ok) {
		const text = await res.text();
		throw new Error(`${res.status} ${res.statusText} — ${text}`);
	}
	return (await res.json()) as RejectResponse;
}
