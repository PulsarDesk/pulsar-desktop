import { defineConfig } from 'vitest/config';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// `#lib/*` resolves through the `imports` field of package.json (SvelteKit 3 dropped `$lib`).
export default defineConfig({
	plugins: [
		svelte({
			// Mirrors vite.config.ts (there is no svelte.config.js to pick this up from).
			compilerOptions: {
				runes: ({ filename }) =>
					filename.split(/[/\\]/).includes('node_modules') ? undefined : true
			}
		})
	],
	resolve: {
		conditions: ['browser']
	},
	test: {
		environment: 'jsdom',
		globals: true,
		setupFiles: ['./vitest-setup.ts'],
		include: ['src/**/*.{test,spec}.{js,ts}']
	}
});
