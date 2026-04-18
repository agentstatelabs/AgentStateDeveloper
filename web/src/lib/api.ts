import type { Health, SymbolSummary, SymbolDetail, LedgerEntry, EffectDecl } from './types';

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
