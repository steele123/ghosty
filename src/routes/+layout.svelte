<script lang="ts">
	import './layout.css';
	import { browser } from "$app/environment";
	import { page } from "$app/stores";
	import GhostyShell from "$lib/GhostyShell.svelte";

	type AppTab = "launch" | "presence" | "utility" | "debug";

	let { children } = $props();

	if (browser) {
		document.documentElement.classList.add("dark");
	}

	function activeTabFor(pathname: string): AppTab {
		if (pathname.startsWith("/presence")) {
			return "presence";
		}

		if (pathname.startsWith("/utility")) {
			return "utility";
		}

		if (pathname.startsWith("/debug")) {
			return "debug";
		}

		return "launch";
	}
</script>

<GhostyShell activeTab={activeTabFor($page.url.pathname)} />
<div hidden>{@render children()}</div>
