// Settings search (issue #6): filter the rendered settings rows by their label + description.
//
// The rows are the tabs' real markup (`.srow`), so a match stays an editable control — no
// second copy of any setting. While a query is set, Settings renders every tab stacked (one
// `.tabpane` each) and `applySettingsFilter` hides what doesn't match: rows, then category
// headers (`.subgroup` + its `.subdesc`) left with no visible row, then tab panes left
// empty. Other loose nodes (notes, hints) are hidden while searching. Matches are
// highlighted with the CSS Custom Highlight API where the webview has it (Chromium/WebView2,
// recent WebKit); elsewhere the filter alone still works.

/** Case-, accent- and Turkish-dotless-insensitive form of ONE character, so indices line up
 * between the original text and its folded form (each char folds to exactly one char). */
function foldChar(c: string): string {
	if (c === 'ı' || c === 'İ' || c === 'I') return 'i';
	const lower = c.toLocaleLowerCase('tr');
	const base = lower.normalize('NFD').replace(/[̀-ͯ]/g, '');
	return base.length === 1 ? base : lower.length === 1 ? lower : c;
}

/** Fold a string char by char (same length as the input). */
export function fold(s: string): string {
	let out = '';
	for (const c of s) out += foldChar(c);
	return out;
}

/** The query's words (folded); a row matches when every word occurs in its text. */
export function queryWords(q: string): string[] {
	return fold(q.trim())
		.split(/\s+/)
		.filter((w) => w.length > 0);
}

export function matches(text: string, words: string[]): boolean {
	if (words.length === 0) return true;
	const f = fold(text);
	return words.every((w) => f.includes(w));
}

const HIGHLIGHT = 'pulsar-settings-search';

type HighlightRegistry = { set(name: string, h: unknown): void; delete(name: string): void };
function registry(): HighlightRegistry | null {
	const css = (globalThis as { CSS?: { highlights?: HighlightRegistry } }).CSS;
	const H = (globalThis as { Highlight?: new (...r: Range[]) => unknown }).Highlight;
	return css?.highlights && H ? css.highlights : null;
}

function highlight(rows: HTMLElement[], words: string[]) {
	const reg = registry();
	if (!reg) return;
	if (words.length === 0 || rows.length === 0) {
		reg.delete(HIGHLIGHT);
		return;
	}
	const ranges: Range[] = [];
	for (const row of rows) {
		const walker = document.createTreeWalker(row, NodeFilter.SHOW_TEXT);
		for (let n = walker.nextNode(); n; n = walker.nextNode()) {
			const text = n.nodeValue ?? '';
			const f = fold(text);
			for (const w of words) {
				for (let i = f.indexOf(w); i >= 0; i = f.indexOf(w, i + w.length)) {
					const r = document.createRange();
					r.setStart(n, i);
					r.setEnd(n, i + w.length);
					ranges.push(r);
				}
			}
		}
	}
	const H = (globalThis as unknown as { Highlight: new (...r: Range[]) => unknown }).Highlight;
	reg.set(HIGHLIGHT, new H(...ranges));
}

const ROW = '.srow';

/** Show/hide the panes' rows for query `q` ('' = show everything). Returns the number of
 * rows left visible. `root` holds the `.tabpane` elements (or one tab's nodes directly). */
export function applySettingsFilter(root: HTMLElement, q: string): number {
	const words = queryWords(q);
	const searching = words.length > 0;
	const panes = Array.from(root.querySelectorAll<HTMLElement>('.tabpane'));
	const scopes = panes.length ? panes : [root];
	let shown = 0;
	const visibleRows: HTMLElement[] = [];
	for (const pane of scopes) {
		let paneShown = 0;
		// Direct children in order: a category header owns the rows until the next header.
		let header: HTMLElement[] = [];
		let headerShown = 0;
		const closeHeader = () => {
			for (const h of header) h.hidden = searching && headerShown === 0;
			header = [];
			headerShown = 0;
		};
		for (const el of Array.from(pane.children) as HTMLElement[]) {
			if (el.classList.contains('tabhead')) continue;
			if (el.classList.contains('subgroup')) {
				closeHeader();
				header.push(el);
				continue;
			}
			if (el.classList.contains('subdesc')) {
				header.push(el);
				continue;
			}
			const isRow = el.matches(ROW);
			const ok = !searching || (isRow && matches(el.textContent ?? '', words));
			el.hidden = !ok;
			if (ok && isRow) {
				headerShown++;
				paneShown++;
				visibleRows.push(el);
			}
		}
		closeHeader();
		if (panes.length) pane.hidden = searching && paneShown === 0;
		shown += paneShown;
	}
	highlight(searching ? visibleRows : [], words);
	return shown;
}

/** Drop the match highlight (Settings unmounted or the query cleared). */
export function clearSettingsHighlight() {
	registry()?.delete(HIGHLIGHT);
}
