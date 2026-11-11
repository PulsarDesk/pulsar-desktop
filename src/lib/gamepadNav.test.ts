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
	const cards: { card: HTMLButtonElement; go: HTMLButtonElement; star: HTMLButtonElement }[] = [];
	for (const [i, x] of [0, 230].entries()) {
		const row = document.createElement('div');
		row.setAttribute('data-navgroup', '');
		root.appendChild(row);
		const card = box(btn(`card${i + 1}`, row), x, 0, 220, 60);
		card.setAttribute('data-navcard', '');
		const go = box(btn(`go${i + 1}`, row), x + 150, 16, 28, 28);
		const star = box(btn(`star${i + 1}`, row), x + 184, 16, 28, 28);
		for (const el of [card, go, star]) nav.item(el);
		cards.push({ card, go, star });
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

	it('moves card to card, never onto a card’s inner buttons', () => {
		layout(nav);
		key(nav, 'ArrowRight'); // first input only reveals the ring at the default
		expect(focused()).toBe('card1');
		key(nav, 'ArrowRight'); // go1/star1 sit closer, but the next CARD wins
		expect(focused()).toBe('card2');
		key(nav, 'ArrowRight');
		expect(focused()).toBe('card2');
		key(nav, 'ArrowLeft');
		expect(focused()).toBe('card1');
		key(nav, 'ArrowDown');
		expect(focused()).toBe('card3');
	});

	it('Enter enters a card, arrows walk its buttons, Escape returns to the card', () => {
		const { cards } = layout(nav);
		const cardClick = vi.fn();
		cards[0].card.addEventListener('click', cardClick);
		key(nav, 'ArrowRight'); // wake on card1
		key(nav, 'Enter');
		expect(cardClick).not.toHaveBeenCalled(); // entered, not activated
		expect(focused()).toBe('go1');
		expect(nav.inCard).toBe(true);
		key(nav, 'ArrowRight');
		expect(focused()).toBe('star1');
		key(nav, 'ArrowRight'); // confined to the card: card2 is not reachable from inside
		expect(focused()).toBe('star1');
		key(nav, 'ArrowDown');
		expect(focused()).toBe('star1');
		key(nav, 'ArrowLeft');
		expect(focused()).toBe('go1');
		const goClick = vi.fn();
		cards[0].go.addEventListener('click', goClick);
		key(nav, 'Enter'); // A on an inner button clicks it
		expect(goClick).toHaveBeenCalledOnce();
		key(nav, 'Escape');
		expect(focused()).toBe('card1');
		expect(nav.inCard).toBe(false);
		key(nav, 'ArrowRight');
		expect(focused()).toBe('card2');
	});

	it('a card without buttons is clicked on Enter', () => {
		const { bare } = layout(nav);
		const click = vi.fn();
		bare.addEventListener('click', click);
		key(nav, 'ArrowDown'); // wake on card1
		key(nav, 'ArrowDown');
		expect(focused()).toBe('card3');
		key(nav, 'Enter');
		expect(click).toHaveBeenCalledOnce();
	});

	it('a modal’s back handler wins over leaving the card; the card stays entered', () => {
		const onBack = vi.fn();
		nav.setOpts({ onBack });
		const { cards } = layout(nav);
		key(nav, 'ArrowRight');
		key(nav, 'Enter');
		key(nav, 'ArrowRight'); // star1
		const close = vi.fn();
		nav.pushBack(close);
		key(nav, 'Escape');
		expect(close).toHaveBeenCalledOnce();
		expect(focused()).toBe('star1');
		nav.popBack(close);
		nav.focus(cards[0].star); // what the confirm does on cancel
		expect(nav.inCard).toBe(true);
		key(nav, 'Escape');
		expect(focused()).toBe('card1');
		key(nav, 'Escape'); // outside a card: the shell's back
		expect(onBack).toHaveBeenCalledOnce();
	});

	it('leaves the entered state when the card is removed', () => {
		const { cards } = layout(nav);
		key(nav, 'ArrowRight');
		key(nav, 'Enter');
		cards[0].card.parentElement!.remove();
		expect(nav.inCard).toBe(false);
		nav.focusFirst();
		expect(focused()).toBe('card2');
	});
});
