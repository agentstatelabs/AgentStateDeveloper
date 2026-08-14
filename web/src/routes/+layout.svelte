<script lang="ts">
	import '@agentstate/lens-core/tokens.css';
	import '../app.css';
	import { getHealth, getSymbolsFast, getAwaitingApproval } from '$lib/api';
	import type { Health } from '$lib/types';
	import { symbols, approvals } from '$lib/stores.svelte';
	import { page } from '$app/state';

	let { children } = $props();

	let health = $state<Health | null>(null);
	let healthError = $state<string | null>(null);

	$effect(() => {
		getHealth()
			.then((h) => (health = h))
			.catch((e) => (healthError = String(e)));
		getSymbolsFast()
			.then((s) => symbols.set(s))
			.catch((e) => symbols.setError(String(e)));
		getAwaitingApproval()
			.then((list) => approvals.set(list.length))
			.catch(() => approvals.set(0));
	});

	let filter = $state('');
	// Search the full list but cap the rendered rows — at 9.8k symbols an
	// unvirtualized sidebar would add ~40k DOM nodes to every page.
	const SIDEBAR_MAX = 500;
	let matched = $derived(
		filter.trim()
			? symbols.list.filter((s) => s.qname.toLowerCase().includes(filter.toLowerCase()))
			: symbols.list
	);
	let filtered = $derived(matched.slice(0, SIDEBAR_MAX));
	let overflow = $derived(Math.max(0, matched.length - SIDEBAR_MAX));

	const NAV = [
		{ href: '/activity', label: 'Activity' },
		{ href: '/history', label: 'History' },
		{ href: '/territory', label: 'Territory' },
		{ href: '/graph', label: 'Graph' },
		{ href: '/effects', label: 'Effects' },
		{ href: '/approvals', label: 'Approvals' },
		{ href: '/audit', label: 'Audit' }
	];

	function navActive(href: string): boolean {
		return page.url.pathname === href || page.url.pathname.startsWith(href + '/');
	}

	function isActive(qname: string): boolean {
		const m = page.url.pathname.match(/^\/symbols\/(.+)$/);
		return m ? decodeURIComponent(m[1]) === qname : false;
	}
</script>

<div class="app">
	<header class="shell-header">
		<div class="brand">
			<a href="/">ASD <span class="brand-lens">Lens</span></a>
			{#if approvals.count > 0}
				<a class="approvals-badge" href="/approvals" title="Ledger entries awaiting approval">
					{approvals.count} awaiting approval
				</a>
			{/if}
		</div>
		<nav class="top-links" aria-label="primary">
			{#each NAV as n (n.href)}
				<a href={n.href} class:active={navActive(n.href)} aria-current={navActive(n.href) ? 'page' : undefined}>
					{n.label}
				</a>
			{/each}
		</nav>
		<div class="health">
			{#if healthError}
				<span class="health-pill bad"><span class="dot"></span>offline</span>
			{:else if health}
				<span class="health-pill ok"><span class="dot"></span>{health.symbol_count.toLocaleString()} symbols</span>
				<span class="db-path" title={health.db_path}>{health.db_path}</span>
			{:else}
				<span class="health-pill"><span class="dot"></span>loading…</span>
			{/if}
		</div>
	</header>

	<div class="body">
		<aside>
			{#if approvals.count > 0}
				<a class="side-approvals" href="/approvals">
					<span class="dot"></span>
					<span class="lbl">awaiting approval</span>
					<span class="num">{approvals.count}</span>
				</a>
			{/if}
			<input
				type="text"
				placeholder="filter symbols…"
				bind:value={filter}
				class="filter"
			/>
			{#if symbols.error}
				<div class="state-error side-state">{symbols.error}</div>
			{:else}
				<ul>
					{#each filtered as s (s.symbol_id)}
						<li class:active={isActive(s.qname)}>
							<a href="/symbols/{encodeURIComponent(s.qname)}">
								<span class="kind {s.kind}">{s.kind}</span>
								<span class="qname">{s.qname}</span>
								<span class="file">{s.file}:{s.start.line}</span>
							</a>
						</li>
					{/each}
				</ul>
				{#if overflow > 0}
					<div class="side-note">…{overflow.toLocaleString()} more — refine the filter</div>
				{/if}
				{#if filtered.length === 0 && symbols.list.length > 0}
					<div class="side-note">no symbol matches “{filter.trim()}”</div>
				{:else if symbols.list.length === 0 && !symbols.error}
					<div class="side-note">
						no symbols indexed yet — run <code>asd index .</code> and refresh
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
	/* Legacy palette bridge — earlier pages (and the territory prototypes)
	   still reference the pre-system --bg/--fg variables; they now resolve
	   to the lens-core tokens so the whole app themes as one. */
	:global(:root) {
		--bg: var(--lens-bg);
		--bg-alt: var(--lens-surface);
		--bg-hover: var(--lens-surface-raised);
		--fg: var(--lens-text);
		--fg-dim: var(--lens-muted);
		--accent: var(--lens-accent);
		--ok: var(--lens-ok);
		--bad: var(--lens-danger);
		--border: var(--lens-border);
		--kind-function: var(--lens-kind-function);
		--kind-method: var(--lens-kind-method);
		--kind-class: var(--lens-kind-class);
		--kind-module: var(--lens-kind-module);
		--kind-variable: var(--lens-kind-variable);
	}

	.app {
		display: flex;
		flex-direction: column;
		height: 100vh;
	}

	/* -- top bar -------------------------------------------------------- */
	.shell-header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--lens-space-4);
		padding: 10px var(--lens-space-4);
		border-bottom: 1px solid var(--lens-border);
		background: var(--lens-surface);
	}
	.brand {
		display: flex;
		align-items: center;
		gap: var(--lens-space-2);
		font-weight: 700;
		letter-spacing: 0.02em;
		color: var(--lens-text-strong);
		white-space: nowrap;
	}
	.brand-lens {
		color: var(--lens-accent);
		font-weight: 600;
	}
	.approvals-badge {
		font-weight: 600;
		font-size: var(--lens-font-size-2xs);
		letter-spacing: 0;
		padding: 2px 9px;
		border-radius: var(--lens-radius-full);
		background: var(--lens-warn-tint);
		color: var(--lens-warn);
		border: 1px solid var(--lens-warn-border);
		transition: background var(--lens-dur-fast) var(--lens-ease);
	}
	.approvals-badge:hover {
		background: color-mix(in srgb, var(--lens-warn) 16%, transparent);
	}
	.top-links {
		display: flex;
		gap: var(--lens-space-1);
		font-size: var(--lens-font-size-xs);
	}
	.top-links a {
		color: var(--lens-muted);
		font-weight: 500;
		padding: 3px 10px;
		border-radius: var(--lens-radius-full);
		transition:
			color var(--lens-dur-fast) var(--lens-ease),
			background var(--lens-dur-fast) var(--lens-ease);
	}
	.top-links a:hover {
		color: var(--lens-text);
		background: var(--lens-surface-raised);
	}
	.top-links a.active {
		color: var(--lens-accent);
		background: var(--lens-accent-tint);
		font-weight: 600;
	}

	.health {
		display: flex;
		align-items: center;
		gap: var(--lens-space-2);
		min-width: 0;
	}
	.health-pill {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		font-size: var(--lens-font-size-2xs);
		font-weight: 600;
		padding: 2px 9px;
		border-radius: var(--lens-radius-full);
		border: 1px solid var(--lens-border);
		color: var(--lens-muted);
		white-space: nowrap;
	}
	.health-pill .dot {
		width: 6px;
		height: 6px;
		border-radius: 50%;
		background: currentColor;
	}
	.health-pill.ok {
		color: var(--lens-ok);
		border-color: var(--lens-ok-border);
		background: var(--lens-ok-tint);
	}
	.health-pill.bad {
		color: var(--lens-danger);
		border-color: var(--lens-danger-border);
		background: var(--lens-danger-tint);
	}
	.db-path {
		color: var(--lens-text-faint);
		font-family: var(--lens-font-mono);
		font-size: var(--lens-font-size-2xs);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		max-width: 28ch;
	}

	/* -- sidebar ---------------------------------------------------------- */
	.body {
		display: flex;
		flex: 1;
		min-height: 0;
	}
	aside {
		width: 320px;
		flex-shrink: 0;
		border-right: 1px solid var(--lens-border);
		overflow-y: auto;
		background: var(--lens-surface);
	}
	.side-approvals {
		display: flex;
		align-items: center;
		gap: var(--lens-space-2);
		margin: 10px 12px 0 12px;
		padding: 6px 10px;
		background: var(--lens-warn-tint);
		border: 1px solid var(--lens-warn-border);
		border-radius: var(--lens-radius-sm);
		color: var(--lens-warn);
		font-size: var(--lens-font-size-2xs);
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: var(--lens-tracking-caps);
		transition: background var(--lens-dur-fast) var(--lens-ease);
	}
	.side-approvals:hover {
		background: color-mix(in srgb, var(--lens-warn) 15%, transparent);
	}
	.side-approvals .dot {
		width: 6px;
		height: 6px;
		border-radius: 50%;
		background: currentColor;
	}
	.side-approvals .lbl {
		flex: 1;
	}
	.side-approvals .num {
		font-weight: 700;
		font-size: var(--lens-font-size-xs);
	}
	.filter {
		width: calc(100% - 24px);
		margin: 10px 12px;
		padding: 6px 9px;
		background: var(--lens-bg);
		color: var(--lens-text);
		border: 1px solid var(--lens-border);
		border-radius: var(--lens-radius-sm);
		font-family: var(--lens-font-sans);
		font-size: var(--lens-font-size-xs);
		box-sizing: border-box;
		transition: border-color var(--lens-dur-fast) var(--lens-ease);
	}
	.filter::placeholder {
		color: var(--lens-text-faint);
	}
	.filter:focus {
		outline: none;
		border-color: var(--lens-accent);
		box-shadow: 0 0 0 3px var(--lens-accent-tint);
	}
	aside ul {
		list-style: none;
		margin: 0;
		padding: 0 0 40px 0;
	}
	aside li a {
		display: grid;
		grid-template-columns: 60px 1fr;
		padding: 5px 12px;
		border-left: 2px solid transparent;
		gap: var(--lens-space-2);
		align-items: baseline;
		transition: background var(--lens-dur-fast) var(--lens-ease);
	}
	aside li a:hover {
		background: var(--lens-surface-raised);
	}
	aside li a:focus-visible {
		outline: 2px solid var(--lens-focus);
		outline-offset: -2px;
	}
	aside li.active a {
		background: var(--lens-surface-raised);
		border-left-color: var(--lens-accent);
	}
	.kind {
		font-size: 10px;
		text-transform: uppercase;
		letter-spacing: var(--lens-tracking-wide);
		color: var(--lens-muted);
	}
	.kind.function {
		color: var(--lens-kind-function);
	}
	.kind.method {
		color: var(--lens-kind-method);
	}
	.kind.class {
		color: var(--lens-kind-class);
	}
	.kind.module {
		color: var(--lens-kind-module);
	}
	.kind.variable {
		color: var(--lens-kind-variable);
	}
	.qname {
		font-family: var(--lens-font-mono);
		font-size: var(--lens-font-size-xs);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.file {
		grid-column: 2;
		color: var(--lens-text-faint);
		font-family: var(--lens-font-mono);
		font-size: var(--lens-font-size-2xs);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.side-note {
		color: var(--lens-muted);
		font-size: var(--lens-font-size-xs);
		padding: 10px 12px;
	}
	.side-state {
		margin: 10px 12px;
	}

	main {
		flex: 1;
		overflow-y: auto;
		padding: var(--lens-space-6);
	}
</style>
