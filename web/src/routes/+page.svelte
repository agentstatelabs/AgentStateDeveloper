<script lang="ts">
	import { Badge } from '@agentstate/lens-core';
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

<svelte:head>
	<title>ASD Lens</title>
</svelte:head>

<div class="landing">
	<header class="page-header">
		<h1>AgentStateDeveloper — Lens</h1>
		<p class="page-desc">Code-level context and audit overlay for agent-authored code.</p>
	</header>

	<div class="card">
		<h2 class="micro-label">Status</h2>
		{#if err}
			<div class="state-error">API unreachable: {err}</div>
			<p class="hint">
				Start the HTTP server: <code>asd-serve</code>
			</p>
		{:else if health}
			<dl>
				<dt>Status</dt>
				<dd><Badge tone="ok">{health.status}</Badge></dd>
				<dt>Database</dt>
				<dd><code>{health.db_path}</code></dd>
				<dt>Indexed symbols</dt>
				<dd>{health.symbol_count.toLocaleString()}</dd>
			</dl>
		{:else}
			<div class="state-loading">loading…</div>
		{/if}
	</div>

	<div class="card">
		<h2 class="micro-label">Get started</h2>
		<p>Select a symbol from the left sidebar to view its declared effects and decision ledger.</p>
		<p class="hint">
			Index a new Python repo with <code>asd index &lt;path&gt;</code>, then refresh this page.
		</p>
	</div>
</div>

<style>
	.landing {
		max-width: 720px;
	}
	.card {
		background: var(--lens-surface);
		border: 1px solid var(--lens-border);
		border-radius: var(--lens-radius-md);
		padding: var(--lens-space-4) var(--lens-space-5);
		margin-bottom: var(--lens-space-4);
	}
	.card :global(h2.micro-label) {
		margin: 0 0 10px;
	}
	dl {
		display: grid;
		grid-template-columns: 180px 1fr;
		gap: 6px 12px;
		margin: 0;
	}
	dt {
		color: var(--lens-muted);
	}
	dd {
		margin: 0;
	}
	.hint {
		color: var(--lens-muted);
	}
</style>
