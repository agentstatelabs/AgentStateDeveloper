// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
	site: 'https://agentstatedeveloper.dev',
	integrations: [
		starlight({
			title: 'AgentStateDeveloper',
			description:
				'Code-level context and audit overlay for agent-authored code — decision ledger, effect declarations, call graph, policy gate, and audit stream.',
			logo: {
				src: './src/assets/asd-wordmark.svg',
				replacesTitle: true,
			},
			social: [
				{
					icon: 'github',
					label: 'GitHub',
					href: 'https://github.com/agentstatelabs/AgentStateDeveloper',
				},
			],
			customCss: ['./src/styles/theme.css'],
			sidebar: [
				{
					label: 'Getting started',
					items: [
						{ label: 'Introduction', slug: 'guides/introduction' },
						{ label: 'Quick start', slug: 'guides/quickstart' },
						{ label: 'Core concepts', slug: 'guides/concepts' },
					],
				},
				{
					label: 'How it works',
					items: [
						{ label: 'Architecture', slug: 'guides/architecture' },
						{ label: 'Ecosystem: ASG, CTXone, ASD', slug: 'guides/ecosystem' },
						{ label: 'Git+ overlay model', slug: 'guides/git-overlay' },
						{ label: 'Policy & ratification', slug: 'guides/policy' },
						{ label: 'Audit log', slug: 'guides/audit' },
					],
				},
				{
					label: 'Language support',
					items: [
						{ label: 'Python', slug: 'guides/python' },
						{ label: 'TypeScript', slug: 'guides/typescript' },
					],
				},
				{
					label: 'Reference',
					items: [
						{ label: 'CLI (asd)', slug: 'reference/cli' },
						{ label: 'MCP tools', slug: 'reference/mcp-tools' },
						{ label: 'HTTP API', slug: 'reference/http-api' },
						{ label: 'Policy schema', slug: 'reference/policy-schema' },
						{ label: 'Audit event schema', slug: 'reference/audit-schema' },
					],
				},
			],
			favicon: '/favicon.svg',
			head: [
				{
					tag: 'meta',
					attrs: {
						property: 'og:image',
						content: 'https://agentstatedeveloper.dev/og-image.png',
					},
				},
				{
					tag: 'meta',
					attrs: { name: 'twitter:card', content: 'summary_large_image' },
				},
			],
		}),
	],
});
