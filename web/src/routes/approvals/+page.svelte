<script lang="ts">
	import {
		getAwaitingApproval,
		approveEntry,
		rejectEntry,
		withdrawEntry,
		ApprovalActionError
	} from '$lib/api';
	import type { LedgerEntry } from '$lib/types';
	import { symbols, approvals } from '$lib/stores.svelte';

	type Action = 'approve' | 'reject' | 'withdraw';

	let entries = $state<LedgerEntry[]>([]);
	let err = $state<string | null>(null);
	let loading = $state(true);
	let approver = $state('reviewer-1');
	let approverKind = $state('human');
	let pendingId = $state<string | null>(null);
	let rowError = $state<Record<string, string>>({});
	/** Per-row flag: the last failure was the OSS "commercial feature" stub. */
	let rowEditionLocked = $state<Record<string, boolean>>({});
	let rowMessage = $state<Record<string, string>>({});
	let rowReason = $state<Record<string, string>>({});
	/** Two-step confirm: which (entry, action) is armed and awaiting a second click. */
	let confirming = $state<{ id: string; action: Action } | null>(null);
	/** Sticky page-level flag once ANY action reports the OSS stub. */
	let editionLocked = $state(false);

	async function refresh() {
		const list = await getAwaitingApproval();
		entries = list;
		approvals.set(list.length);
	}

	$effect(() => {
		loading = true;
		err = null;
		refresh()
			.then(() => (loading = false))
			.catch((e) => {
				err = String(e);
				loading = false;
			});
	});

	function setRowError(entryId: string, e: unknown) {
		const commercial = e instanceof ApprovalActionError && e.commercial;
		rowEditionLocked = { ...rowEditionLocked, [entryId]: commercial };
		if (commercial) editionLocked = true;
		rowError = {
			...rowError,
			[entryId]: e instanceof Error ? e.message : String(e)
		};
	}

	function clearRowError(entryId: string) {
		rowError = { ...rowError, [entryId]: '' };
		rowEditionLocked = { ...rowEditionLocked, [entryId]: false };
	}

	/** First click arms the confirm; the second click performs the action. */
	function requestAction(entryId: string, action: Action): boolean {
		if (confirming?.id === entryId && confirming.action === action) {
			confirming = null;
			return true;
		}
		confirming = { id: entryId, action };
		return false;
	}

	function isConfirming(entryId: string, action: Action): boolean {
		return confirming?.id === entryId && confirming.action === action;
	}

	async function run(entryId: string, work: () => Promise<unknown>) {
		pendingId = entryId;
		clearRowError(entryId);
		try {
			await work();
			await refresh();
		} catch (e) {
			setRowError(entryId, e);
		} finally {
			pendingId = null;
		}
	}

	function approve(entryId: string) {
		if (!requestAction(entryId, 'approve')) return;
		run(entryId, () =>
			approveEntry(entryId, approver, approverKind, rowMessage[entryId] || undefined)
		);
	}

	function reject(entryId: string) {
		const reason = (rowReason[entryId] || '').trim();
		if (!reason) {
			confirming = null;
			rowError = { ...rowError, [entryId]: 'reason is required to reject' };
			return;
		}
		if (!requestAction(entryId, 'reject')) return;
		run(entryId, () => rejectEntry(entryId, approver, reason, approverKind));
	}

	function withdraw(entry: LedgerEntry) {
		if (!requestAction(entry.entry_id, 'withdraw')) return;
		// Withdraw is the author retracting their own proposal — the server
		// validates author_id against the entry, so we send the recorded author.
		run(entry.entry_id, () => withdrawEntry(entry.entry_id, entry.author.id));
	}

	function ts(s: string): string {
		try {
			return new Date(s).toISOString().replace('T', ' ').replace(/\..*$/, 'Z');
		} catch {
			return s;
		}
	}

	function approversFor(tags: string[] | undefined): string[] {
		if (!tags) return [];
		return tags.filter((t) => t.startsWith('approver:')).map((t) => t.slice('approver:'.length));
	}
</script>

<header class="q-header">
	<div class="title-row">
		<h1>Awaiting approval</h1>
		<nav class="tabs" aria-label="approvals views">
			<a class="tab active" href="/approvals" aria-current="page">Queue</a>
			<a class="tab" href="/approvals/history">History</a>
		</nav>
	</div>
	<p class="muted">
		Ledger entries gated by a policy that requires human or senior-agent attestation before they land.
	</p>
	{#if editionLocked}
		<div class="edition-banner">
			<strong>Approval actions are not available on this server.</strong>
			<span>
				This <code>asd-serve</code> is the OSS edition — approve / reject / withdraw are Team-tier
				features (<code>asd-pro</code>). The queue below is read-only here.
			</span>
		</div>
	{/if}
	<div class="approver-bar">
		<label>
			Approver
			<input type="text" bind:value={approver} class="approver-input" />
		</label>
		<label>
			Kind
			<select bind:value={approverKind} class="approver-kind">
				<option value="human">human</option>
				<option value="senior_agent">senior_agent</option>
			</select>
		</label>
	</div>
</header>

{#if loading}
	<div class="muted">loading…</div>
{:else if err}
	<div class="error">{err}</div>
{:else if entries.length === 0}
	<div class="muted empty">No entries awaiting approval.</div>
{:else}
	<ul class="queue">
		{#each entries as le (le.entry_id)}
			{@const qname = symbols.qnameOf(le.symbol_id)}
			{@const approvers = approversFor(le.tags)}
			<li>
				<div class="row-head">
					<span class="le-kind kind-{le.kind}">{le.kind}</span>
					<span class="summary">{le.summary}</span>
				</div>
				<div class="row-meta">
					{#if qname}
						<a class="qname" href="/symbols/{encodeURIComponent(qname)}">{qname}</a>
					{:else}
						<code class="sid">{le.symbol_id}</code>
					{/if}
					<span class="sep">·</span>
					<span>{le.author.kind}:{le.author.id}</span>
					<span class="sep">·</span>
					<span>{ts(le.created_at)}</span>
				</div>
				{#if le.matched_policy}
					<div class="row-policy">
						<span class="policy-label">policy</span>
						<code>{le.matched_policy}</code>
					</div>
				{/if}
				{#if approvers.length > 0}
					<div class="row-approvers">approvers: {approvers.join(', ')}</div>
				{/if}
				<div class="row-inputs">
					<input
						type="text"
						class="row-input"
						placeholder="optional approval note"
						bind:value={rowMessage[le.entry_id]}
						disabled={pendingId === le.entry_id}
					/>
					<input
						type="text"
						class="row-input"
						placeholder="rejection reason (required to reject)"
						bind:value={rowReason[le.entry_id]}
						disabled={pendingId === le.entry_id}
					/>
				</div>
				<div class="row-actions">
					<button
						class="approve-btn"
						class:arm={isConfirming(le.entry_id, 'approve')}
						disabled={pendingId === le.entry_id}
						onclick={() => approve(le.entry_id)}
					>
						{#if pendingId === le.entry_id}working…{:else if isConfirming(le.entry_id, 'approve')}confirm approve?{:else}approve{/if}
					</button>
					<button
						class="reject-btn"
						class:arm={isConfirming(le.entry_id, 'reject')}
						disabled={pendingId === le.entry_id}
						onclick={() => reject(le.entry_id)}
					>
						{isConfirming(le.entry_id, 'reject') ? 'confirm reject?' : 'reject'}
					</button>
					<button
						class="withdraw-btn"
						class:arm={isConfirming(le.entry_id, 'withdraw')}
						disabled={pendingId === le.entry_id}
						title="Retract this proposal as its author ({le.author.id})"
						onclick={() => withdraw(le)}
					>
						{isConfirming(le.entry_id, 'withdraw') ? 'confirm withdraw?' : 'withdraw'}
					</button>
					{#if confirming?.id === le.entry_id}
						<button class="cancel-btn" onclick={() => (confirming = null)}>cancel</button>
					{/if}
				</div>
				{#if rowError[le.entry_id]}
					{#if rowEditionLocked[le.entry_id]}
						<div class="row-notice edition">
							<span class="notice-label">team tier</span>
							{rowError[le.entry_id]}
						</div>
					{:else}
						<div class="row-notice err">{rowError[le.entry_id]}</div>
					{/if}
				{/if}
			</li>
		{/each}
	</ul>
{/if}

<style>
	.q-header {
		margin-bottom: 20px;
		border-bottom: 1px solid var(--border);
		padding-bottom: 12px;
	}
	h1 {
		margin: 0;
		font-size: 18px;
		font-weight: 600;
	}
	.title-row {
		display: flex;
		align-items: baseline;
		gap: 16px;
	}
	.tabs {
		display: flex;
		gap: 4px;
	}
	.tab {
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		font-weight: 600;
		padding: 3px 10px;
		border-radius: 999px;
		border: 1px solid transparent;
		color: var(--fg-dim);
	}
	.tab:hover {
		color: var(--accent);
	}
	.tab.active {
		color: var(--accent);
		border-color: rgba(122, 162, 255, 0.4);
		background: rgba(122, 162, 255, 0.08);
	}
	.edition-banner {
		margin-top: 10px;
		padding: 8px 12px;
		background: rgba(235, 203, 139, 0.08);
		border: 1px solid rgba(235, 203, 139, 0.3);
		color: #ebcb8b;
		border-radius: 4px;
		font-size: 12px;
	}
	.edition-banner strong {
		margin-right: 6px;
	}
	.muted {
		color: var(--fg-dim);
	}
	.q-header .muted {
		margin: 4px 0 0 0;
		font-size: 12px;
	}
	.error {
		color: var(--bad);
	}
	.empty {
		padding: 24px 0;
	}
	.queue {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.queue li {
		padding: 10px 14px;
		margin-bottom: 8px;
		background: var(--bg-alt);
		border: 1px solid var(--border);
		border-left: 3px solid #ebcb8b;
		border-radius: 4px;
	}
	.row-head {
		display: flex;
		gap: 10px;
		align-items: baseline;
	}
	.le-kind {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		padding: 2px 6px;
		border-radius: 3px;
		background: var(--bg);
	}
	.kind-hazard {
		background: rgba(224, 108, 117, 0.18);
		color: var(--bad);
	}
	.kind-decision {
		background: rgba(122, 162, 255, 0.18);
		color: var(--accent);
	}
	.kind-constraint,
	.kind-assumption {
		background: rgba(235, 203, 139, 0.18);
		color: #ebcb8b;
	}
	.kind-rationale,
	.kind-tradeoff {
		background: var(--bg);
		color: var(--fg-dim);
	}
	.summary {
		font-weight: 600;
	}
	.row-meta {
		color: var(--fg-dim);
		font-size: 11px;
		margin-top: 4px;
		display: flex;
		gap: 6px;
		align-items: baseline;
		flex-wrap: wrap;
	}
	.row-meta .qname {
		color: var(--accent);
		text-decoration: underline;
		text-decoration-style: dotted;
	}
	.row-meta .qname:hover {
		text-decoration-style: solid;
	}
	.row-meta .sep {
		opacity: 0.6;
	}
	.sid {
		font-family: "SF Mono", ui-monospace, monospace;
		font-size: 11px;
	}
	.row-policy {
		margin-top: 6px;
		display: inline-flex;
		align-items: baseline;
		gap: 6px;
		padding: 3px 8px;
		background: var(--bg);
		border: 1px solid var(--border);
		border-radius: 10px;
		font-size: 11px;
	}
	.row-policy code {
		background: transparent;
		padding: 0;
		color: var(--fg-dim);
	}
	.policy-label {
		color: var(--fg-dim);
		text-transform: uppercase;
		letter-spacing: 0.08em;
		font-size: 9px;
	}
	.row-approvers {
		margin-top: 4px;
		color: var(--fg-dim);
		font-size: 11px;
		font-style: italic;
	}
	.approver-bar {
		display: flex;
		gap: 16px;
		margin-top: 10px;
		align-items: baseline;
		font-size: 11px;
		color: var(--fg-dim);
	}
	.approver-bar label {
		display: flex;
		gap: 6px;
		align-items: baseline;
	}
	.approver-input,
	.approver-kind {
		background: var(--bg);
		color: var(--fg);
		border: 1px solid var(--border);
		border-radius: 3px;
		padding: 3px 6px;
		font-family: inherit;
		font-size: 11px;
	}
	.row-actions {
		margin-top: 8px;
		display: flex;
		gap: 10px;
		align-items: baseline;
	}
	.approve-btn {
		background: rgba(111, 207, 151, 0.15);
		color: var(--ok);
		border: 1px solid rgba(111, 207, 151, 0.4);
		border-radius: 3px;
		padding: 4px 12px;
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		font-weight: 600;
		cursor: pointer;
		font-family: inherit;
	}
	.approve-btn:hover:not(:disabled) {
		background: rgba(111, 207, 151, 0.25);
	}
	.approve-btn:disabled,
	.reject-btn:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
	.reject-btn {
		background: rgba(224, 108, 117, 0.15);
		color: var(--bad);
		border: 1px solid rgba(224, 108, 117, 0.4);
		border-radius: 3px;
		padding: 4px 12px;
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		font-weight: 600;
		cursor: pointer;
		font-family: inherit;
	}
	.reject-btn:hover:not(:disabled) {
		background: rgba(224, 108, 117, 0.25);
	}
	.withdraw-btn {
		background: rgba(235, 203, 139, 0.12);
		color: #ebcb8b;
		border: 1px solid rgba(235, 203, 139, 0.4);
		border-radius: 3px;
		padding: 4px 12px;
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		font-weight: 600;
		cursor: pointer;
		font-family: inherit;
	}
	.withdraw-btn:hover:not(:disabled) {
		background: rgba(235, 203, 139, 0.22);
	}
	.withdraw-btn:disabled {
		opacity: 0.5;
		cursor: not-allowed;
	}
	/* Armed confirm state — the second click commits. */
	.approve-btn.arm,
	.reject-btn.arm,
	.withdraw-btn.arm {
		outline: 1px solid currentColor;
		outline-offset: 1px;
	}
	.cancel-btn {
		background: transparent;
		color: var(--fg-dim);
		border: 1px solid var(--border);
		border-radius: 3px;
		padding: 4px 10px;
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		cursor: pointer;
		font-family: inherit;
	}
	.cancel-btn:hover {
		color: var(--fg);
	}
	.row-notice {
		margin-top: 8px;
		font-size: 11px;
		padding: 6px 10px;
		border-radius: 3px;
	}
	.row-notice.err {
		color: var(--bad);
		background: rgba(224, 108, 117, 0.08);
		border: 1px solid rgba(224, 108, 117, 0.3);
	}
	.row-notice.edition {
		color: #ebcb8b;
		background: rgba(235, 203, 139, 0.08);
		border: 1px solid rgba(235, 203, 139, 0.3);
	}
	.notice-label {
		display: inline-block;
		font-size: 9px;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		font-weight: 700;
		padding: 1px 6px;
		margin-right: 8px;
		border-radius: 999px;
		border: 1px solid rgba(235, 203, 139, 0.45);
	}
	.row-inputs {
		margin-top: 8px;
		display: flex;
		gap: 8px;
		flex-wrap: wrap;
	}
	.row-input {
		flex: 1 1 220px;
		min-width: 180px;
		background: var(--bg);
		color: var(--fg);
		border: 1px solid var(--border);
		border-radius: 3px;
		padding: 4px 8px;
		font-family: inherit;
		font-size: 11px;
	}
</style>
