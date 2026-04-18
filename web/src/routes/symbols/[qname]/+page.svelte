<script lang="ts">
	import { getSymbolDetail, getCallers, getCallees } from '$lib/api';
	import type { SymbolDetail, SymbolSummary } from '$lib/types';
	import { symbols } from '$lib/stores';
	import { page } from '$app/state';

	let detail = $state<SymbolDetail | null>(null);
	let callers = $state<SymbolSummary[]>([]);
	let callees = $state<SymbolSummary[]>([]);
	let err = $state<string | null>(null);
	let loading = $state(true);

	let qname = $derived(decodeURIComponent(page.params.qname));

	$effect(() => {
		const q = qname;
		loading = true;
		err = null;
		detail = null;
		callers = [];
		callees = [];
		getSymbolDetail(q)
			.then((d) => {
				detail = d;
				loading = false;
			})
			.catch((e) => {
				err = String(e);
				loading = false;
			});
		getCallers(q).then((c) => (callers = c)).catch(() => {});
		getCallees(q).then((c) => (callees = c)).catch(() => {});
	});

	function fmtQualifiers(q: unknown): string | null {
		if (q == null) return null;
		if (typeof q === 'object' && Object.keys(q as object).length === 0) return null;
		try {
			return JSON.stringify(q);
		} catch {
			return null;
		}
	}

	function ts(s: string): string {
		try {
			return new Date(s).toISOString().replace('T', ' ').replace(/\..*$/, 'Z');
		} catch {
			return s;
		}
	}
</script>

{#if loading}
	<div class="muted">loading…</div>
{:else if err}
	<div class="error">{err}</div>
{:else if detail}
	{@const s = detail.symbol}
	<header class="sym-header">
		<div class="title">
			<span class="kind {s.kind}">{s.kind}</span>
			<h1>{s.qname}</h1>
		</div>
		<div class="loc">
			<code>{s.file}</code>
			<span class="range">:{s.start.line}-{s.end.line}</span>
			<span class="lang">{s.language}</span>
		</div>
		{#if s.signature}
			<pre class="sig"><code>{s.signature}</code></pre>
		{/if}
	</header>

	<section>
		<h2>Effects</h2>
		{#if detail.effects}
			{@const ed = detail.effects}
			{#if ed.verification}
				{@const v = ed.verification}
				<div class="verif status-{v.status}">
					<span class="label">verification</span>
					<span class="by">{v.by}</span>
					<span class="status">{v.status}</span>
					<span class="at">{ts(v.at)}</span>
				</div>
				{#if v.status === 'mismatch' && v.mismatches && v.mismatches.length > 0}
					<div class="mismatch-banner">
						<div class="mismatch-head">
							<strong>{v.mismatches.length} mismatch{v.mismatches.length === 1 ? '' : 'es'}</strong>
							<span class="muted"> — declared vs observed effects diverge</span>
						</div>
						<ul class="mismatch-list">
							{#each v.mismatches as m (JSON.stringify(m))}
								{@const mo = m as { effect: string; kind: string; note?: string | null; detected_in?: string | null }}
								<li>
									<span class="mm-effect">{mo.effect}</span>
									<span class="mm-kind mm-kind-{mo.kind}">{mo.kind}</span>
									{#if mo.note}
										<span class="mm-note">{mo.note}</span>
									{/if}
								</li>
							{/each}
						</ul>
					</div>
				{/if}
			{/if}
			{#if ed.declared.length === 0}
				<div class="muted">no declared effects</div>
			{:else}
				<ul class="effects">
					{#each ed.declared as eff}
						<li>
							<span class="eff-cat">{eff.effect}</span>
							{#if fmtQualifiers(eff.qualifiers)}
								<code class="qual">{fmtQualifiers(eff.qualifiers)}</code>
							{/if}
							{#if eff.note}
								<div class="note">{eff.note}</div>
							{/if}
						</li>
					{/each}
				</ul>
			{/if}
			{#if ed.transitive && ed.transitive.length > 0}
				<h3>Transitive</h3>
				<ul class="effects">
					{#each ed.transitive as t}
						<li>
							<span class="eff-cat">{t.effect}</span>
							<span class="via">
								via
								{#each t.via as vId, i}
									{@const vq = symbols.qnameOf(vId)}
									{#if vq}
										<a href="/symbols/{encodeURIComponent(vq)}">{vq}</a>
									{:else}
										<code>{vId}</code>
									{/if}
									{#if i < t.via.length - 1},{/if}
								{/each}
							</span>
						</li>
					{/each}
				</ul>
			{/if}
		{:else}
			<div class="muted">no effect record</div>
		{/if}
	</section>

	<section class="call-graph">
		<div class="cg-col">
			<h2>Called by ({callers.length})</h2>
			{#if callers.length === 0}
				<div class="muted">no known callers in index</div>
			{:else}
				<ul class="cg-list">
					{#each callers as c}
						<li>
							<a href="/symbols/{encodeURIComponent(c.qname)}">
								<span class="kind {c.kind}">{c.kind}</span>
								<span class="qname">{c.qname}</span>
							</a>
						</li>
					{/each}
				</ul>
			{/if}
		</div>
		<div class="cg-col">
			<h2>Calls ({callees.length})</h2>
			{#if callees.length === 0}
				<div class="muted">no known callees in index</div>
			{:else}
				<ul class="cg-list">
					{#each callees as c}
						<li>
							<a href="/symbols/{encodeURIComponent(c.qname)}">
								<span class="kind {c.kind}">{c.kind}</span>
								<span class="qname">{c.qname}</span>
							</a>
						</li>
					{/each}
				</ul>
			{/if}
		</div>
	</section>

	<section>
		<h2>Ledger</h2>
		{#if detail.ledger.length === 0}
			<div class="muted">
				no ledger entries — try
				<code>asd ledger append {s.qname} --kind decision --summary "..."</code>
			</div>
		{:else}
			<ul class="ledger">
				{#each detail.ledger as le}
					{@const tags = le.tags ?? []}
					{@const awaiting = tags.includes('awaiting-approval')}
					{@const approvers = tags
						.filter((t) => t.startsWith('approver:'))
						.map((t) => t.slice('approver:'.length))}
					<li class:awaiting>
						<div class="le-head">
							<span class="le-kind kind-{le.kind}">{le.kind}</span>
							{#if awaiting}
								<span class="le-awaiting" title="policy requires approval before this entry lands">
									AWAITING APPROVAL
								</span>
							{/if}
							<span class="le-summary">{le.summary}</span>
						</div>
						<div class="le-meta">
							<span>{le.author.kind}:{le.author.id}</span>
							<span>· {ts(le.created_at)}</span>
						</div>
						{#if le.matched_policy}
							<div class="le-policy">
								<span class="le-policy-label">policy</span>
								<code class="le-policy-path">{le.matched_policy}</code>
							</div>
						{/if}
						{#if approvers.length > 0}
							<div class="le-approvers">approvers: {approvers.join(', ')}</div>
						{/if}
						{#if le.body}
							<pre class="le-body">{le.body}</pre>
						{/if}
					</li>
				{/each}
			</ul>
		{/if}
	</section>
{/if}

<style>
	.sym-header {
		margin-bottom: 24px;
		border-bottom: 1px solid var(--border);
		padding-bottom: 16px;
	}
	.title {
		display: flex;
		align-items: baseline;
		gap: 12px;
	}
	h1 {
		margin: 0;
		font-size: 18px;
		font-weight: 600;
	}
	.loc {
		color: var(--fg-dim);
		font-size: 12px;
		margin-top: 4px;
		display: flex;
		gap: 10px;
	}
	.range {
		color: var(--fg-dim);
	}
	.lang {
		text-transform: uppercase;
		letter-spacing: 0.06em;
		font-size: 10px;
	}
	.sig {
		margin: 12px 0 0 0;
		padding: 10px 12px;
		background: var(--bg-alt);
		border-left: 3px solid var(--accent);
		border-radius: 4px;
		font-size: 12px;
		white-space: pre-wrap;
	}
	section {
		margin: 24px 0;
	}
	h2 {
		margin: 0 0 12px 0;
		font-size: 12px;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		color: var(--fg-dim);
	}
	h3 {
		margin: 16px 0 8px 0;
		font-size: 11px;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		color: var(--fg-dim);
	}
	.verif {
		display: flex;
		gap: 10px;
		align-items: baseline;
		padding: 6px 10px;
		margin-bottom: 10px;
		border-radius: 4px;
		font-size: 11px;
	}
	.verif .label {
		text-transform: uppercase;
		letter-spacing: 0.08em;
		color: var(--fg-dim);
	}
	.verif.status-ok {
		background: rgba(111, 207, 151, 0.12);
		color: var(--ok);
	}
	.verif.status-mismatch {
		background: rgba(224, 108, 117, 0.12);
		color: var(--bad);
	}
	.verif.status-unverified {
		background: var(--bg-alt);
		color: var(--fg-dim);
	}
	.effects {
		list-style: none;
		padding: 0;
		margin: 0;
	}
	.effects li {
		padding: 8px 12px;
		margin-bottom: 6px;
		background: var(--bg-alt);
		border: 1px solid var(--border);
		border-radius: 4px;
	}
	.eff-cat {
		color: var(--accent);
		font-weight: 600;
		margin-right: 10px;
	}
	.qual {
		color: var(--fg-dim);
		font-size: 11px;
	}
	.via {
		color: var(--fg-dim);
		font-size: 11px;
	}
	.note {
		color: var(--fg-dim);
		font-size: 11px;
		margin-top: 4px;
		font-family: "SF Mono", ui-monospace, monospace;
	}
	.ledger {
		list-style: none;
		padding: 0;
		margin: 0;
	}
	.ledger li {
		padding: 10px 14px;
		margin-bottom: 8px;
		background: var(--bg-alt);
		border: 1px solid var(--border);
		border-radius: 4px;
		border-left: 3px solid var(--accent);
	}
	.le-head {
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
	.le-summary {
		font-weight: 600;
	}
	.le-meta {
		color: var(--fg-dim);
		font-size: 11px;
		margin-top: 4px;
	}
	.le-body {
		margin: 8px 0 0 0;
		padding: 8px 10px;
		background: var(--bg);
		border-radius: 3px;
		font-size: 11px;
		white-space: pre-wrap;
	}
	.ledger li.awaiting {
		border-left-color: #ebcb8b;
	}
	.le-awaiting {
		font-size: 10px;
		font-weight: 700;
		letter-spacing: 0.1em;
		padding: 2px 7px;
		border-radius: 3px;
		background: rgba(235, 203, 139, 0.2);
		color: #ebcb8b;
		border: 1px solid rgba(235, 203, 139, 0.45);
	}
	.le-policy {
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
	.le-policy-label {
		color: var(--fg-dim);
		text-transform: uppercase;
		letter-spacing: 0.08em;
		font-size: 9px;
	}
	.le-policy-path {
		color: var(--fg-dim);
		font-family: "SF Mono", ui-monospace, monospace;
		font-size: 11px;
		background: transparent;
		padding: 0;
	}
	.le-approvers {
		margin-top: 4px;
		color: var(--fg-dim);
		font-size: 11px;
		font-style: italic;
	}
	.kind.function { color: var(--kind-function); }
	.kind.method { color: var(--kind-method); }
	.kind.class { color: var(--kind-class); }
	.kind.module { color: var(--kind-module); }
	.kind.variable { color: var(--kind-variable); }
	.kind {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.06em;
	}
	.muted {
		color: var(--fg-dim);
	}
	.error {
		color: var(--bad);
	}
	.call-graph {
		display: grid;
		grid-template-columns: 1fr 1fr;
		gap: 24px;
	}
	.cg-col {
		min-width: 0;
	}
	.cg-list {
		list-style: none;
		padding: 0;
		margin: 0;
	}
	.cg-list li a {
		display: grid;
		grid-template-columns: 60px 1fr;
		gap: 8px;
		padding: 6px 10px;
		border-radius: 4px;
		align-items: baseline;
	}
	.cg-list li a:hover {
		background: var(--bg-hover);
	}
	.mismatch-banner {
		margin: 8px 0 12px 0;
		padding: 8px 12px;
		background: rgba(224, 108, 117, 0.1);
		border: 1px solid rgba(224, 108, 117, 0.4);
		border-radius: 4px;
	}
	.mismatch-head {
		margin-bottom: 6px;
		color: var(--bad);
	}
	.mismatch-list {
		list-style: none;
		padding: 0;
		margin: 0;
	}
	.mismatch-list li {
		display: grid;
		grid-template-columns: 140px 110px 1fr;
		gap: 8px;
		padding: 3px 0;
		font-size: 11px;
		align-items: baseline;
	}
	.mm-effect {
		color: var(--accent);
	}
	.mm-kind {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		padding: 1px 6px;
		border-radius: 3px;
	}
	.mm-kind-unobserved {
		background: rgba(235, 203, 139, 0.18);
		color: #ebcb8b;
	}
	.mm-kind-undeclared {
		background: rgba(224, 108, 117, 0.18);
		color: var(--bad);
	}
	.mm-note {
		color: var(--fg-dim);
	}
	.via a {
		color: var(--accent);
		text-decoration: underline;
		text-decoration-style: dotted;
	}
	.via a:hover {
		text-decoration-style: solid;
	}
</style>
