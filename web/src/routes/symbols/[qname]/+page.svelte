<script lang="ts">
	import { goto } from '$app/navigation';
	import { SymbolDetail } from '@agentstate/lens-core';
	import { asdClient } from '$lib/api';
	import { symbols } from '$lib/stores';
	import { page } from '$app/state';

	let qname = $derived(decodeURIComponent(page.params.qname ?? ''));

	function symbolHref(q: string): string {
		return `/symbols/${encodeURIComponent(q)}`;
	}
</script>

<SymbolDetail
	client={asdClient}
	{qname}
	{symbolHref}
	resolveQname={(id) => symbols.qnameOf(id)}
	onSymbolNavigate={(q) => goto(symbolHref(q))}
/>
