import { describe, expect, it } from 'vitest';
import modalSrc from './Modal.svelte?raw';

// `app.css?raw` comes back empty under vitest (CSS processing is off), so read the file from disk
// (vitest runs from the repo root). The specifier is widened to `string` because the project has
// no @types/node — the tests run in Node, the app type-checks against the DOM only.
const fs = await import('node:fs' as string);
const appCss: string = fs.readFileSync('src/app.css', 'utf8');

// jsdom does not cascade component styles, so these guard the CSS source itself.
// Regression for issue #1: the "Add device" dialog sat under its own backdrop and off-centre.

/** Every `prop: value` declared inside the first `selector { … }` block of `css`. */
function decls(css: string, selector: string, prop: string): string[] {
	const block = css.match(new RegExp(`\\n\\s*${selector.replace('.', '\\.')}\\s*\\{([^}]*)\\}`));
	expect(block, `${selector} block`).not.toBeNull();
	return [...block![1].matchAll(new RegExp(`(?:^|[;\\s])${prop}:\\s*([^;]+);`, 'g'))].map((m) =>
		m[1].trim()
	);
}

describe('Modal styles', () => {
	it('stacks the dialog above its backdrop', () => {
		const modalZ = decls(modalSrc, '.modal', 'z-index');
		const backdropZ = decls(modalSrc, '.backdrop', 'z-index');
		expect(modalZ).toHaveLength(1);
		expect(backdropZ).toHaveLength(1);
		expect(Number(modalZ[0])).toBeGreaterThan(Number(backdropZ[0]));
	});

	it('keeps the entrance animation off `transform` so the centring survives', () => {
		const keyframes = appCss.match(/@keyframes pulsar-modal-in\s*\{([\s\S]*?)\n\}/);
		expect(keyframes).not.toBeNull();
		expect(keyframes![1]).not.toMatch(/(^|\s)transform:/);
	});

	it('makes the app non-selectable by default, text fields excepted', () => {
		expect(appCss).toMatch(/\nbody \{[^}]*\buser-select: none;/);
		expect(appCss).toMatch(/input,\s*textarea,[^{]*\.selectable\s*\{\s*-webkit-user-select: text;\s*user-select: text;/);
	});
});
