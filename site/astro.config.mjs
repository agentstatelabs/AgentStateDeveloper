// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
	site: 'https://agentstatedeveloper.dev',
	integrations: [
		starlight({
			title: 'AgentStateDeveloper',
			description:
				'Code-level context and audit overlay for agent-authored code — the git+ layer agents needed.',
			social: [
				{
					icon: 'github',
					label: 'GitHub',
					href: 'https://github.com/agentstatelabs/AgentStateDeveloper',
				},
			],
			customCss: ['./src/styles/custom.css'],
			sidebar: [
				{
					label: 'Getting Started',
					items: [
						{ label: 'Introduction', slug: 'guides/introduction' },
						{ label: 'Quick Start', slug: 'guides/quickstart' },
						{ label: 'Core Concepts', slug: 'guides/concepts' },
					],
				},
				{
					label: 'How It Works',
					items: [
						{ label: 'Architecture', slug: 'guides/architecture' },
						{ label: 'Ecosystem: ASG, CTXone, ASD', slug: 'guides/ecosystem' },
						{ label: 'Git+ Overlay Model', slug: 'guides/git-overlay' },
						{ label: 'Policy & Ratification', slug: 'guides/policy' },
						{ label: 'Audit Log', slug: 'guides/audit' },
					],
				},
				{
					label: 'Language Support',
					items: [
						{ label: 'Python', slug: 'guides/python' },
						{ label: 'TypeScript', slug: 'guides/typescript' },
					],
				},
				{
					label: 'Reference',
					items: [
						{ label: 'CLI (`asd`)', slug: 'reference/cli' },
						{ label: 'MCP Tools', slug: 'reference/mcp-tools' },
						{ label: 'HTTP API', slug: 'reference/http-api' },
						{ label: 'Policy File Schema', slug: 'reference/policy-schema' },
						{ label: 'Audit Event Schema', slug: 'reference/audit-schema' },
					],
				},
			],
		}),
	],
});
