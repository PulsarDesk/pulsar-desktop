<script lang="ts">
	import Icon from '#lib/Icon.svelte';
	import { t } from '#lib/i18n.svelte.js';
	import { allPeers, fmtPeerId, normalizeId } from '#lib/peers.svelte.js';
	import { api, isTauri } from '#lib/api.js';
	import TabGhost from './TabGhost.svelte';

	type Tab = {
		tabId: number;
		phase: 'connecting' | 'active';
		target: { name: string; id: string };
		label?: string;
		mode?: 'remote' | 'game';
		/** Moved in from another window: plays an arrival animation once. */
		adopted?: boolean;
	};
	type Props = {
		sessions: Tab[];
		activeTab: 'home' | number;
		onSelect: (tab: 'home' | number) => void;
		onEnd: (tabId: number) => void;
		onRename: (tabId: number, name: string) => void;
		/** Show the Home tab (off in a detached tab window — it has no Home). */
		showHome?: boolean;
		/** Drag to reorder: tab `tabId` goes right before tab `before` (null = the end). */
		onReorder?: (tabId: number, before: number | null) => void;
		/** Whether tab `tabId` may leave this window (tear-off / merge). */
		canMove?: (tabId: number) => boolean;
		/** Tab `tabId` was dropped outside the strip. `grab`: where on the tab the pointer holds
		 * it (window client CSS px, as the tab would sit in a new window); `screen`: the
		 * pointer in screen px (estimate). */
		onDragOut?: (tabId: number, grab: [number, number], screen: [number, number]) => void;
		/** Whether tab `tabId` may split the session area (drag it there, IntelliJ-style). */
		canSplit?: (tabId: number) => boolean;
		/** The dragged tab is over the area below the strip (client px), or left it (null). */
		onStageDrag?: (tabId: number | null, x: number, y: number) => void;
		/** …and was dropped there. */
		onStageDrop?: (tabId: number, x: number, y: number) => void;
		/** A dragged tab was dropped back on the strip (after any reorder). */
		onStripDrop?: (tabId: number) => void;
		/** The split group, shown as ONE tab holding its members (null = no split). */
		group?: { members: number[]; shown: boolean } | null;
		/** The group tab was clicked (show the split). */
		onGroupSelect?: () => void;
		/** The group tab was dragged out of the window (the whole split moves). */
		onGroupDragOut?: (grab: [number, number], screen: [number, number]) => void;
	};
	let {
		sessions,
		activeTab,
		onSelect,
		onEnd,
		onRename,
		showHome = true,
		onReorder,
		canMove = () => false,
		onDragOut,
		canSplit = () => false,
		onStageDrag,
		onStageDrop,
		onStripDrop,
		group = null,
		onGroupSelect,
		onGroupDragOut
	}: Props = $props();

	// Group members sit inside the group tab; the rest are ordinary tabs (reorderable).
	const members = $derived(
		group ? group.members.map((id) => sessions.find((s) => s.tabId === id)).filter((s): s is Tab => !!s) : []
	);
	const others = $derived(group ? sessions.filter((s) => !group.members.includes(s.tabId)) : sessions);

	// ── Drag (Chrome-style): reorder inside the strip; dragged out of it (or onto another
	// Pulsar window's strip) the tab leaves this window. A press that doesn't move past the
	// threshold stays a click. The pointer is captured, so moves keep coming outside the window.
	const DRAG_SLOP = 6;
	/** How far below the strip the pointer must go before it's over the session area. */
	const STAGE_DIST = 24;
	let stripEl = $state<HTMLDivElement | null>(null);
	let drag = $state<{
		tabId: number;
		x0: number;
		y0: number;
		dx: number;
		moved: boolean;
		out: boolean;
		/** Over the session area (split drop), not the strip. */
		stage: boolean;
		grab: [number, number];
		/** Strip index the tab started at / would land at now (neighbours shift live). */
		from: number;
		to: number;
		/** Where each session tab's centre was at the press (strip order), and the dragged
		 * tab's width + gap — the room the neighbours make for it. */
		centres: number[];
		room: number;
		/** Off the strip: the tab's picture follows the pointer (client px). */
		gx: number;
		gy: number;
		off: boolean;
		/** The native ghost window is up (the pointer is outside this window). */
		ghostOut: boolean;
	} | null>(null);
	// Where the pointer holds the floating picture (matches tabwin.rs GHOST_GRAB).
	const GHOST_GRAB = { x: 24, y: 17 };
	let ghostMoving = false;
	function ghostOut(d: NonNullable<typeof drag>, on: boolean) {
		if (!isTauri || d.ghostOut === on) return;
		d.ghostOut = on;
		if (on) {
			const s = sessions.find((x) => x.tabId === d.tabId);
			api.tabGhostShow(s ? tabName(s) : '').catch(() => {});
		} else api.tabGhostHide().catch(() => {});
	}
	let justDragged = false;
	const dpr = () => (typeof window !== 'undefined' ? window.devicePixelRatio || 1 : 1);
	const screenOf = (e: PointerEvent): [number, number] => [e.screenX * dpr(), e.screenY * dpr()];

	function onTabDown(e: PointerEvent, s: Tab) {
		if (e.button !== 0 || editing === s.tabId) return;
		if ((e.target as HTMLElement | null)?.closest('button, input')) return;
		const el = e.currentTarget as HTMLElement;
		const r = el.getBoundingClientRect();
		// Where the pointer holds the tab in a new window: the strip sits at the same height
		// there and the tab is its first (8 px strip padding, no Home tab).
		const grab: [number, number] = [e.clientX - r.left + 8, e.clientY];
		const centres = stripEl
			? [...stripEl.querySelectorAll<HTMLElement>('[data-tab]')].map((x) => {
					const b = x.getBoundingClientRect();
					return b.left + b.width / 2;
				})
			: [];
		// Only ordinary tabs reorder (a group member's slot is its pane): -1 = no live reorder.
		const from = others.findIndex((x) => x.tabId === s.tabId);
		drag = {
			tabId: s.tabId,
			x0: e.clientX,
			y0: e.clientY,
			dx: 0,
			moved: false,
			out: false,
			stage: false,
			grab,
			from,
			to: from,
			centres,
			room: r.width + 2,
			gx: e.clientX,
			gy: e.clientY,
			off: false,
			ghostOut: false
		};
		el.setPointerCapture(e.pointerId);
	}

	function onTabMove(e: PointerEvent) {
		const d = drag;
		if (!d) return;
		if (!d.moved && Math.hypot(e.clientX - d.x0, e.clientY - d.y0) < DRAG_SLOP) return;
		d.moved = true;
		d.dx = e.clientX - d.x0;
		const strip = stripEl?.getBoundingClientRect();
		// Outside the window → its own window (or another window's strip); below the strip,
		// inside the window → split the session area; else → reorder along the strip.
		const outside = e.clientX < 0 || e.clientY < 0 || e.clientX > window.innerWidth || e.clientY > window.innerHeight;
		d.out = outside && canMove(d.tabId);
		const stage = !outside && !!strip && e.clientY > strip.bottom + STAGE_DIST && canSplit(d.tabId);
		if (stage) onStageDrag?.(d.tabId, e.clientX, e.clientY);
		else if (d.stage) onStageDrag?.(null, 0, 0);
		d.stage = stage;
		// The tab itself goes with the pointer once it leaves the strip: drawn here while the
		// pointer is over this window, as a tiny native window once it is outside.
		d.gx = e.clientX;
		d.gy = e.clientY;
		d.off = !!strip && (outside || e.clientY > strip.bottom + STAGE_DIST || e.clientY < strip.top - STAGE_DIST);
		ghostOut(d, outside);
		if (outside && !ghostMoving) {
			ghostMoving = true;
			api.tabGhostMove()
				.catch(() => {})
				.finally(() => (ghostMoving = false));
		}
		// Live reorder: the slot the dragged tab's centre has passed (neighbours slide aside).
		if (!stage && !d.out && d.from >= 0) {
			const c = d.centres[d.from] + d.dx;
			let to = 0;
			d.centres.forEach((x, i) => {
				if (i !== d.from && c > x) to++;
			});
			d.to = to;
		} else d.to = d.from;
	}

	/** How far a NEIGHBOUR slides while a tab is dragged past it (px; 0 = stays). */
	function shiftOf(i: number): number {
		const d = drag;
		if (!d || !d.moved || d.out || d.stage || d.from < 0 || i === d.from) return 0;
		if (d.from < d.to && i > d.from && i <= d.to) return -d.room;
		if (d.to < d.from && i >= d.to && i < d.from) return d.room;
		return 0;
	}

	function onTabUp(e: PointerEvent) {
		const d = drag;
		drag = null;
		if (d) ghostOut(d, false);
		if (!d || !d.moved) return;
		justDragged = true;
		setTimeout(() => (justDragged = false), 0);
		if (d.stage) {
			onStageDrag?.(null, 0, 0);
			onStageDrop?.(d.tabId, e.clientX, e.clientY);
			return;
		}
		if (d.out) {
			onDragOut?.(d.tabId, d.grab, screenOf(e));
			return;
		}
		if (d.from >= 0 && d.to !== d.from) {
			const rest = others.filter((x) => x.tabId !== d.tabId);
			onReorder?.(d.tabId, rest[d.to]?.tabId ?? null);
		}
		onStripDrop?.(d.tabId);
	}

	// ── The group tab: drag its handle out of the window to move the whole split; a press
	// that doesn't move is a click (show the split).
	let gdrag = $state<{ x0: number; y0: number; moved: boolean; off: boolean; ghostOut: boolean; gx: number; gy: number; grab: [number, number] } | null>(null);
	const groupTitle = $derived(t('tab.group', { n: members.length }));
	function onGroupDown(e: PointerEvent) {
		if (e.button !== 0) return;
		const el = e.currentTarget as HTMLElement;
		const r = el.getBoundingClientRect();
		gdrag = { x0: e.clientX, y0: e.clientY, moved: false, off: false, ghostOut: false, gx: e.clientX, gy: e.clientY, grab: [e.clientX - r.left + 8, e.clientY] };
		el.setPointerCapture(e.pointerId);
	}
	function onGroupMove(e: PointerEvent) {
		const g = gdrag;
		if (!g) return;
		if (!g.moved && Math.hypot(e.clientX - g.x0, e.clientY - g.y0) < DRAG_SLOP) return;
		g.moved = true;
		g.gx = e.clientX;
		g.gy = e.clientY;
		const strip = stripEl?.getBoundingClientRect();
		const outside = e.clientX < 0 || e.clientY < 0 || e.clientX > window.innerWidth || e.clientY > window.innerHeight;
		g.off = outside || (!!strip && (e.clientY > strip.bottom + STAGE_DIST || e.clientY < strip.top - STAGE_DIST));
		if (isTauri && outside !== g.ghostOut) {
			g.ghostOut = outside;
			if (outside) api.tabGhostShow(groupTitle).catch(() => {});
			else api.tabGhostHide().catch(() => {});
		}
		if (outside && !ghostMoving) {
			ghostMoving = true;
			api.tabGhostMove()
				.catch(() => {})
				.finally(() => (ghostMoving = false));
		}
	}
	function onGroupUp(e: PointerEvent) {
		const g = gdrag;
		gdrag = null;
		if (!g) return;
		if (g.ghostOut) api.tabGhostHide().catch(() => {});
		if (!g.moved) {
			onGroupSelect?.();
			return;
		}
		const outside = e.clientX < 0 || e.clientY < 0 || e.clientX > window.innerWidth || e.clientY > window.innerHeight;
		if (outside) onGroupDragOut?.(g.grab, screenOf(e));
	}
	function onGroupCancel() {
		if (gdrag?.ghostOut) api.tabGhostHide().catch(() => {});
		gdrag = null;
	}

	function onTabCancel() {
		if (drag?.stage) onStageDrag?.(null, 0, 0);
		if (drag) ghostOut(drag, false);
		drag = null;
	}

	// Inline rename: which tab is being edited + its draft text. Commit on
	// Enter/blur, cancel on Escape; an emptied name restores the default.
	let editing = $state<number | null>(null);
	let draft = $state('');
	function startEdit(s: Tab) {
		editing = s.tabId;
		draft = tabName(s);
	}

	// Default tab title. A game-mode tab's `target.name` is WHAT it streams (a game title,
	// "Desktop", "Screen 2", a window title) — on its own it doesn't say which host, so it
	// reads "<host name> (<what>)". The host name comes from the peer store, which the host's
	// pushed identity keeps up to date; an unknown host falls back to its id.
	function hostName(id: string): string {
		const nid = normalizeId(id);
		const n = allPeers().find((p) => p.id === nid)?.name?.trim() ?? '';
		return n && normalizeId(n) !== nid ? n : fmtPeerId(id);
	}
	function tabName(s: Tab): string {
		if (s.label) return s.label;
		if (s.mode !== 'game') return s.target.name;
		const host = hostName(s.target.id);
		return s.target.name && s.target.name !== host ? `${host} (${s.target.name})` : host;
	}
	function commitEdit() {
		if (editing !== null) onRename(editing, draft);
		editing = null;
	}
	function editKey(e: KeyboardEvent) {
		if (e.key === 'Enter') commitEdit();
		else if (e.key === 'Escape') editing = null;
	}
	// Focus the input as soon as it mounts (it appears on the pencil click).
	function autofocus(node: HTMLInputElement) {
		node.focus();
		node.select();
	}
</script>

{#snippet tabItem(s: Tab, i: number, member: boolean)}
	{@const dragging = drag?.tabId === s.tabId && drag.moved}
	<div
		class="tab"
		class:on={activeTab === s.tabId}
		class:arrived={s.adopted}
		class:dragging
		class:out={dragging && drag?.off}
		class:sliding={!!drag?.moved && !dragging && !member}
		class:member
		style={dragging && !drag?.off
			? `transform:translateX(${drag?.dx ?? 0}px)`
			: !member && shiftOf(i)
				? `transform:translateX(${shiftOf(i)}px)`
				: ''}
		data-tab={member ? undefined : s.tabId}
		data-member={member ? s.tabId : undefined}
		title={canMove(s.tabId) ? t('tab.dragHint') : undefined}
		role="button"
		tabindex="0"
		onpointerdown={(e) => onTabDown(e, s)}
		onpointermove={onTabMove}
		onpointerup={onTabUp}
		onpointercancel={onTabCancel}
		onclick={() => {
			if (!justDragged) onSelect(s.tabId);
		}}
		onkeydown={(e) => (e.key === 'Enter' || e.key === ' ') && onSelect(s.tabId)}
	>
		<span class="tdot" class:live={s.phase === 'active'}></span>
		{#if editing === s.tabId}
			<!-- svelte-ignore a11y_no_static_element_interactions -->
			<input
				class="tedit"
				bind:value={draft}
				use:autofocus
				onblur={commitEdit}
				onkeydown={(e) => {
					e.stopPropagation();
					editKey(e);
				}}
				onclick={(e) => e.stopPropagation()}
				aria-label={t('tab.rename')}
			/>
		{:else}
			<span class="tname">{tabName(s)}</span>
			<button
				class="tx tpen"
				aria-label={t('tab.rename')}
				title={t('tab.rename')}
				onclick={(e) => {
					e.stopPropagation();
					startEdit(s);
				}}
			>
				<Icon name="edit" size={11} />
			</button>
		{/if}
		<button
			class="tx"
			aria-label={t('tab.close')}
			onclick={(e) => {
				e.stopPropagation();
				onEnd(s.tabId);
			}}
		>
			<Icon name="x" size={11} />
		</button>
	</div>
{/snippet}

<div class="tabs" bind:this={stripEl}>
	{#if showHome}
		<button class="tab" class:on={activeTab === 'home'} onclick={() => onSelect('home')}>
			<Icon name="home" size={15} />{t('tab.home')}
		</button>
	{/if}
	{#if group && members.length}
		<!-- The split group: one tab holding its sessions (drag the handle to move them all). -->
		<div class="tgroup" class:on={group.shown} class:gdragging={gdrag?.moved && gdrag.off}>
			<div
				class="ghandle"
				role="button"
				tabindex="0"
				title={t('tab.groupHint')}
				aria-label={groupTitle}
				onpointerdown={onGroupDown}
				onpointermove={onGroupMove}
				onpointerup={onGroupUp}
				onpointercancel={onGroupCancel}
				onkeydown={(e) => (e.key === 'Enter' || e.key === ' ') && onGroupSelect?.()}
			>
				<Icon name="split" size={13} />
			</div>
			{#each members as s, i (s.tabId)}
				{@render tabItem(s, i, true)}
			{/each}
		</div>
	{/if}
	{#each others as s, i (s.tabId)}
		{@render tabItem(s, i, false)}
	{/each}
</div>
{#if gdrag?.moved && gdrag.off && !gdrag.ghostOut}
	<div class="ghost-float" style="left:{gdrag.gx - GHOST_GRAB.x}px;top:{gdrag.gy - GHOST_GRAB.y}px">
		<TabGhost title={groupTitle} />
	</div>
{/if}
{#if drag?.moved && drag.off && !drag.ghostOut}
	{@const ds = sessions.find((x) => x.tabId === drag?.tabId)}
	{#if ds}
		<div class="ghost-float" style="left:{drag.gx - GHOST_GRAB.x}px;top:{drag.gy - GHOST_GRAB.y}px">
			<TabGhost title={tabName(ds)} live={ds.phase === 'active'} />
		</div>
	{/if}
{/if}


<style>
	/* tab strip (home + connected hosts) */
	.tabs {
		flex: none;
		display: flex;
		align-items: stretch;
		gap: 2px;
		height: 36px;
		padding: 0 8px;
		background: var(--surface-2);
		border-bottom: 1px solid var(--border);
		overflow-x: auto;
	}
	.tab {
		display: inline-flex;
		align-items: center;
		gap: 7px;
		max-width: 220px;
		padding: 0 10px;
		border: none;
		border-bottom: 2px solid transparent;
		background: transparent;
		color: var(--text-muted);
		font-size: 13px;
		font-weight: 500;
		cursor: pointer;
		white-space: nowrap;
	}
	.tab:hover {
		background: var(--surface-3);
	}
	.tab.on {
		color: var(--accent-ink);
		border-bottom-color: var(--accent);
		background: var(--surface);
	}
	/* A tab dropped in from another window: drops into place with an accent flash. */
	.tab.arrived {
		animation: tab-arrive 0.6s ease-out;
	}
	@keyframes tab-arrive {
		0% {
			transform: translateY(-14px) scale(0.92);
			box-shadow: 0 0 0 2px var(--accent);
		}
		45% {
			transform: translateY(0) scale(1.03);
			box-shadow: 0 0 0 2px var(--accent);
		}
		100% {
			transform: none;
			box-shadow: 0 0 0 0 transparent;
		}
	}
	/* The split group: its members framed together, a handle (split icon) to grab them all. */
	.tgroup {
		display: inline-flex;
		align-items: stretch;
		gap: 1px;
		margin: 4px 2px;
		border: 1px solid var(--border-strong);
		border-radius: 7px;
		background: color-mix(in srgb, var(--accent) 6%, transparent);
	}
	.tgroup.on {
		border-color: var(--accent);
		background: color-mix(in srgb, var(--accent) 12%, transparent);
	}
	.tgroup.gdragging {
		opacity: 0.4;
		outline: 1px dashed var(--accent);
	}
	.ghandle {
		display: grid;
		place-items: center;
		width: 24px;
		color: var(--accent-ink);
		cursor: grab;
		border-right: 1px solid var(--border);
	}
	.ghandle:hover {
		background: var(--surface-3);
	}
	.tab.member {
		max-width: 160px;
		border-bottom-width: 0;
		border-radius: 0;
	}
	.tab.member.on {
		background: var(--surface);
		box-shadow: inset 0 -2px 0 var(--accent);
	}
	/* Neighbours slide aside while a tab is dragged past them. */
	.tab.sliding {
		transition: transform 0.15s ease;
	}
	/* Being dragged: follows the pointer along the strip, above its neighbours. */
	.tab.dragging {
		position: relative;
		z-index: 2;
		cursor: grabbing;
		box-shadow: 0 2px 10px rgb(0 0 0 / 0.25);
		background: var(--surface);
	}
	/* Off the strip: its slot stays as a faint placeholder; the tab itself (.ghost-float or the
	   native ghost window) is under the pointer. */
	.tab.out {
		opacity: 0.35;
		outline: 1px dashed var(--accent);
	}
	.ghost-float {
		position: fixed;
		z-index: 1000;
		pointer-events: none;
	}
	.tname {
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.tedit {
		width: 110px;
		border: 1px solid var(--accent);
		border-radius: 5px;
		background: var(--surface);
		color: var(--text);
		font-size: 12.5px;
		padding: 2px 6px;
		outline: none;
	}
	.tdot {
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: var(--border-strong);
		flex: none;
	}
	.tdot.live {
		background: var(--ok);
	}
	.tx {
		display: grid;
		place-items: center;
		width: 18px;
		height: 18px;
		border: none;
		border-radius: 5px;
		background: transparent;
		color: var(--text-faint);
		cursor: pointer;
	}
	.tx:hover {
		background: var(--surface-3);
		color: var(--text);
	}
	/* The pencil stays invisible until the tab is hovered (keeps the strip clean). */
	.tpen {
		opacity: 0;
		transition: opacity 0.12s;
	}
	.tab:hover .tpen,
	.tpen:focus-visible {
		opacity: 1;
	}
</style>
