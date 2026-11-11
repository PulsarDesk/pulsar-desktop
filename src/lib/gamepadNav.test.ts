import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { GamepadNav } from './gamepadNav.svelte';

// jsdom has no layout: give each element a fixed box so the spatial nav can work.
function box<T extends HTMLElement>(el: T, left: number, top: number, width: number, height: number) {
	el.getBoundingClientRect = () =>
		({ left, top, width, height, right: left + width, bottom: top + height, x: left, y: top }) as DOMRect;
	return el;
}
function btn(label: string, parent: HTMLElement) {
	const b = document.createElement('button');
	b.textContent = label;
	parent.appendChild(b);
	return b;
}
const key = (nav: GamepadNav, k: string) => nav.onKeydown(new KeyboardEvent('keydown', { key: k }));
const focused = () => document.querySelector('[data-navfocus]')?.textContent;

/** Two device cards side by side, each with two buttons floating over its right edge (the
 * gaming Home layout), plus a third card without buttons below. */
function layout(nav: GamepadNav) {
	const root = document.createElement('div');
	document.body.appendChild(root);
	const cards: { card: HTMLButtonElement; star: HTMLButtonElement; rm: HTMLButtonElement }[] = [];
	for (const [i, x] of [0, 230].entries()) {
		const row = document.createElement('div');
		row.setAttribute('data-navgroup', '');
		root.appendChild(row);
		const card = box(btn(`card${i + 1}`, row), x, 0, 220, 60);
		card.setAttribute('data-navcard', '');
		const star = box(btn(`star${i + 1}`, row), x + 150, 16, 28, 28);
		const rm = box(btn(`rm${i + 1}`, row), x + 184, 16, 28, 28);
		for (const el of [card, star, rm]) nav.item(el);
		cards.push({ card, star, rm });
	}
	const row3 = document.createElement('div');
	row3.setAttribute('data-navgroup', '');
	root.appendChild(row3);
	const bare = box(btn('card3', row3), 0, 80, 220, 60);
	bare.setAttribute('data-navcard', '');
	nav.item(bare);
	cards[0].card.setAttribute('data-navdefault', '');
	return { root, cards, bare };
}

describe('GamepadNav card groups', () => {
	let nav: GamepadNav;
	beforeEach(() => {
		Element.prototype.scrollIntoView ??= () => {}; // jsdom has no scrolling
		nav = new GamepadNav();
	});
	afterEach(() => {
		nav.stop();
		document.body.innerHTML = '';
	});

	it('right walks into the card’s own buttons, then on to the next card itself', () => {
		layout(nav);
		key(nav, 'ArrowRight'); // first input only reveals the ring at the default
		expect(focused()).toBe('card1');
		key(nav, 'ArrowRight');
		expect(focused()).toBe('star1');
		key(nav, 'ArrowRight');
		expect(focused()).toBe('rm1');
		key(nav, 'ArrowRight'); // past the last button: the next CARD, not its buttons
		expect(focused()).toBe('card2');
		key(nav, 'ArrowRight');
		expect(focused()).toBe('star2');
		key(nav, 'ArrowRight');
		key(nav, 'ArrowRight'); // nothing further right: stays
		expect(focused()).toBe('rm2');
	});

	it('left walks back through the buttons to the card, and never into another card’s buttons', () => {
		const { cards } = layout(nav);
		key(nav, 'ArrowRight'); // wake on card1
		nav.focus(cards[1].rm);
		key(nav, 'ArrowLeft');
		expect(focused()).toBe('star2');
		key(nav, 'ArrowLeft');
		expect(focused()).toBe('card2');
		key(nav, 'ArrowLeft'); // rm1/star1 sit closer, but the previous CARD wins
		expect(focused()).toBe('card1');
		key(nav, 'ArrowLeft');
		expect(focused()).toBe('card1');
	});

	it('up/down go card to card, from a card or from its buttons', () => {
		const { cards } = layout(nav);
		key(nav, 'ArrowRight'); // wake on card1
		key(nav, 'ArrowRight'); // star1
		key(nav, 'ArrowDown');
		expect(focused()).toBe('card3');
		key(nav, 'ArrowUp');
		expect(focused()).toBe('card1');
		nav.focus(cards[1].rm);
		key(nav, 'ArrowDown'); // measured from card2: the card below-left, not a button
		expect(focused()).toBe('card3');
	});

	it('A on a card clicks it (connects); A on a button clicks the button', () => {
		const { cards, bare } = layout(nav);
		const cardClick = vi.fn();
		const starClick = vi.fn();
		const bareClick = vi.fn();
		cards[0].card.addEventListener('click', cardClick);
		cards[0].star.addEventListener('click', starClick);
		bare.addEventListener('click', bareClick);
		key(nav, 'Enter'); // a cold A wakes on the default and clicks it
		expect(cardClick).toHaveBeenCalledOnce();
		key(nav, 'ArrowRight');
		key(nav, 'Enter');
		expect(starClick).toHaveBeenCalledOnce();
		expect(cardClick).toHaveBeenCalledOnce();
		key(nav, 'ArrowDown');
		key(nav, 'Enter');
		expect(bareClick).toHaveBeenCalledOnce();
	});

	it('B/Esc on a button returns to its card; on a card it runs the shell’s back; a modal wins', () => {
		const onBack = vi.fn();
		nav.setOpts({ onBack });
		layout(nav);
		key(nav, 'ArrowRight'); // card1
		key(nav, 'ArrowRight'); // star1
		key(nav, 'ArrowRight'); // rm1
		expect(nav.inCard).toBe(true);
		const close = vi.fn();
		nav.pushBack(close); // e.g. the remove confirm opened from rm1
		key(nav, 'Escape');
		expect(close).toHaveBeenCalledOnce();
		expect(focused()).toBe('rm1');
		nav.popBack(close);
		key(nav, 'Escape');
		expect(focused()).toBe('card1');
		expect(nav.inCard).toBe(false);
		expect(onBack).not.toHaveBeenCalled();
		key(nav, 'Escape');
		expect(onBack).toHaveBeenCalledOnce();
	});

	it('a removed card no longer counts as focus inside a card', () => {
		const onBack = vi.fn();
		nav.setOpts({ onBack });
		const { cards } = layout(nav);
		key(nav, 'ArrowRight');
		key(nav, 'ArrowRight'); // star1
		cards[0].card.parentElement!.remove();
		expect(nav.inCard).toBe(false);
		key(nav, 'Escape');
		expect(onBack).toHaveBeenCalledOnce();
	});
});
