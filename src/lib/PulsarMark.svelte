<script lang="ts">
	let { size = 28, color = 'var(--mark)' }: { size?: number; color?: string } = $props();
</script>

<svg class="pulsar-mark" width={size} height={size} viewBox="0 0 52 52" fill="none" aria-hidden="true">
	<circle class="ring outer" cx="26" cy="26" r="24" stroke={color} stroke-width="1.5" />
	<circle class="ring mid" cx="26" cy="26" r="16" stroke={color} stroke-width="1.75" />
	<circle cx="26" cy="26" r="8.5" stroke={color} stroke-width="2" />
	<circle cx="26" cy="26" r="3.5" fill={color} />
</svg>

<style>
	.outer {
		opacity: 0.18;
	}
	.mid {
		opacity: 0.42;
	}
	/* The faint pulse rings vanish against the dark surfaces at their light-theme opacity. */
	:global(:root[data-theme='dark']) .outer {
		opacity: 0.32;
	}
	:global(:root[data-theme='dark']) .mid {
		opacity: 0.6;
	}
</style>
