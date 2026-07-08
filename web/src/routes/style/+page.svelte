<script lang="ts">
	/**
	 * /style — living reference for the Lens design system (dev-facing).
	 *
	 * Renders the lens-core tokens and core components straight from the
	 * running CSS, so this page is always true to what ships. When adding
	 * UI, come here first: if a color/space/recipe you need is missing,
	 * extend tokens.css in lens-core — don't hard-code.
	 */
	import {
		Badge,
		KindBadge,
		ActivityGlyph,
		VerifyBadge,
		type AuditVerifyReport
	} from '@agentstate/lens-core';

	const SURFACES = [
		['--lens-bg', 'page background'],
		['--lens-surface', 'cards, panels, sidebars'],
		['--lens-surface-raised', 'hover, chips, wells'],
		['--lens-overlay', 'popovers, tooltips, slide-ins'],
		['--lens-border-subtle', 'row separators'],
		['--lens-border', 'default border'],
		['--lens-border-strong', 'hovered/active border']
	];
	const TEXT = [
		['--lens-text-strong', 'headings, focal values'],
		['--lens-text', 'body'],
		['--lens-text-secondary', 'supporting'],
		['--lens-muted', 'captions, micro-labels'],
		['--lens-text-faint', 'hints (AA-large only)']
	];
	const SEMANTIC = [
		['--lens-accent', 'links, focus, active nav'],
		['--lens-ok', 'verified / healthy'],
		['--lens-warn', 'pending / unverified'],
		['--lens-danger', 'error / chain break'],
		['--lens-info', 'neutral notice']
	];
	const KINDS = ['function', 'method', 'class', 'module', 'variable'];
	const LEDGER = ['decision', 'assumption', 'constraint', 'rationale', 'hazard', 'tradeoff'];
	const GLYPHS = [
		'decision',
		'hypothesis',
		'hazard',
		'failed_attempt',
		'effect_declared',
		'effect_verified',
		'index_run',
		'audit'
	];
	const SPACES = ['1', '2', '3', '4', '5', '6', '8', '12'];
	const TYPE = [
		['--lens-font-size-2xs', 'micro-labels, timestamps'],
		['--lens-font-size-xs', 'metadata, table chrome'],
		['--lens-font-size-sm', 'body default'],
		['--lens-font-size-md', 'emphasized body'],
		['--lens-font-size-lg', 'page titles'],
		['--lens-font-size-xl', 'hero titles']
	];

	const verified: AuditVerifyReport = {
		configured: true,
		verified: true,
		total_events: 128,
		signed_events: 128,
		unsigned_events: 0,
		chain_breaks: []
	};
	const broken: AuditVerifyReport = {
		configured: true,
		verified: false,
		total_events: 128,
		signed_events: 127,
		chain_breaks: [{ index: 17, event_id: 'evt_9f3ac1', reason: 'prev_hash mismatch' }]
	};
	const unavailable: AuditVerifyReport = { configured: false, verified: false };
</script>

<svelte:head>
	<title>Style — ASD Lens</title>
</svelte:head>

<header class="page-header">
	<h1>Design system</h1>
	<p class="page-desc">
		Living reference for the Lens visual identity — tokens and components rendered from the CSS
		that ships. Source of truth: <code>@agentstate/lens-core/tokens.css</code>. Restrained,
		dark-first, flight recorder — color carries meaning, never decoration.
	</p>
</header>

<section>
	<h2 class="micro-label">Surfaces &amp; borders</h2>
	<div class="swatches">
		{#each SURFACES as [name, role] (name)}
			<div class="swatch">
				<div class="chip" style="background: var({name});"></div>
				<code>{name}</code>
				<span class="role">{role}</span>
			</div>
		{/each}
	</div>
</section>

<section>
	<h2 class="micro-label">Text ramp</h2>
	<div class="rows">
		{#each TEXT as [name, role] (name)}
			<div class="row">
				<span class="sample" style="color: var({name});">The agent recorded a decision</span>
				<code>{name}</code>
				<span class="role">{role}</span>
			</div>
		{/each}
	</div>
</section>

<section>
	<h2 class="micro-label">Accent &amp; semantic status</h2>
	<div class="rows">
		{#each SEMANTIC as [name, role] (name)}
			{@const base = name.replace('--lens-', '')}
			<div class="row">
				<span class="pill" style="color: var({name}); border-color: var(--lens-{base}-border, var(--lens-border)); background: var(--lens-{base}-tint, transparent);">
					{base}
				</span>
				<code>{name}</code>
				<span class="role">{role} · pairs: <code class="inline">-tint</code> / <code class="inline">-border</code></span>
			</div>
		{/each}
	</div>
</section>

<section>
	<h2 class="micro-label">Symbol kinds · ledger kinds</h2>
	<div class="inline-row">
		{#each KINDS as k (k)}<KindBadge kind={k} />{/each}
	</div>
	<div class="inline-row">
		{#each LEDGER as l (l)}
			<span class="ledger-chip" style="color: var(--lens-ledger-{l});">◆ {l}</span>
		{/each}
	</div>
	<div class="inline-row">
		{#each GLYPHS as g (g)}
			<span class="glyph-item"><ActivityGlyph kind={g} /> <span class="role">{g.replace('_', ' ')}</span></span>
		{/each}
	</div>
</section>

<section>
	<h2 class="micro-label">Badges</h2>
	<div class="inline-row">
		<Badge>muted</Badge>
		<Badge tone="accent">accent</Badge>
		<Badge tone="ok">ok</Badge>
		<Badge tone="warn">warn</Badge>
		<Badge tone="danger">danger</Badge>
	</div>
	<div class="inline-row stack">
		<VerifyBadge report={verified} />
		<VerifyBadge report={broken} />
		<VerifyBadge report={unavailable} />
	</div>
</section>

<section>
	<h2 class="micro-label">Type scale</h2>
	<div class="rows">
		{#each TYPE as [name, role] (name)}
			<div class="row">
				<span class="sample" style="font-size: var({name});">Flight recorder, not confetti</span>
				<code>{name}</code>
				<span class="role">{role}</span>
			</div>
		{/each}
	</div>
	<p class="note">
		UI text: <code>--lens-font-sans</code>. Identifiers, hashes, timestamps, code:
		<code>--lens-font-mono</code> — <span class="mono-demo">core.engine.hydrate_state:412</span>
	</p>
</section>

<section>
	<h2 class="micro-label">Spacing · radii · elevation · motion</h2>
	<div class="space-row">
		{#each SPACES as n (n)}
			<div class="space-item">
				<div class="space-bar" style="width: var(--lens-space-{n});"></div>
				<code>space-{n}</code>
			</div>
		{/each}
	</div>
	<div class="inline-row">
		{#each ['sm', 'md', 'lg'] as r (r)}
			<div class="radius-demo" style="border-radius: var(--lens-radius-{r});">radius-{r}</div>
		{/each}
		<div class="radius-demo" style="border-radius: var(--lens-radius-full); padding: 4px 14px;">radius-full</div>
	</div>
	<div class="inline-row">
		{#each ['sm', 'md', 'lg'] as e (e)}
			<div class="shadow-demo" style="box-shadow: var(--lens-shadow-{e});">shadow-{e}</div>
		{/each}
	</div>
	<p class="note">
		Motion: <code>--lens-dur-fast</code> 120ms (hover/focus) · <code>--lens-dur</code> 180ms
		(reveals) · <code>--lens-dur-slow</code> 280ms (panels). All three collapse to 0 under
		<code>prefers-reduced-motion</code>; keyframe animations must gate themselves too.
	</p>
</section>

<section>
	<h2 class="micro-label">States &amp; banners (app chrome)</h2>
	<div class="state-loading">loading…</div>
	<div class="state-empty">No entries yet — empty states always say what would fill them.</div>
	<div class="state-error">something failed: connection refused</div>
	<div class="banner" data-tone="warn" style="margin-top: 10px;">
		<strong>Warn banner.</strong> Feature-gate and configuration notices.
	</div>
	<div class="banner" data-tone="danger" style="margin-top: 8px;">
		<strong>Danger banner.</strong> Data-loss and chain-break notices.
	</div>
</section>

<section>
	<h2 class="micro-label">Focus</h2>
	<p class="note">
		Keyboard focus is globally visible: 2px <code>--lens-focus</code> ring via
		<code>:focus-visible</code> (tokens.css base rule). Tab through:
	</p>
	<div class="inline-row">
		<button class="demo-btn">button</button>
		<input class="demo-input" placeholder="input (ring + border)" />
		<a href="/style" class="demo-link">link</a>
	</div>
</section>

<style>
	section {
		margin-bottom: var(--lens-space-8);
		max-width: 860px;
	}
	section :global(h2.micro-label) {
		margin: 0 0 var(--lens-space-3);
		padding-bottom: var(--lens-space-1);
		border-bottom: 1px solid var(--lens-border-subtle);
	}
	.swatches {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(200px, 1fr));
		gap: var(--lens-space-3);
	}
	.swatch {
		display: grid;
		gap: 3px;
		font-size: var(--lens-font-size-2xs);
	}
	.swatch .chip {
		height: 36px;
		border-radius: var(--lens-radius-sm);
		border: 1px solid var(--lens-border);
	}
	.swatch code,
	.row code {
		background: none;
		padding: 0;
		color: var(--lens-text-secondary);
	}
	.role {
		color: var(--lens-muted);
		font-size: var(--lens-font-size-2xs);
	}
	.rows {
		display: flex;
		flex-direction: column;
		gap: var(--lens-space-2);
	}
	.row {
		display: grid;
		grid-template-columns: minmax(220px, 1fr) 200px 1fr;
		gap: var(--lens-space-4);
		align-items: baseline;
		font-size: var(--lens-font-size-xs);
	}
	.sample {
		font-size: var(--lens-font-size-sm);
	}
	.pill {
		display: inline-block;
		font-size: var(--lens-font-size-2xs);
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: var(--lens-tracking-caps);
		padding: 2px 10px;
		border-radius: var(--lens-radius-full);
		border: 1px solid;
	}
	.inline-row {
		display: flex;
		align-items: center;
		gap: var(--lens-space-3);
		flex-wrap: wrap;
		margin-bottom: var(--lens-space-3);
	}
	.inline-row.stack {
		flex-direction: column;
		align-items: flex-start;
	}
	.ledger-chip {
		font-size: var(--lens-font-size-2xs);
		text-transform: uppercase;
		letter-spacing: var(--lens-tracking-wide);
	}
	.glyph-item {
		display: inline-flex;
		align-items: center;
		gap: 5px;
	}
	.note {
		color: var(--lens-muted);
		font-size: var(--lens-font-size-xs);
		max-width: 72ch;
	}
	.mono-demo {
		font-family: var(--lens-font-mono);
		color: var(--lens-text-secondary);
	}
	.space-row {
		display: flex;
		gap: var(--lens-space-4);
		align-items: flex-end;
		flex-wrap: wrap;
		margin-bottom: var(--lens-space-4);
	}
	.space-item {
		display: grid;
		gap: 3px;
		font-size: var(--lens-font-size-2xs);
	}
	.space-item code {
		background: none;
		padding: 0;
		color: var(--lens-muted);
	}
	.space-bar {
		height: 10px;
		background: var(--lens-accent);
		opacity: 0.7;
		border-radius: 2px;
	}
	.radius-demo {
		background: var(--lens-surface-raised);
		border: 1px solid var(--lens-border);
		padding: var(--lens-space-2) var(--lens-space-4);
		font-size: var(--lens-font-size-2xs);
		color: var(--lens-text-secondary);
	}
	.shadow-demo {
		background: var(--lens-surface-raised);
		border: 1px solid var(--lens-border);
		border-radius: var(--lens-radius-md);
		padding: var(--lens-space-3) var(--lens-space-4);
		font-size: var(--lens-font-size-2xs);
		color: var(--lens-text-secondary);
	}
	.demo-btn {
		font: inherit;
		font-size: var(--lens-font-size-xs);
		background: var(--lens-surface);
		color: var(--lens-text);
		border: 1px solid var(--lens-border);
		border-radius: var(--lens-radius-sm);
		padding: 4px 12px;
		cursor: pointer;
		transition: background var(--lens-dur-fast) var(--lens-ease);
	}
	.demo-btn:hover {
		background: var(--lens-surface-raised);
	}
	.demo-input {
		font: inherit;
		font-size: var(--lens-font-size-xs);
		background: var(--lens-bg);
		color: var(--lens-text);
		border: 1px solid var(--lens-border);
		border-radius: var(--lens-radius-sm);
		padding: 4px 8px;
	}
	.demo-input::placeholder {
		color: var(--lens-text-faint);
	}
	.demo-input:focus {
		outline: none;
		border-color: var(--lens-accent);
		box-shadow: 0 0 0 3px var(--lens-accent-tint);
	}
	.demo-link {
		color: var(--lens-accent);
	}
	.demo-link:hover {
		color: var(--lens-accent-hover);
		text-decoration: underline;
	}
</style>
