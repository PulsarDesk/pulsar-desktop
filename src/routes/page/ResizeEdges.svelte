<script lang="ts">
	// Resize grips along the window's edges. The window is frameless (decorations:false) and on
	// Linux (GTK under XWayland) that leaves NO native resize border, so a thin strip on each edge
	// and corner hands the drag to the window manager — which still enforces tauri.conf's
	// minWidth/minHeight. On Windows/macOS tao keeps native resizing for undecorated windows, so
	// this is Linux-only. Hidden in fullscreen and while maximized (nothing to resize then).
	import { onMount } from 'svelte';
	import { isTauri, isWindowMaximized, startWindowResize, type ResizeEdge } from '#lib/api.js';

	let { fullscreen = false }: { fullscreen?: boolean } = $props();

	const linux = typeof navigator !== 'undefined' && /Linux/.test(navigator.userAgent) && !/Android/.test(navigator.userAgent);
	const EDGES: ResizeEdge[] = ['North', 'South', 'East', 'West', 'NorthEast', 'NorthWest', 'SouthEast', 'SouthWest'];

	let maximized = $state(false);
	onMount(() => {
		if (!isTauri || !linux) return;
		let unlisten: (() => void) | undefined;
		(async () => {
			try {
				const { getCurrentWindow } = await import('@tauri-apps/api/window');
				maximized = await isWindowMaximized();
				unlisten = await getCurrentWindow().onResized(async () => {
					maximized = await isWindowMaximized();
				});
			} catch {
				/* not in Tauri / API unavailable */
			}
		})();
		return () => unlisten?.();
	});

	function grab(e: PointerEvent, edge: ResizeEdge) {
		if (e.button !== 0) return;
		e.preventDefault();
		startWindowResize(edge).catch((err) => console.warn('[resize] drag failed', err));
	}
</script>

{#if isTauri && linux && !fullscreen && !maximized}
	{#each EDGES as edge (edge)}
		<div class="grip {edge}" role="presentation" onpointerdown={(e) => grab(e, edge)}></div>
	{/each}
{/if}

<style>
	.grip {
		position: fixed;
		z-index: 1000; /* above every modal/backdrop: a resize always works */
	}
	.North,
	.South {
		left: 10px;
		right: 10px;
		height: 5px;
		cursor: ns-resize;
	}
	.North {
		top: 0;
	}
	.South {
		bottom: 0;
	}
	.East,
	.West {
		top: 10px;
		bottom: 10px;
		width: 5px;
		cursor: ew-resize;
	}
	.East {
		right: 0;
	}
	.West {
		left: 0;
	}
	.NorthEast,
	.NorthWest,
	.SouthEast,
	.SouthWest {
		width: 10px;
		height: 10px;
	}
	.NorthWest {
		top: 0;
		left: 0;
		cursor: nwse-resize;
	}
	.SouthEast {
		bottom: 0;
		right: 0;
		cursor: nwse-resize;
	}
	.NorthEast {
		top: 0;
		right: 0;
		cursor: nesw-resize;
	}
	.SouthWest {
		bottom: 0;
		left: 0;
		cursor: nesw-resize;
	}
</style>
