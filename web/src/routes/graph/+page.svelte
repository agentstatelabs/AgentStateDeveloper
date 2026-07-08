<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { CallGraphView } from '@agentstate/lens-core';
	import { asdClient } from '$lib/api';

	// URL is the source of truth: /graph?q=<qname> deep-links a graph and
	// back/forward re-drives the view via `initialQuery`.
	let initialQuery = $derived(page.url.searchParams.get('q') ?? '');

	function symbolHref(q: string): string {
		return `/symbols/${encodeURIComponent(q)}`;
	}

	function syncUrl(qname: string) {
		const target = `/graph?q=${encodeURIComponent(qname)}`;
		if (page.url.pathname + page.url.search !== target) {
			goto(target, { replaceState: false, keepFocus: true, noScroll: true });
		}
	}
</script>

<svelte:head>
	<title>Graph — ASD Lens</title>
</svelte:head>

<header class="page-header">
	<h1>Call graph</h1>
	<p class="page-desc">
		Explore who calls what, up to three hops from any symbol. Toggle “cross-module only” to see
		just the edges that cross a module boundary — the calls most likely to break someone else.
	</p>
</header>

<CallGraphView
	client={asdClient}
	{symbolHref}
	{initialQuery}
	onQueryChange={syncUrl}
	onViewSymbol={(q) => goto(symbolHref(q))}
/>

