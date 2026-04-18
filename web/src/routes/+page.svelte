<script lang="ts">
	import { getHealth } from '$lib/api';
	import type { Health } from '$lib/types';

	let health = $state<Health | null>(null);
	let err = $state<string | null>(null);

	$effect(() => {
		getHealth()
			.then((h) => (health = h))
			.catch((e) => (err = String(e)));
	});
</script>

<div class="landing">
	<h1>AgentStateDeveloper — Lens</h1>
	<p class="tagline">Code-level context and audit overlay for agent-authored code.</p>

	<div class="card">
		<h2>Status</h2>
		{#if err}
			<div class="bad">API unreachable: {err}</div>
			<p class="muted">
				Start the HTTP server: <code>asd-serve</code>
			</p>
		{:else if health}
			<dl>
				<dt>Database</dt>
				<dd><code>{health.db_path}</code></dd>
				<dt>Indexed symbols</dt>
				<dd>{health.symbol_count}</dd>
			</dl>
		{:else}
			<div class="muted">loading…</div>
		{/if}
	</div>

	<div class="card">
		<h2>Get started</h2>
		<p>Select a symbol from the left sidebar to view its declared effects and decision ledger.</p>
		<p class="muted">
			Index a new Python repo with <code>asd index &lt;path&gt;</code>, then refresh this page.
		</p>
	</div>
</div>

<style>
	.landing {
		max-width: 720px;
	}
	h1 {
		margin: 0 0 6px 0;
		font-size: 20px;
	}
	.tagline {
		color: var(--fg-dim);
		margin: 0 0 24px 0;
	}
	.card {
		background: var(--bg-alt);
		border: 1px solid var(--border);
		border-radius: 6px;
		padding: 16px 20px;
		margin-bottom: 16px;
	}
	h2 {
		margin: 0 0 10px 0;
		font-size: 13px;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		color: var(--fg-dim);
	}
	dl {
		display: grid;
		grid-template-columns: 180px 1fr;
		gap: 6px 12px;
		margin: 0;
	}
	dt {
		color: var(--fg-dim);
	}
	dd {
		margin: 0;
	}
	.bad {
		color: var(--bad);
	}
	.muted {
		color: var(--fg-dim);
	}
</style>
