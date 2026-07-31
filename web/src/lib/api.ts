import { createAsdClient, createHttpTransport, type AsdClient } from '@agentstate/lens-core';
import type {
	Health,
	SymbolSummary,
	LedgerEntry,
	AuditResponse,
	AuditFilters,
	AuditVerifyReport
} from './types';

// In dev we proxy /api/* via vite.config.ts → same-origin works.
// In prod the Rust HTTP server serves both the API and the built Lens.
export const API_BASE = '';

/** Typed ASD client for the shared lens-core components (same-origin /api/v1). */
export const asdClient: AsdClient = createAsdClient(createHttpTransport(`${API_BASE}/api/v1`));

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

/**
 * Snapshot-first symbol listing (territory prototype). `/api/v1/symbols`
 * resolves every qname individually — measured 8m45s for a 2000-row page on
 * the 9.8k-symbol AcmeProj index — and it holds the engine mutex while doing
 * so, starving every other API call. Until that endpoint uses the bulk id
 * map, prefer the setup-time snapshot and fall back to the live API.
 */
export async function getSymbolsFast(): Promise<SymbolSummary[]> {
	try {
		const res = await fetch('/territory-symbols.json');
		if (res.ok) {
			const snap = (await res.json()) as {
				id: string;
				q: string;
				f: string;
				k: string;
				l: number;
			}[];
			if (Array.isArray(snap) && snap.length > 0) {
				return snap.map((s) => ({
					symbol_id: s.id,
					qname: s.q,
					file: s.f,
					kind: s.k,
					start: { line: s.l, col: 0 },
					language: 'swift'
				})) as unknown as SymbolSummary[];
			}
		}
	} catch {
		/* fall through */
	}
	return getSymbols();
}

// Symbol detail / callers / callees / ledger / effects moved to the shared
// AsdClient (`asdClient` above) consumed by the lens-core SymbolDetail view.

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

export interface WithdrawResponse {
	status: 'withdrawn' | 'already-withdrawn';
	entry: LedgerEntry;
}

/**
 * Error from an approval-family POST. OSS asd-serve has no ratify engine:
 * approve/reject/withdraw all return 500 with
 * `{"error": "... is a commercial feature (Team tier) — install asd-pro ..."}`.
 * `commercial` is true for that case so the UI can present "not available
 * in this edition" honestly instead of a generic failure.
 */
export class ApprovalActionError extends Error {
	status: number;
	commercial: boolean;

	constructor(status: number, message: string) {
		super(message);
		this.name = 'ApprovalActionError';
		this.status = status;
		this.commercial = message.includes('commercial feature');
	}
}

async function postApproval<T>(entryId: string, action: string, body: unknown): Promise<T> {
	const res = await fetch(
		`${API_BASE}/api/v1/approvals/${encodeURIComponent(entryId)}/${action}`,
		{
			method: 'POST',
			headers: { 'Content-Type': 'application/json' },
			body: JSON.stringify(body)
		}
	);
	if (!res.ok) {
		const text = await res.text();
		// ApiError bodies are `{"error": "..."}` — unwrap for readable UI.
		let message = `${res.status} ${res.statusText}`;
		try {
			const parsed = JSON.parse(text) as { error?: string };
			message = parsed.error ?? `${message} — ${text}`;
		} catch {
			if (text) message = `${message} — ${text}`;
		}
		throw new ApprovalActionError(res.status, message);
	}
	return (await res.json()) as T;
}

/// Approve a ledger entry currently tagged `awaiting-approval`.
export function approveEntry(
	entryId: string,
	approver: string,
	approverKind: string = 'human',
	message?: string
): Promise<ApproveResponse> {
	return postApproval<ApproveResponse>(entryId, 'approve', {
		approver,
		approver_kind: approverKind,
		agent_id: 'asd-lens',
		...(message ? { message } : {})
	});
}

export function getAuditVerify(): Promise<AuditVerifyReport> {
	return getJson<AuditVerifyReport>('/api/v1/audit/verify');
}

export function getAudit(filters: AuditFilters = {}): Promise<AuditResponse> {
	const params = new URLSearchParams();
	if (filters.eventType) params.set('event_type', filters.eventType);
	if (filters.since) params.set('since', filters.since);
	if (filters.actor) params.set('actor', filters.actor);
	if (filters.outcome) params.set('outcome', filters.outcome);
	if (filters.subject) params.set('subject', filters.subject);
	if (filters.limit) params.set('limit', String(filters.limit));
	const qs = params.toString();
	return getJson<AuditResponse>(`/api/v1/audit${qs ? '?' + qs : ''}`);
}

/// Reject an awaiting-approval ledger entry.
export function rejectEntry(
	entryId: string,
	reviewer: string,
	reason: string,
	reviewerKind: string = 'human'
): Promise<RejectResponse> {
	return postApproval<RejectResponse>(entryId, 'reject', {
		reviewer,
		reviewer_kind: reviewerKind,
		reason,
		agent_id: 'asd-lens'
	});
}

/// Withdraw an awaiting-approval ledger entry (author retracts their own).
export function withdrawEntry(entryId: string, authorId: string): Promise<WithdrawResponse> {
	return postApproval<WithdrawResponse>(entryId, 'withdraw', {
		author_id: authorId,
		agent_id: 'asd-lens'
	});
}
