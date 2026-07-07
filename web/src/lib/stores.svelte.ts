import type { SymbolSummary } from './types';

/// Svelte 5 module-level rune store. Shared across layout and routes so
/// we only fetch /api/v1/symbols once per session and can resolve
/// `symbol_id → qname` lookups (used for transitive `via` chains).

class SymbolsStore {
	list = $state<SymbolSummary[]>([]);
	loaded = $state(false);
	error = $state<string | null>(null);

	byId = $derived.by(() => {
		const m = new Map<string, SymbolSummary>();
		for (const s of this.list) m.set(s.symbol_id, s);
		return m;
	});

	set(list: SymbolSummary[]) {
		this.list = list;
		this.loaded = true;
		this.error = null;
	}

	setError(err: string) {
		this.error = err;
		this.loaded = true;
	}

	qnameOf(symbolId: string): string | null {
		return this.byId.get(symbolId)?.qname ?? null;
	}
}

export const symbols = new SymbolsStore();

/// Tracks the count of ledger entries awaiting approval. The layout polls
/// this once on mount so the sidebar badge stays cheap; the /approvals
/// route fetches the full list separately.
class ApprovalsStore {
	count = $state(0);
	loaded = $state(false);

	set(n: number) {
		this.count = n;
		this.loaded = true;
	}
}

export const approvals = new ApprovalsStore();
