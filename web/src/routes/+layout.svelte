<script lang="ts">
	import { getHealth, getSymbols } from '$lib/api';
	import type { Health, SymbolSummary } from '$lib/types';
	import { page } from '$app/state';

	let { children } = $props();

	let health = $state<Health | null>(null);
	let healthError = $state<string | null>(null);
	let symbols = $state<SymbolSummary[]>([]);
	let listError = $state<string | null>(null);

	$effect(() => {
		getHealth()
			.then((h) => (health = h))
			.catch((e) => (healthError = String(e)));
		getSymbols()
			.then((s) => (symbols = s))
			.catch((e) => (listError = String(e)));
	});

	let filter = $state('');
	let filtered = $derived(
		filter.trim()
			? symbols.filter((s) => s.qname.toLowerCase().includes(filter.toLowerCase()))
			: symbols
	);

	function kindBadge(k: string): string {
		return k;
	}

	function isActive(qname: string): boolean {
		const m = page.url.pathname.match(/^\/symbols\/(.+)$/);
		return m ? decodeURIComponent(m[1]) === qname : false;
	}
</script>

<div class="app">
	<header>
		<div class="brand">ASD Lens</div>
		<div class="health">
			{#if healthError}
				<span class="bad">offline</span>
			{:else if health}
				<span class="ok">{health.symbol_count} symbols</span>
				<span class="muted">· {health.db_path}</span>
			{:else}
				<span class="muted">loading…</span>
			{/if}
		</div>
	</header>

	<div class="body">
		<aside>
			<input
				type="text"
				placeholder="filter symbols…"
				bind:value={filter}
				class="filter"
			/>
			{#if listError}
				<div class="error">{listError}</div>
			{:else}
				<ul>
					{#each filtered as s (s.symbol_id)}
						<li class:active={isActive(s.qname)}>
							<a href="/symbols/{encodeURIComponent(s.qname)}">
								<span class="kind {s.kind}">{kindBadge(s.kind)}</span>
								<span class="qname">{s.qname}</span>
								<span class="file">{s.file}:{s.start.line}</span>
							</a>
						</li>
					{/each}
				</ul>
				{#if filtered.length === 0 && symbols.length > 0}
					<div class="muted empty">no match</div>
				{:else if symbols.length === 0 && !listError}
					<div class="muted empty">
						no symbols indexed — run <code>asd index .</code>
					</div>
				{/if}
			{/if}
		</aside>

		<main>
			{@render children()}
		</main>
	</div>
</div>

<style>
	:global(:root) {
		--bg: #0f1115;
		--bg-alt: #171a21;
		--bg-hover: #1f232c;
		--fg: #d8dde6;
		--fg-dim: #8690a0;
		--accent: #7aa2ff;
		--ok: #6fcf97;
		--bad: #e06c75;
		--border: #262a33;
		--kind-function: #88c0d0;
		--kind-method: #a3be8c;
		--kind-class: #d08770;
		--kind-module: #b48ead;
		--kind-variable: #ebcb8b;
	}
	:global(html, body) {
		margin: 0;
		padding: 0;
		background: var(--bg);
		color: var(--fg);
		font-family: -apple-system, BlinkMacSystemFont, "SF Mono", ui-monospace, "JetBrains Mono",
			Consolas, monospace;
		font-size: 13px;
		height: 100%;
	}
	:global(a) {
		color: inherit;
		text-decoration: none;
	}
	.app {
		display: flex;
		flex-direction: column;
		height: 100vh;
	}
	header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		padding: 10px 16px;
		border-bottom: 1px solid var(--border);
		background: var(--bg-alt);
	}
	.brand {
		font-weight: 700;
		letter-spacing: 0.04em;
	}
	.health .ok {
		color: var(--ok);
	}
	.health .bad {
		color: var(--bad);
	}
	.health .muted {
		color: var(--fg-dim);
		margin-left: 6px;
	}
	.body {
		display: flex;
		flex: 1;
		min-height: 0;
	}
	aside {
		width: 320px;
		border-right: 1px solid var(--border);
		overflow-y: auto;
		background: var(--bg-alt);
	}
	.filter {
		width: calc(100% - 24px);
		margin: 10px 12px;
		padding: 6px 8px;
		background: var(--bg);
		color: var(--fg);
		border: 1px solid var(--border);
		border-radius: 4px;
		font-family: inherit;
		font-size: 12px;
	}
	aside ul {
		list-style: none;
		margin: 0;
		padding: 0 0 40px 0;
	}
	aside li a {
		display: grid;
		grid-template-columns: 60px 1fr;
		padding: 6px 12px;
		border-left: 3px solid transparent;
		gap: 8px;
		align-items: baseline;
	}
	aside li a:hover {
		background: var(--bg-hover);
	}
	aside li.active a {
		background: var(--bg-hover);
		border-left-color: var(--accent);
	}
	.kind {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		opacity: 0.9;
	}
	.kind.function {
		color: var(--kind-function);
	}
	.kind.method {
		color: var(--kind-method);
	}
	.kind.class {
		color: var(--kind-class);
	}
	.kind.module {
		color: var(--kind-module);
	}
	.kind.variable {
		color: var(--kind-variable);
	}
	.qname {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.file {
		grid-column: 2;
		color: var(--fg-dim);
		font-size: 11px;
	}
	main {
		flex: 1;
		overflow-y: auto;
		padding: 24px;
	}
	.error {
		color: var(--bad);
		padding: 10px 12px;
	}
	.empty {
		padding: 10px 12px;
	}
	:global(code) {
		background: var(--bg);
		padding: 1px 5px;
		border-radius: 3px;
	}
</style>
