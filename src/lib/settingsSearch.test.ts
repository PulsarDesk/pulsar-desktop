import { describe, it, expect } from 'vitest';
import { fold, matches, queryWords, applySettingsFilter } from './settingsSearch.js';

describe('settings search matching', () => {
	it('folds case, accents and the Turkish dotless i, keeping the length', () => {
		expect(fold('Işık Çubuğu')).toBe('isik cubugu');
		expect(fold('İMLEÇ')).toBe('imlec');
		expect(fold('Işık Çubuğu').length).toBe('Işık Çubuğu'.length);
	});

	it('needs every word, in any order', () => {
		const w = queryWords('  imlec   serbest ');
		expect(matches('Serbest imleç — İmleç videodan çıkınca…', w)).toBe(true);
		expect(matches('Fare hassasiyeti', w)).toBe(false);
		expect(matches('anything', [])).toBe(true);
	});
});

describe('applySettingsFilter', () => {
	function dom() {
		const root = document.createElement('div');
		root.innerHTML = `
			<section class="tabpane"><div class="tabhead">Yayın</div>
				<div class="subgroup">Görüntü kalitesi</div><div class="subdesc">…</div>
				<div class="srow" id="codec"><b>Video codec</b><span>Sıkıştırma biçimi</span></div>
				<div class="detnote" id="note">bulunan encoderlar</div>
				<div class="subgroup" id="g2">Fare ve kontrol</div><div class="subdesc" id="d2">…</div>
				<div class="srow" id="free"><b>Serbest imleç</b><span>İmleç videodan çıkınca</span></div>
			</section>
			<section class="tabpane" id="net"><div class="tabhead">Ağ</div>
				<div class="srow"><b>Relay sunucusu</b></div>
			</section>`;
		return root;
	}

	it('hides non-matching rows, empty categories and empty tabs', () => {
		const root = dom();
		expect(applySettingsFilter(root, 'codec')).toBe(1);
		const q = (s: string) => root.querySelector<HTMLElement>(s)!;
		expect(q('#codec').hidden).toBe(false);
		expect(q('#free').hidden).toBe(true);
		expect(q('#g2').hidden).toBe(true); // no visible row left under it
		expect(q('#d2').hidden).toBe(true);
		expect(q('#note').hidden).toBe(true); // loose notes hide while searching
		expect(q('#net').hidden).toBe(true);
	});

	it('an empty query shows everything again; no match gives 0', () => {
		const root = dom();
		expect(applySettingsFilter(root, 'zzz')).toBe(0);
		expect(applySettingsFilter(root, '')).toBe(3);
		for (const el of root.querySelectorAll<HTMLElement>('*')) expect(el.hidden).toBe(false);
	});
});
