<script lang="ts">
	// The picture of a dragged session tab: under the cursor inside the window (Tabs.svelte),
	// and alone in the tiny always-on-top `tabghost` window while the drag is outside it.
	type Props = { title: string; live?: boolean };
	let { title, live = true }: Props = $props();
</script>

<div class="ghost">
	<span class="dot" class:live></span>
	<span class="name">{title}</span>
</div>

<style>
	.ghost {
		display: inline-flex;
		align-items: center;
		gap: 7px;
		box-sizing: border-box;
		width: 220px;
		height: 34px;
		padding: 0 12px;
		border: 1px solid var(--accent);
		border-radius: 8px;
		background: var(--surface);
		color: var(--accent-ink);
		font-size: 13px;
		font-weight: 600;
		white-space: nowrap;
		box-shadow: 0 6px 18px rgb(0 0 0 / 0.28);
		pointer-events: none;
		user-select: none;
	}
	.name {
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.dot {
		flex: none;
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: var(--border-strong);
	}
	.dot.live {
		background: var(--ok);
	}
</style>
