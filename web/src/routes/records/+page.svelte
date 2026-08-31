<!--
	/records — search the record ASD keeps of itself.

	The /history page charts the distilled history in aggregate. This page is
	the other half: the individual rows behind those charts, searchable, plus
	the raw commit chain they were distilled from and the search feedback that
	shapes ranking.

	The framing that makes the four tabs one page: ASG distills the commit
	chain into rollup rows and a milestone spine, and only then does GC
	reclaim the raw objects. So "milestones + rollup" is what survives a
	prune, "commits" is what a prune consumes, and the `on spine` marker on
	each commit is the join between them.

	All filter state lives in the URL, so any view here is a shareable link.
-->
<script lang="ts">
	import { untrack } from 'svelte';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import {
		getGcDryRun,
		isGcUncomputed,
		type GcEstimate,
		withdrawFeedback,
		expireFeedback,
		type FeedbackRecord,
		getMilestones,
		getRollup,
		getCommits,
		getFeedback,
		fmtNum,
		fmtTime,
		fmtBytes,
		type MilestonePage,
		type RollupPage,
		type CommitPage,
		type FeedbackPage,
		type FacetValue
	} from '$lib/metrics';
	import FacetRow from './FacetRow.svelte';
	import Pager from './Pager.svelte';

	type Tab = 'milestones' | 'rollup' | 'commits' | 'feedback';

	const TABS: { id: Tab; label: string; blurb: string }[] = [
		{
			id: 'milestones',
			label: 'Milestones',
			blurb:
				'The named spine. Each row pins a state_root that GC must keep reachable — these survive a prune by name.'
		},
		{
			id: 'rollup',
			label: 'Rollup',
			blurb:
				'One row per day × namespace × agent × intent. What the /history charts are summed from, and what remains once the commits themselves are reclaimed.'
		},
		{
			id: 'commits',
			label: 'Commits',
			blurb:
				'The raw chain. Rows marked “on spine” are pinned by a milestone; the rest survive only as a +1 in the rollup once retention lets them go.'
		},
		{
			id: 'feedback',
			label: 'Feedback',
			blurb:
				'Recorded (query, symbol) verdicts — the corrective signal search ranking reads. Expired entries are shown, not hidden: they still explain a past ranking.'
		}
	];

	// -- URL as state -------------------------------------------------------
	// Reading from page.url keeps back/forward and copy-link working; every
	// mutation goes through setParams so there's one place that decides when
	// paging resets.

	let params = $derived(page.url.searchParams);
	let tab = $derived<Tab>(((params.get('tab') as Tab) ?? 'milestones') as Tab);
	let q = $derived(params.get('q') ?? '');
	let from = $derived(params.get('from') ?? '');
	let to = $derived(params.get('to') ?? '');
	let offset = $derived(Number(params.get('offset') ?? '0') || 0);
	let limit = $derived(Number(params.get('limit') ?? '50') || 50);
	let scan = $derived(Number(params.get('scan') ?? '5000') || 5000);
	// Categorical filters. Each tab uses a subset; unused keys stay absent.
	let kind = $derived(params.get('kind') ?? undefined);
	let namespace = $derived(params.get('namespace') ?? undefined);
	let agent = $derived(params.get('agent') ?? undefined);
	let intent = $derived(params.get('intent') ?? undefined);
	let verdict = $derived(params.get('verdict') ?? undefined);
	let author = $derived(params.get('author') ?? undefined);
	let milestoneOnly = $derived(params.get('milestone') ?? undefined);

	/**
	 * Apply a patch to the query string. Any change other than paging sends
	 * you back to page 1 — staying on offset 400 while narrowing a filter to
	 * 12 rows would show an empty table.
	 */
	function setParams(patch: Record<string, string | undefined>, keepOffset = false) {
		const next = new URLSearchParams(page.url.searchParams);
		for (const [k, v] of Object.entries(patch)) {
			if (v === undefined || v === '') next.delete(k);
			else next.set(k, v);
		}
		if (!keepOffset && !('offset' in patch)) next.delete('offset');
		goto(`?${next.toString()}`, { replaceState: true, keepFocus: true, noScroll: true });
	}

	/** Switching tabs keeps the text query and dates, drops facet selections. */
	function selectTab(next: Tab) {
		const carried = new URLSearchParams();
		carried.set('tab', next);
		for (const k of ['q', 'from', 'to', 'limit'] as const) {
			const v = page.url.searchParams.get(k);
			if (v) carried.set(k, v);
		}
		goto(`?${carried.toString()}`, { replaceState: true, noScroll: true });
	}

	// -- Search box ---------------------------------------------------------
	// Local mirror + debounce so typing doesn't fire a request per keystroke.
	// Synced back from the URL only when they actually differ, otherwise the
	// effect would fight the input on every render.

	let qInput = $state('');
	let debounce: ReturnType<typeof setTimeout> | null = null;

	$effect(() => {
		// Track `q` only. The write to `qInput` is untracked, otherwise this
		// effect depends on the state it sets and re-triggers itself.
		const urlQ = q;
		untrack(() => {
			if (urlQ !== qInput.trim()) qInput = urlQ;
		});
	});

	function onQueryInput(v: string) {
		qInput = v;
		if (debounce) clearTimeout(debounce);
		debounce = setTimeout(() => setParams({ q: v.trim() || undefined }), 250);
	}

	// -- Data ---------------------------------------------------------------

	let milestones = $state<MilestonePage | null>(null);
	let rollup = $state<RollupPage | null>(null);
	let commits = $state<CommitPage | null>(null);
	let feedback = $state<FeedbackPage | null>(null);
	let gc = $state<GcEstimate | null>(null);
	let gcState = $state<'idle' | 'running' | 'unavailable'>('idle');
	let loading = $state(true);
	let err = $state<string | null>(null);

	// Guards against a slow earlier request landing after a fast later one.
	let seq = 0;

	$effect(() => {
		// Touch every filter so the effect re-runs when any of them changes.
		const f = {
			q,
			from: from || undefined,
			to: to || undefined,
			limit,
			offset,
			kind,
			namespace,
			agent,
			intent,
			verdict,
			author,
			milestone: milestoneOnly,
			scan
		};
		const which = tab;
		const mine = ++seq;
		loading = true;
		err = null;

		const call =
			which === 'milestones'
				? getMilestones(f).then((r) => (milestones = r))
				: which === 'rollup'
					? getRollup(f).then((r) => (rollup = r))
					: which === 'commits'
						? getCommits(f).then((r) => (commits = r))
						: getFeedback(f).then((r) => (feedback = r));

		call
			.catch((e) => {
				if (mine === seq) err = e instanceof Error ? e.message : String(e);
			})
			.finally(() => {
				if (mine === seq) loading = false;
			});
	});

	/**
	 * Reclaim estimate.
	 *
	 * Computing one walks the whole object DAG — ~26s on this repo's 866k
	 * objects — so the server memoizes it on the ref head. On mount we ask
	 * only for the memo (`cachedOnly`), which answers in milliseconds whether
	 * warm or cold: warm renders the strip immediately, cold offers the
	 * button. Nothing here can make the page wait on a walk.
	 *
	 * A 404 means this build's engine predates Plan B — report that, don't
	 * leave the button looking broken.
	 */
	function loadGc(cachedOnly = false) {
		if (gcState === 'running') return;
		if (!cachedOnly) gcState = 'running';
		getGcDryRun({ cachedOnly })
			.then((g) => {
				if (isGcUncomputed(g)) {
					// Cold cache and we didn't ask for a walk — leave the button up.
					gc = null;
					gcState = 'idle';
					return;
				}
				gc = g;
				gcState = 'idle';
			})
			.catch(() => {
				gc = null;
				gcState = 'unavailable';
			});
	}

	$effect(() => {
		// Cheap either way — see loadGc.
		loadGc(true);
	});

	let active = $derived(TABS.find((t) => t.id === tab) ?? TABS[0]);
	let current = $derived(
		tab === 'milestones'
			? milestones
			: tab === 'rollup'
				? rollup
				: tab === 'commits'
					? commits
					: feedback
	);

	// -- Feedback lifecycle actions ----------------------------------------
	// Patch the one row from the action's response rather than refetching:
	// the list is paginated and filtered, so a refetch could reorder or drop
	// the row the user just acted on, out from under them.

	let acting = $state<string | null>(null);
	let actionErr = $state<string | null>(null);

	function patchRow(updated: FeedbackRecord) {
		if (!feedback) return;
		feedback = {
			...feedback,
			items: feedback.items.map((e) => (e.entry_id === updated.entry_id ? updated : e))
		};
	}

	function retire(entry: FeedbackRecord, how: 'withdraw' | 'expire') {
		if (acting) return;
		acting = entry.entry_id;
		actionErr = null;
		const call =
			how === 'withdraw'
				? withdrawFeedback(entry.entry_id, { by: 'asd-lens' })
				: expireFeedback(entry.entry_id);
		call
			.then(patchRow)
			.catch((e) => (actionErr = e instanceof Error ? e.message : String(e)))
			.finally(() => (acting = null));
	}

	function facetsOf(name: string): FacetValue[] {
		return current?.facets?.[name] ?? [];
	}

	/** Any filter set beyond the tab itself — drives the "clear" affordance. */
	let hasFilters = $derived(
		Boolean(q || from || to || kind || namespace || agent || intent || verdict || author || milestoneOnly)
	);

	function clearAll() {
		goto(`?tab=${tab}`, { replaceState: true, noScroll: true });
	}
</script>

<svelte:head>
	<title>Records — ASD Lens</title>
</svelte:head>

<header class="page-header">
	<h1>Records</h1>
	<p class="page-desc">
		Everything ASD distilled out of its own history before a prune could reclaim it — and the raw
		chain it was distilled from. <a href="/history">History</a> charts these rows in aggregate; here
		you can search them.
	</p>
	{#if gc}
		<div class="gc-strip" class:unsafe={!gc.safe}>
			<div class="gc-item">
				<span class="gc-label">reclaimable</span>
				<strong>{fmtNum(gc.reclaimable_objects)}</strong>
				<span class="gc-sub">objects · {fmtBytes(gc.estimated_reclaimable_bytes)}</span>
			</div>
			<div class="gc-item">
				<span class="gc-label">store</span>
				<strong>{fmtBytes(gc.bytes_before)}</strong>
				<span class="gc-sub">{fmtNum(gc.objects_before)} objects</span>
			</div>
			<div class="gc-item">
				<span class="gc-label">undistilled</span>
				<strong>{fmtNum(gc.undistilled_commits)}</strong>
				<span class="gc-sub">commits not yet summarized</span>
			</div>
			<div class="gc-verdict" data-tone={gc.safe ? 'ok' : 'warn'}>
				{gc.safe ? 'safe to sweep' : 'sweep would lose undistilled history'}
				{#if gc.cached && (gc.age_secs ?? 0) > 60}
					<button
						type="button"
						class="recompute"
						onclick={() => loadGc()}
						title="Estimate computed {Math.round((gc.age_secs ?? 0) / 60)} min ago"
					>
						recompute
					</button>
				{/if}
			</div>
		</div>
	{:else}
		<div class="gc-strip idle">
			<p>
				{#if gcState === 'unavailable'}
					This server's engine predates the GC surface, so there's no reclaim estimate to show.
				{:else}
					A reclaim estimate shows how much of the store a sweep would drop, and whether any commit
					is still undistilled. It walks the whole object graph — tens of seconds on a large store
					— so it's computed on request, then remembered until the next write.
				{/if}
			</p>
			{#if gcState !== 'unavailable'}
				<!-- Arrow, not a bare reference: `onclick={loadGc}` would hand the
				     MouseEvent to `cachedOnly`, and a truthy value there means
				     "never compute" — the button would do nothing visible. -->
				<button type="button" onclick={() => loadGc()} disabled={gcState === 'running'}>
					{gcState === 'running' ? 'walking the object graph…' : 'estimate reclaim'}
				</button>
			{/if}
		</div>
	{/if}
</header>

<div class="tabs" role="tablist" aria-label="Record types">
	{#each TABS as t (t.id)}
		<button
			role="tab"
			aria-selected={tab === t.id}
			class:active={tab === t.id}
			onclick={() => selectTab(t.id)}
		>
			{t.label}
		</button>
	{/each}
</div>

<p class="tab-blurb">{active.blurb}</p>

<div class="controls">
	<label class="search">
		<span class="sr-only">search {active.label.toLowerCase()}</span>
		<input
			type="search"
			placeholder={tab === 'feedback'
				? 'search query, symbol, author, note…'
				: tab === 'rollup'
					? 'search day, agent, intent, namespace…'
					: 'search description, commit, agent…'}
			value={qInput}
			oninput={(e) => onQueryInput(e.currentTarget.value)}
		/>
	</label>
	{#if tab !== 'feedback'}
		<label class="date">
			from
			<input
				type="date"
				value={from}
				onchange={(e) => setParams({ from: e.currentTarget.value || undefined })}
			/>
		</label>
		<label class="date">
			to
			<input
				type="date"
				value={to}
				onchange={(e) => setParams({ to: e.currentTarget.value || undefined })}
			/>
		</label>
	{/if}
	{#if tab === 'commits'}
		<label class="date">
			spine
			<select
				value={milestoneOnly ?? ''}
				onchange={(e) => setParams({ milestone: e.currentTarget.value || undefined })}
			>
				<option value="">all</option>
				<option value="1">pinned only</option>
				<option value="0">unpinned only</option>
			</select>
		</label>
		<label class="date">
			scan
			<input
				type="number"
				min="1"
				max="100000"
				step="1000"
				value={scan}
				onchange={(e) => setParams({ scan: e.currentTarget.value || undefined })}
			/>
		</label>
	{/if}
	<label class="date">
		rows
		<select value={String(limit)} onchange={(e) => setParams({ limit: e.currentTarget.value })}>
			<option value="25">25</option>
			<option value="50">50</option>
			<option value="100">100</option>
			<option value="250">250</option>
		</select>
	</label>
	{#if hasFilters}
		<button class="clear" type="button" onclick={clearAll}>clear filters</button>
	{/if}
	{#if loading}<span class="spinner">loading…</span>{/if}
</div>

<div class="facets">
	{#if tab === 'milestones'}
		<FacetRow
			label="kind"
			values={facetsOf('kinds')}
			selected={kind}
			onselect={(v) => setParams({ kind: v })}
		/>
		<FacetRow
			label="agent"
			values={facetsOf('agents')}
			selected={agent}
			onselect={(v) => setParams({ agent: v })}
		/>
		<FacetRow
			label="namespace"
			values={facetsOf('namespaces')}
			selected={namespace}
			onselect={(v) => setParams({ namespace: v })}
		/>
	{:else if tab === 'rollup'}
		<FacetRow
			label="intent"
			values={facetsOf('intents')}
			selected={intent}
			onselect={(v) => setParams({ intent: v })}
		/>
		<FacetRow
			label="agent"
			values={facetsOf('agents')}
			selected={agent}
			onselect={(v) => setParams({ agent: v })}
		/>
		<FacetRow
			label="namespace"
			values={facetsOf('namespaces')}
			selected={namespace}
			onselect={(v) => setParams({ namespace: v })}
		/>
	{:else if tab === 'commits'}
		<FacetRow
			label="intent"
			values={facetsOf('intents')}
			selected={intent}
			onselect={(v) => setParams({ intent: v })}
		/>
		<FacetRow
			label="agent"
			values={facetsOf('agents')}
			selected={agent}
			onselect={(v) => setParams({ agent: v })}
		/>
	{:else}
		<FacetRow
			label="verdict"
			values={facetsOf('verdicts')}
			selected={verdict}
			onselect={(v) => setParams({ verdict: v })}
		/>
		<FacetRow
			label="author"
			values={facetsOf('authors')}
			selected={author}
			onselect={(v) => setParams({ author: v })}
		/>
	{/if}
</div>

{#if err}
	<div class="state-error">{err}</div>
{:else if !current && loading}
	<div class="state-loading">loading…</div>
{:else if current && current.total === 0}
	<div class="state-empty">
		No matching {active.label.toLowerCase()}.
		{#if hasFilters}Loosen the filters, or <button class="linkish" onclick={clearAll}>clear them</button>.{/if}
		{#if tab === 'feedback' && !hasFilters}
			Nothing has been recorded yet — verdicts arrive via <code>asd feedback</code> and the
			<code>feedback_mark</code> MCP tool.
		{/if}
		{#if tab !== 'feedback' && !hasFilters}
			The distilled tables are empty until the extractor has run — they populate on the first
			<code>/api/v1/history</code> call.
		{/if}
	</div>
{:else if current}
	<!-- Per-tab tables. Each keeps the same header/row rhythm so switching
	     tabs doesn't feel like switching applications. -->
	{#if tab === 'milestones' && milestones}
		{#if milestones.unpinned > 0}
			<div class="banner" data-tone="warn">
				<strong>{fmtNum(milestones.unpinned)}</strong> of these milestones pin no
				<code>state_root</code> — written before the retention hook existed. They survive as a
				description, but GC has nothing to hold reachable for them.
			</div>
		{/if}
		<div class="table-scroll">
		<table>
			<thead>
				<tr>
					<th>when</th>
					<th>kind</th>
					<th>description</th>
					<th>agent</th>
					<th>commit</th>
					<th>pins</th>
				</tr>
			</thead>
			<tbody>
				{#each milestones.items as m (m.commit_id + m.kind)}
					<tr>
						<td class="mono nowrap">{fmtTime(m.timestamp)}</td>
						<td><span class="tag">{m.kind}</span></td>
						<td class="desc">{m.description}</td>
						<td class="nowrap">{m.agent}</td>
						<td class="mono nowrap" title={m.commit_id}>{m.commit}</td>
						<td class="mono nowrap">
							{#if m.pins_state}
								<span class="pin ok" title="GC keeps this snapshot reachable">{m.state_root}</span>
							{:else}
								<span class="pin none" title="no state_root — nothing pinned">none</span>
							{/if}
						</td>
					</tr>
				{/each}
			</tbody>
		</table>
		</div>
	{:else if tab === 'rollup' && rollup}
		<div class="summary">
			<strong>{fmtNum(rollup.totals.commits)}</strong> commits across
			<strong>{fmtNum(rollup.totals.days.count)}</strong> days
			{#if rollup.totals.days.first}
				· {rollup.totals.days.first} → {rollup.totals.days.last}
			{/if}
		</div>
		<div class="table-scroll">
		<table>
			<thead>
				<tr>
					<th>day</th>
					<th>agent</th>
					<th>intent</th>
					<th class="num">commits</th>
					<th>first</th>
					<th>last</th>
					<th>namespace</th>
				</tr>
			</thead>
			<tbody>
				{#each rollup.items as r (r.day + r.namespace + r.agent + r.intent)}
					<tr>
						<td class="mono nowrap">{r.day}</td>
						<td class="nowrap">{r.agent}</td>
						<td><span class="tag">{r.intent}</span></td>
						<td class="num mono">{fmtNum(r.commits)}</td>
						<td class="mono nowrap faint">{fmtTime(r.first_ts).slice(11)}</td>
						<td class="mono nowrap faint">{fmtTime(r.last_ts).slice(11)}</td>
						<td class="nowrap faint">{r.namespace}</td>
					</tr>
				{/each}
			</tbody>
		</table>
		</div>
	{:else if tab === 'commits' && commits}
		<div class="summary">
			<strong>{fmtNum(commits.on_spine)}</strong> of {fmtNum(commits.total)} matching commits are
			pinned by a milestone · walked {fmtNum(commits.scanned)} reachable from <code>HEAD</code>
			{#if commits.capped}
				· <span class="warn-text"
					>walk capped at {fmtNum(commits.scan)} — counts describe the scanned window, not the whole
					store</span
				>
			{:else if commits.distilled > commits.scanned}
				· <span class="warn-text">
					the rollup records {fmtNum(commits.distilled)} commits, so
					<strong>{fmtNum(commits.distilled - commits.scanned)}</strong> are no longer reachable at all
					— already-garbage a sweep would drop, listed here only as rollup counts
				</span>
			{/if}
		</div>
		<div class="table-scroll">
		<table>
			<thead>
				<tr>
					<th>when</th>
					<th>intent</th>
					<th>description</th>
					<th>agent</th>
					<th>commit</th>
					<th>spine</th>
				</tr>
			</thead>
			<tbody>
				{#each commits.items as c (c.commit_id)}
					<tr class:pinned={c.on_spine}>
						<td class="mono nowrap">{fmtTime(c.timestamp)}</td>
						<td><span class="tag">{c.intent}</span></td>
						<td class="desc">
							{c.description}
							{#if c.reasoning}
								<details class="reasoning">
									<summary>reasoning</summary>
									<p>{c.reasoning}</p>
								</details>
							{/if}
						</td>
						<td class="nowrap">{c.agent}</td>
						<td class="mono nowrap" title={c.commit_id}>{c.commit}</td>
						<td class="nowrap">
							{#if c.on_spine}
								<span class="pin ok" title="a milestone pins this commit's state_root">pinned</span>
							{:else}
								<span class="pin none" title="reclaimable once retention allows">—</span>
							{/if}
						</td>
					</tr>
				{/each}
			</tbody>
		</table>
		</div>
	{:else if tab === 'feedback' && feedback}
		{#if actionErr}
			<div class="banner" data-tone="warn">{actionErr}</div>
		{/if}
		<div class="table-scroll">
		<table>
			<thead>
				<tr>
					<th>when</th>
					<th>verdict</th>
					<th>query</th>
					<th>symbol</th>
					<th>author</th>
					<th>note</th>
					<th>state</th>
				</tr>
			</thead>
			<tbody>
				{#each feedback.items as f (f.entry_id)}
					<tr class:retired={f.inert}>
						<td class="mono nowrap">{fmtTime(f.created_at)}</td>
						<td><span class="tag">{f.verdict}</span></td>
						<td class="desc mono">{f.query}</td>
						<td class="qname">
							<a href="/symbols/{encodeURIComponent(f.symbol_qname)}">{f.symbol_qname}</a>
							{#if f.file_scope}<span class="faint"> · {f.file_scope}</span>{/if}
						</td>
						<td class="nowrap">{f.author}</td>
						<td class="desc faint">{f.note ?? ''}</td>
						<td class="state nowrap">
							{#if f.withdrawn}
								<span class="tag retired-tag" title={f.withdrawn_reason ?? undefined}>
									withdrawn{f.withdrawn_by ? ` · ${f.withdrawn_by}` : ''}
								</span>
							{:else if f.expired}
								<span class="tag retired-tag">expired</span>
							{:else}
								<!-- Live: offer both retirements. Withdraw first — a verdict
								     you are looking at because it is wrong is the common case;
								     expiry is for one that has merely aged out. No purge here:
								     it hard-deletes from an append-only store and belongs
								     behind a CLI --yes, not one click. -->
								<button
									type="button"
									disabled={acting === f.entry_id}
									onclick={() => retire(f, 'withdraw')}
									title="This verdict was wrong — retract it. It stays listed, marked."
								>
									withdraw
								</button>
								<button
									type="button"
									class="subtle"
									disabled={acting === f.entry_id}
									onclick={() => retire(f, 'expire')}
									title="This verdict was right but has aged out — lapse it."
								>
									expire
								</button>
							{/if}
						</td>
					</tr>
				{/each}
			</tbody>
		</table>
		</div>
	{/if}

	<Pager
		total={current.total}
		{offset}
		{limit}
		onpage={(o) => setParams({ offset: o === 0 ? undefined : String(o) }, true)}
	/>
{/if}

<style>
	.page-header {
		margin-bottom: var(--lens-space-4);
	}
	.page-header h1 {
		margin: 0;
		font-size: var(--lens-font-size-xl);
		color: var(--lens-text-strong);
	}
	.page-desc {
		margin: var(--lens-space-2) 0 0;
		max-width: 74ch;
		color: var(--lens-text-secondary);
		font-size: var(--lens-font-size-sm);
		line-height: 1.55;
	}
	.page-desc a {
		color: var(--lens-accent);
	}

	.gc-strip {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--lens-space-5);
		margin-top: var(--lens-space-3);
		padding: var(--lens-space-3) var(--lens-space-4);
		border: 1px solid var(--lens-border);
		border-radius: var(--lens-radius-md);
		background: var(--lens-surface);
	}
	.gc-strip.unsafe {
		border-color: var(--lens-warn-border);
		background: var(--lens-warn-tint);
	}
	.gc-strip.idle {
		gap: var(--lens-space-3);
	}
	.gc-strip.idle p {
		margin: 0;
		flex: 1 1 340px;
		font-size: var(--lens-font-size-2xs);
		color: var(--lens-muted);
		line-height: 1.55;
	}
	.gc-strip.idle button {
		appearance: none;
		cursor: pointer;
		border: 1px solid var(--lens-border);
		background: var(--lens-surface-raised);
		color: var(--lens-text-secondary);
		font-family: inherit;
		font-size: var(--lens-font-size-2xs);
		padding: 5px 12px;
		border-radius: var(--lens-radius-sm);
		white-space: nowrap;
	}
	.gc-strip.idle button:hover:not(:disabled) {
		color: var(--lens-text-strong);
		border-color: var(--lens-border-strong);
	}
	.gc-strip.idle button:disabled {
		opacity: 0.6;
		cursor: progress;
	}
	.gc-strip.idle button:focus-visible {
		outline: 2px solid var(--lens-focus);
		outline-offset: 2px;
	}
	.gc-item {
		display: flex;
		flex-direction: column;
		gap: 1px;
	}
	.gc-label {
		font-size: var(--lens-font-size-2xs);
		text-transform: uppercase;
		letter-spacing: var(--lens-tracking-caps);
		color: var(--lens-text-faint);
	}
	.gc-item strong {
		font-size: var(--lens-font-size-md);
		color: var(--lens-text-strong);
		font-variant-numeric: tabular-nums;
	}
	.gc-sub {
		font-size: var(--lens-font-size-2xs);
		color: var(--lens-muted);
	}
	.gc-verdict {
		margin-left: auto;
		font-size: var(--lens-font-size-2xs);
		font-weight: 600;
		padding: 3px 10px;
		border-radius: var(--lens-radius-full);
	}
	.gc-verdict[data-tone='ok'] {
		background: var(--lens-ok-tint);
		color: var(--lens-ok);
		border: 1px solid var(--lens-ok-border);
	}
	.gc-verdict[data-tone='warn'] {
		background: var(--lens-warn-tint);
		color: var(--lens-warn);
		border: 1px solid var(--lens-warn-border);
	}
	.recompute {
		appearance: none;
		cursor: pointer;
		margin-left: 8px;
		border: 0;
		background: none;
		color: inherit;
		opacity: 0.7;
		font-family: inherit;
		font-size: inherit;
		font-weight: 400;
		text-decoration: underline;
		padding: 0;
	}
	.recompute:hover {
		opacity: 1;
	}

	.tabs {
		display: flex;
		gap: 2px;
		margin-bottom: var(--lens-space-2);
		border-bottom: 1px solid var(--lens-border);
	}
	.tabs button {
		appearance: none;
		border: 0;
		border-bottom: 2px solid transparent;
		background: transparent;
		color: var(--lens-muted);
		font-family: inherit;
		font-size: var(--lens-font-size-xs);
		font-weight: 600;
		padding: 8px 14px;
		cursor: pointer;
		transition: color var(--lens-dur-fast) var(--lens-ease);
	}
	.tabs button:hover {
		color: var(--lens-text);
	}
	.tabs button.active {
		color: var(--lens-accent);
		border-bottom-color: var(--lens-accent);
	}
	.tabs button:focus-visible {
		outline: 2px solid var(--lens-focus);
		outline-offset: -2px;
	}

	.tab-blurb {
		margin: 0 0 var(--lens-space-3);
		max-width: 88ch;
		font-size: var(--lens-font-size-2xs);
		color: var(--lens-muted);
		line-height: 1.5;
	}

	.controls {
		display: flex;
		flex-wrap: wrap;
		gap: 10px;
		align-items: center;
		margin-bottom: var(--lens-space-2);
		font-size: var(--lens-font-size-2xs);
		color: var(--lens-muted);
	}
	.search {
		flex: 1 1 280px;
		min-width: 220px;
	}
	.search input {
		width: 100%;
	}
	.controls label.date {
		display: flex;
		gap: 4px;
		align-items: center;
	}
	.controls input,
	.controls select {
		font-family: inherit;
		font-size: var(--lens-font-size-2xs);
		color: var(--lens-text);
		background: var(--lens-surface);
		border: 1px solid var(--lens-border);
		border-radius: var(--lens-radius-sm);
		padding: 4px 8px;
	}
	.controls input:focus-visible,
	.controls select:focus-visible {
		outline: 2px solid var(--lens-focus);
		outline-offset: 1px;
	}
	.clear,
	.linkish {
		appearance: none;
		cursor: pointer;
		border: 1px solid var(--lens-border);
		background: var(--lens-surface);
		color: var(--lens-text-secondary);
		font-family: inherit;
		font-size: var(--lens-font-size-2xs);
		padding: 4px 10px;
		border-radius: var(--lens-radius-sm);
	}
	.linkish {
		border: 0;
		background: none;
		color: var(--lens-accent);
		padding: 0;
		text-decoration: underline;
	}
	.clear:hover {
		color: var(--lens-text-strong);
		border-color: var(--lens-border-strong);
	}
	.spinner {
		color: var(--lens-text-faint);
	}

	.facets {
		display: flex;
		flex-direction: column;
		gap: 6px;
		margin-bottom: var(--lens-space-3);
	}

	.summary,
	.banner {
		margin-bottom: var(--lens-space-2);
		font-size: var(--lens-font-size-2xs);
		color: var(--lens-text-secondary);
	}
	.summary strong {
		color: var(--lens-text-strong);
		font-variant-numeric: tabular-nums;
	}
	.banner {
		padding: var(--lens-space-2) var(--lens-space-3);
		border-radius: var(--lens-radius-md);
		line-height: 1.5;
	}
	.banner[data-tone='warn'] {
		background: var(--lens-warn-tint);
		border: 1px solid var(--lens-warn-border);
		color: var(--lens-text);
	}
	.warn-text {
		color: var(--lens-warn);
	}

	.table-scroll {
		overflow-x: auto;
		max-width: 100%;
	}
	table {
		width: 100%;
		border-collapse: collapse;
		font-size: var(--lens-font-size-2xs);
	}
	/* Unbounded in the data: a qname can run to ~200 chars. Cap it so the
	   table still fits, and let the container scroll past that. */
	td.qname {
		max-width: 44ch;
		overflow-wrap: anywhere;
	}
	thead th {
		text-align: left;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: var(--lens-tracking-caps);
		color: var(--lens-text-faint);
		padding: 6px 10px;
		border-bottom: 1px solid var(--lens-border);
		white-space: nowrap;
	}
	tbody td {
		padding: 7px 10px;
		border-bottom: 1px solid var(--lens-border-subtle);
		color: var(--lens-text-secondary);
		vertical-align: top;
	}
	tbody tr:hover td {
		background: var(--lens-surface);
	}
	tbody tr.pinned td:first-child {
		box-shadow: inset 2px 0 0 var(--lens-accent);
	}
	/* Retired — expired or withdrawn. Dimmed, not hidden: it still explains
	   why a past search ranked as it did. */
	tbody tr.retired td {
		opacity: 0.55;
	}
	td.state button {
		appearance: none;
		cursor: pointer;
		border: 1px solid var(--lens-border);
		background: var(--lens-surface-raised);
		color: var(--lens-text-secondary);
		font-family: inherit;
		font-size: var(--lens-font-size-2xs);
		padding: 2px 8px;
		border-radius: var(--lens-radius-sm);
	}
	td.state button + button {
		margin-left: 4px;
	}
	td.state button.subtle {
		border-color: transparent;
		background: none;
		color: var(--lens-text-faint);
	}
	td.state button:hover:not(:disabled) {
		color: var(--lens-text-strong);
		border-color: var(--lens-border-strong);
	}
	td.state button:disabled {
		opacity: 0.5;
		cursor: progress;
	}
	td.state button:focus-visible {
		outline: 2px solid var(--lens-focus);
		outline-offset: 2px;
	}
	.retired-tag {
		border-color: var(--lens-warn-border);
		color: var(--lens-warn);
	}
	td a {
		color: var(--lens-accent);
	}
	.mono {
		font-family: var(--lens-font-mono);
	}
	.nowrap {
		white-space: nowrap;
	}
	.num {
		text-align: right;
		font-variant-numeric: tabular-nums;
	}
	.faint {
		color: var(--lens-text-faint);
	}
	.desc {
		color: var(--lens-text);
		min-width: 24ch;
	}
	.tag {
		display: inline-block;
		font-size: var(--lens-font-size-2xs);
		padding: 1px 7px;
		border-radius: var(--lens-radius-full);
		background: var(--lens-surface-raised);
		border: 1px solid var(--lens-border);
		color: var(--lens-text-secondary);
		white-space: nowrap;
	}

	.pin {
		font-family: var(--lens-font-mono);
		font-size: var(--lens-font-size-2xs);
	}
	.pin.ok {
		color: var(--lens-ok);
	}
	.pin.none {
		color: var(--lens-text-faint);
	}
	.reasoning {
		margin-top: 4px;
	}
	.reasoning summary {
		cursor: pointer;
		color: var(--lens-muted);
	}
	.reasoning p {
		margin: 4px 0 0;
		color: var(--lens-text-secondary);
		white-space: pre-wrap;
	}

	.state-loading,
	.state-error,
	.state-empty {
		padding: var(--lens-space-5);
		border: 1px solid var(--lens-border);
		border-radius: var(--lens-radius-md);
		background: var(--lens-surface);
		color: var(--lens-text-secondary);
		font-size: var(--lens-font-size-sm);
		line-height: 1.55;
	}
	.state-error {
		color: var(--lens-danger);
		border-color: var(--lens-danger-border);
		background: var(--lens-danger-tint);
	}
	.state-empty code {
		font-family: var(--lens-font-mono);
		font-size: 0.9em;
	}

	.sr-only {
		position: absolute;
		width: 1px;
		height: 1px;
		padding: 0;
		margin: -1px;
		overflow: hidden;
		clip: rect(0, 0, 0, 0);
		white-space: nowrap;
		border: 0;
	}
</style>
