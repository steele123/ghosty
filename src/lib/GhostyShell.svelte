<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { onMount } from "svelte";
  import { Badge } from "$lib/components/ui/badge";
  import { Button } from "$lib/components/ui/button";
  import { Input } from "$lib/components/ui/input";
  import { Kbd } from "$lib/components/ui/kbd";
  import { ScrollArea } from "$lib/components/ui/scroll-area";
  import { Spinner } from "$lib/components/ui/spinner";
  import {
    Activity,
    Clipboard,
    Gamepad2,
    HeartPulse,
    Keyboard,
    ListChecks,
    Maximize2,
    MessageSquare,
    Minus,
    Power,
    RefreshCcw,
    Search,
    Shield,
    Square,
    Trash2,
    X
  } from "lucide-svelte";

  type LaunchGame = "lol" | "lor" | "valorant" | "lion" | "riotClient";
  type PresenceStatus = "chat" | "offline" | "mobile";
  type StartupStatus = "chat" | "offline" | "mobile" | "last";
  type AppTab = "launch" | "presence" | "utility" | "debug";
  type HealthState = "waiting" | "ready" | "active" | "warning" | "error";
  type LogCategory = "config" | "chat" | "launch" | "error" | "system";
  type LogLevel = "info" | "warn" | "error";

  type HealthStep = {
    key: string;
    label: string;
    state: HealthState;
    detail: string;
  };

  type ConnectionHealth = {
    configProxy: HealthStep;
    configPatched: HealthStep;
    chatServer: HealthStep;
    tlsConnected: HealthStep;
    xmppActive: HealthStep;
    activeConnections: number;
    reconnectAttempts: number;
  };

  type LogEntry = {
    timestamp: string;
    level: LogLevel;
    category: LogCategory;
    message: string;
  };

  type StreamEvent = {
    timestamp: string;
    direction: string;
    bytes: number;
    preview: string;
  };

  type LcuApiResponse = {
    method: string;
    endpoint: string;
    url: string;
    port: number;
    status: number;
    ok: boolean;
    body: unknown | null;
    text: string;
  };

  type PreflightReport = {
    ok: boolean;
    checks: Array<{ label: string; ok: boolean; detail: string }>;
  };

  type AppSnapshot = {
    running: boolean;
    enabled: boolean;
    safeMode: boolean;
    helperFriend: boolean;
    autoAccept: boolean;
    autoAcceptDelayMs: number;
    autoAcceptState: string;
    discordWebhookUrl: string;
    status: PresenceStatus;
    startupStatus: StartupStatus;
    connectToMuc: boolean;
    health: ConnectionHealth;
    chatPort: number | null;
    configPort: number | null;
    riotChatHost: string | null;
    riotChatPort: number | null;
    riotClientPath: string | null;
    activeGame: LaunchGame | null;
    activeGameLabel: string | null;
    logs: LogEntry[];
    streamEvents: StreamEvent[];
  };

  const games: Array<{ id: LaunchGame; label: string; hint: string }> = [
    { id: "lol", label: "League", hint: "league_of_legends" },
    { id: "valorant", label: "VALORANT", hint: "valorant" },
    { id: "lor", label: "Runeterra", hint: "bacon" },
    { id: "lion", label: "2XKO", hint: "lion" },
    { id: "riotClient", label: "Riot Client", hint: "no product flag" }
  ];

  const statuses: Array<{ id: PresenceStatus; label: string }> = [
    { id: "offline", label: "Offline" },
    { id: "mobile", label: "Mobile" },
    { id: "chat", label: "Online" }
  ];

  const startupStatuses: Array<{ id: StartupStatus; label: string }> = [
    { id: "last", label: "Remember Last" },
    { id: "offline", label: "Offline" },
    { id: "mobile", label: "Mobile" },
    { id: "chat", label: "Online" }
  ];

  const lcuMethods = ["GET", "POST", "PUT", "PATCH", "DELETE"];
  const lcuEndpoints: Array<{ path: string; label: string }> = [
    { path: "/lol-summoner/v1/current-summoner", label: "Current Summoner" },
    { path: "/lol-chat/v1/me", label: "Chat Me" },
    { path: "/lol-chat/v1/friends", label: "Friends" },
    { path: "/lol-gameflow/v1/gameflow-phase", label: "Gameflow Phase" },
    { path: "/lol-gameflow/v1/session", label: "Gameflow Session" },
    { path: "/lol-champ-select/v1/session", label: "Champ Select Session" },
    { path: "/lol-lobby/v2/lobby", label: "Lobby" },
    { path: "/lol-login/v1/session", label: "Login Session" },
    { path: "/lol-platform-config/v1/namespaces", label: "Platform Config Namespaces" },
    { path: "/lol-store/v1/wallet", label: "Wallet" },
    { path: "/riotclient/region-locale", label: "Region Locale" },
    { path: "/riotclient/ux-state", label: "Riot UX State" }
  ];

  let snapshot = $state<AppSnapshot>({
    running: false,
    enabled: true,
    safeMode: false,
    helperFriend: false,
    autoAccept: false,
    autoAcceptDelayMs: 2000,
    autoAcceptState: "Disabled",
    discordWebhookUrl: "",
    status: "offline",
    startupStatus: "last",
    connectToMuc: true,
    health: emptyHealth(),
    chatPort: null,
    configPort: null,
    riotChatHost: null,
    riotChatPort: null,
    riotClientPath: null,
    activeGame: null,
    activeGameLabel: null,
    logs: [],
    streamEvents: []
  });
  let selectedGame = $state<LaunchGame>("lol");
  let gamePatchline = $state("live");
  let riotClientParams = $state("");
  let gameParams = $state("");
  let launchGame = $state(true);
  let pendingActions = $state(0);
  let error = $state("");
  let refreshError = $state("");
  let notice = $state("");
  let logFilter = $state<"all" | LogCategory>("all");
  let preflightReport = $state<PreflightReport | null>(null);
  let riotRestartDialogOpen = $state(false);
  let pendingRiotProcesses = $state<string[]>([]);
  let streamAutoScroll = $state(true);
  let streamLogElement = $state<HTMLDivElement | null>(null);
  let lcuMethod = $state("GET");
  let lcuEndpoint = $state("/lol-summoner/v1/current-summoner");
  let lcuBody = $state("");
  let lcuResponse = $state<LcuApiResponse | null>(null);
  let lastStreamEventCount = 0;
  let snapshotRequestId = 0;
  let launchFormInitialized = false;
  let busy = $derived(pendingActions > 0);
  let patchlineError = $derived(validatePatchline(gamePatchline, selectedGame));
  let launchBlocked = $derived(busy || patchlineError !== "");
  const appWindow = getCurrentWindow();
  let { activeTab = "launch" } = $props<{ activeTab?: AppTab }>();

  onMount(() => {
    restoreLaunchForm();
    void refresh();
    const interval = window.setInterval(() => void refresh(), 1500);
    return () => window.clearInterval(interval);
  });

  function runAction(action: () => Promise<void>, recover?: () => void) {
    void action().catch(() => {
      // `call` already surfaced the error in the UI; keep event handlers settled.
      recover?.();
    });
  }

  function toggleChecked(input: HTMLInputElement, currentValue: boolean, action: (checked: boolean) => Promise<void>) {
    const checked = input.checked;
    runAction(() => action(checked), () => {
      input.checked = currentValue;
    });
  }

  async function call<T>(action: () => Promise<T>) {
    pendingActions += 1;
    error = "";
    refreshError = "";
    notice = "";
    try {
      return await action();
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
      throw err;
    } finally {
      pendingActions = Math.max(0, pendingActions - 1);
    }
  }

  async function refresh() {
    if (pendingActions > 0) {
      return;
    }
    const requestId = ++snapshotRequestId;
    try {
      const nextSnapshot = await invoke<AppSnapshot>("get_snapshot");
      if (requestId === snapshotRequestId) {
        snapshot = nextSnapshot;
        syncLaunchFormFromSnapshot(nextSnapshot);
        scrollStreamIfNeeded();
        refreshError = "";
      }
    } catch (err) {
      if (requestId === snapshotRequestId) {
        refreshError = err instanceof Error ? err.message : String(err);
      }
    }
  }

  async function updateSnapshot(action: () => Promise<AppSnapshot>) {
    const requestId = ++snapshotRequestId;
    const nextSnapshot = await call(action);
    if (requestId === snapshotRequestId) {
      snapshot = nextSnapshot;
      syncLaunchFormFromSnapshot(nextSnapshot);
      scrollStreamIfNeeded();
    }
  }

  async function start() {
    if (patchlineError) {
      error = patchlineError;
      return;
    }
    persistLaunchForm();
    const runningRiotProcesses = await call(() => invoke<string[]>("running_riot_processes"));
    if (runningRiotProcesses.length) {
      pendingRiotProcesses = runningRiotProcesses;
      riotRestartDialogOpen = true;
      return;
    }
    await startProxy();
  }

  async function startProxy() {
    await updateSnapshot(() =>
      invoke<AppSnapshot>("start_deceive", {
        game: selectedGame,
        gamePatchline: gamePatchline.trim(),
        riotClientParams: riotClientParams.trim() || null,
        gameParams: gameParams.trim() || null,
        launchGame
      })
    );
  }

  async function confirmRiotRestart() {
    riotRestartDialogOpen = false;
    pendingRiotProcesses = [];
    await updateSnapshot(() => invoke<AppSnapshot>("kill_riot_processes"));
    await startProxy();
  }

  function cancelRiotRestart() {
    riotRestartDialogOpen = false;
    pendingRiotProcesses = [];
    notice = "Start cancelled. Stop Riot Client first or allow Ghosty to restart it.";
  }

  function handleWindowKeydown(event: KeyboardEvent) {
    if (riotRestartDialogOpen && event.key === "Escape" && !busy) {
      cancelRiotRestart();
    }
  }

  async function stop() {
    await updateSnapshot(() => invoke<AppSnapshot>("stop_deceive"));
  }

  async function cleanRestart() {
    if (patchlineError) {
      error = patchlineError;
      return;
    }
    persistLaunchForm();
    await updateSnapshot(() =>
      invoke<AppSnapshot>("clean_restart", {
        game: selectedGame,
        gamePatchline: gamePatchline.trim(),
        riotClientParams: riotClientParams.trim() || null,
        gameParams: gameParams.trim() || null,
        launchGame
      })
    );
  }

  async function runPreflight() {
    const report = await call(() => invoke<PreflightReport>("run_preflight"));
    preflightReport = report;
    if (report.ok) {
      notice = "Preflight checks passed.";
    } else {
      const failed = report.checks.filter((check) => !check.ok).map((check) => check.label).join(", ");
      error = `Preflight failed: ${failed}`;
    }
  }

  async function setStatus(status: PresenceStatus) {
    await updateSnapshot(() => invoke<AppSnapshot>("set_presence_status", { status }));
  }

  async function setStartupStatus(startupStatus: StartupStatus) {
    await updateSnapshot(() => invoke<AppSnapshot>("set_startup_status", { startupStatus }));
  }

  async function setEnabled(enabled: boolean) {
    await updateSnapshot(() => invoke<AppSnapshot>("set_enabled", { enabled }));
  }

  async function setSafeMode(safeMode: boolean) {
    await updateSnapshot(() => invoke<AppSnapshot>("set_safe_mode", { safeMode }));
  }

  async function setHelperFriend(helperFriend: boolean) {
    await updateSnapshot(() => invoke<AppSnapshot>("set_helper_friend", { helperFriend }));
  }

  async function setAutoAccept(autoAccept: boolean) {
    await updateSnapshot(() => invoke<AppSnapshot>("set_auto_accept", { autoAccept }));
  }

  async function setAutoAcceptDelayMs(delayMs: number) {
    await updateSnapshot(() => invoke<AppSnapshot>("set_auto_accept_delay_ms", { delayMs }));
  }

  async function setDiscordWebhookUrl(url: string) {
    await updateSnapshot(() => invoke<AppSnapshot>("set_discord_webhook_url", { url }));
  }

  async function setConnectToMuc(connectToMuc: boolean) {
    await updateSnapshot(() => invoke<AppSnapshot>("set_connect_to_muc", { connectToMuc }));
  }

  async function locate() {
    const requestId = ++snapshotRequestId;
    const path = await call(() => invoke<string | null>("locate_riot_client"));
    if (requestId === snapshotRequestId) {
      snapshot.riotClientPath = path;
    }
  }

  async function killRiot() {
    await updateSnapshot(() => invoke<AppSnapshot>("kill_riot_processes"));
  }

  async function copyLogs() {
    const text = snapshot.logs
      .map((line) => `[${line.timestamp}] ${line.level.toUpperCase()} ${line.category}: ${line.message}`)
      .join("\n");
    await call(async () => {
      await navigator.clipboard.writeText(text);
      notice = snapshot.logs.length ? "Copied logs to clipboard." : "Copied empty log buffer.";
    });
  }

  async function copyStreamEvents() {
    const text = snapshot.streamEvents
      .map((event) => `[${event.timestamp}] ${event.direction} ${event.bytes} bytes: ${event.preview}`)
      .join("\n");
    await call(async () => {
      await navigator.clipboard.writeText(text);
      notice = snapshot.streamEvents.length ? "Copied event stream to clipboard." : "Copied empty event stream.";
    });
  }

  async function callLcuApi() {
    let body: unknown | null;
    try {
      body = parseLcuBody();
    } catch (err) {
      error = err instanceof Error ? err.message : String(err);
      throw err;
    }
    const response = await call(() =>
      invoke<LcuApiResponse>("call_lcu_api", {
        method: lcuMethod,
        endpoint: lcuEndpoint,
        body
      })
    );
    if (response) {
      lcuResponse = response;
      notice = response.ok ? `League Client API returned ${response.status}.` : `League Client API returned HTTP ${response.status}.`;
    }
  }

  function minimizeWindow() {
    void appWindow.minimize();
  }

  function toggleMaximizeWindow() {
    void appWindow.toggleMaximize();
  }

  function closeWindow() {
    void appWindow.close();
  }

  function emptyHealth(): ConnectionHealth {
    const step = (key: string, label: string): HealthStep => ({
      key,
      label,
      state: "waiting",
      detail: "Waiting"
    });
    return {
      configProxy: step("configProxy", "Config proxy"),
      configPatched: step("configPatched", "Config patched"),
      chatServer: step("chatServer", "Chat server"),
      tlsConnected: step("tlsConnected", "TLS connected"),
      xmppActive: step("xmppActive", "XMPP active"),
      activeConnections: 0,
      reconnectAttempts: 0
    };
  }

  function healthSteps(health: ConnectionHealth) {
    const steps = [
      health.configProxy,
      health.configPatched,
      health.chatServer,
      health.tlsConnected,
      health.xmppActive
    ];
    if (health.activeConnections > 0) {
      return steps;
    }
    return steps.map((step) => {
      if (step.state !== "active" || (step.key !== "tlsConnected" && step.key !== "xmppActive")) {
        return step;
      }
      return {
        ...step,
        state: "ready" as HealthState,
        detail: step.key === "tlsConnected" ? "Waiting for Riot Client connection" : "Waiting for Riot chat traffic"
      };
    });
  }

  function filteredLogs() {
    const logs = logFilter === "all" ? snapshot.logs : snapshot.logs.filter((line) => line.category === logFilter);
    return logs.slice().reverse();
  }

  function streamEvents() {
    return snapshot.streamEvents.slice().reverse();
  }

  function toggleStreamAutoScroll() {
    streamAutoScroll = !streamAutoScroll;
    if (streamAutoScroll) {
      scrollStreamToLatest();
    }
  }

  function scrollStreamIfNeeded() {
    const nextCount = snapshot.streamEvents.length;
    const hasNewEvents = nextCount !== lastStreamEventCount;
    lastStreamEventCount = nextCount;
    if (streamAutoScroll && hasNewEvents) {
      window.requestAnimationFrame(scrollStreamToLatest);
    }
  }

  function scrollStreamToLatest() {
    if (streamLogElement) {
      streamLogElement.scrollTop = 0;
    }
  }

  function presenceLabel(status: PresenceStatus) {
    return statuses.find((item) => item.id === status)?.label ?? status;
  }

  function leagueClientStateLabel() {
    return snapshot.autoAcceptState.replace(/^Watching:\s*/, "");
  }

  function autoAcceptDelayLabel(delayMs: number) {
    return `${(delayMs / 1000).toFixed(delayMs % 1000 === 0 ? 0 : 1)}s`;
  }

  function parseDelayInput(value: string) {
    const parsed = Number.parseInt(value, 10);
    if (Number.isNaN(parsed)) {
      return snapshot.autoAcceptDelayMs;
    }
    return Math.max(0, Math.min(10000, parsed));
  }

  function lcuResponseText() {
    if (!lcuResponse) {
      return "";
    }
    if (lcuResponse.body !== null) {
      return JSON.stringify(lcuResponse.body, null, 2);
    }
    return lcuResponse.text;
  }

  function parseLcuBody() {
    if (lcuMethod === "GET" || lcuMethod === "DELETE" || !lcuBody.trim()) {
      return null;
    }
    try {
      return JSON.parse(lcuBody);
    } catch (err) {
      throw new Error(`Body must be valid JSON: ${err instanceof Error ? err.message : String(err)}`);
    }
  }

  function selectGame(game: LaunchGame) {
    selectedGame = game;
    persistLaunchForm();
  }

  function updateGamePatchline(value: string) {
    gamePatchline = value;
    persistLaunchForm();
  }

  function validatePatchline(value: string, game: LaunchGame) {
    if (game === "riotClient") {
      return "";
    }
    const patchline = value.trim();
    if (!patchline) {
      return "Patchline is required.";
    }
    if (/\s/.test(patchline)) {
      return "Patchline cannot contain spaces.";
    }
    return "";
  }

  function updateRiotClientParams(value: string) {
    riotClientParams = value;
    persistLaunchForm();
  }

  function updateGameParams(value: string) {
    gameParams = value;
    persistLaunchForm();
  }

  function updateLaunchGame(value: boolean) {
    launchGame = value;
    persistLaunchForm();
  }

  function restoreLaunchForm() {
    const savedGame = readLaunchGame(readStoredLaunchValue("selectedGame"));
    if (savedGame) {
      selectedGame = savedGame;
    }
    gamePatchline = readStoredLaunchValue("gamePatchline") ?? gamePatchline;
    riotClientParams = readStoredLaunchValue("riotClientParams") ?? riotClientParams;
    gameParams = readStoredLaunchValue("gameParams") ?? gameParams;
    launchGame = readStoredLaunchValue("launchGame") !== "false";
  }

  function syncLaunchFormFromSnapshot(nextSnapshot: AppSnapshot) {
    if (launchFormInitialized) {
      return;
    }
    launchFormInitialized = true;
    if (nextSnapshot.activeGame) {
      selectedGame = nextSnapshot.activeGame;
    }
  }

  function persistLaunchForm() {
    writeStoredLaunchValue("selectedGame", selectedGame);
    writeStoredLaunchValue("gamePatchline", gamePatchline);
    writeStoredLaunchValue("riotClientParams", riotClientParams);
    writeStoredLaunchValue("gameParams", gameParams);
    writeStoredLaunchValue("launchGame", String(launchGame));
  }

  function readLaunchGame(value: string | null): LaunchGame | null {
    return games.some((game) => game.id === value) ? (value as LaunchGame) : null;
  }

  function readStoredLaunchValue(key: string) {
    try {
      return localStorage.getItem(`ghosty.launch.${key}`);
    } catch {
      return null;
    }
  }

  function writeStoredLaunchValue(key: string, value: string) {
    try {
      localStorage.setItem(`ghosty.launch.${key}`, value);
    } catch {
      // Launching should not depend on browser storage being writable.
    }
  }
</script>

<svelte:head>
  <title>Ghosty</title>
</svelte:head>

<svelte:window onkeydown={handleWindowKeydown} />

<div class="titlebar">
  <div class="drag-region" data-tauri-drag-region role="presentation" ondblclick={toggleMaximizeWindow}>
    <img class="brand-mark" data-tauri-drag-region src="/icon.png" alt="" />
    <div class="title-copy" data-tauri-drag-region>
      <strong data-tauri-drag-region>Ghosty</strong>
    </div>
    <div class="title-status" data-tauri-drag-region>
      <Badge class={`title-status-chip ${snapshot.running ? "online" : ""}`} variant="outline" data-tauri-drag-region>
        <Activity size={12} />
        Proxy; {snapshot.running ? "Running" : "Stopped"}
      </Badge>
      <Badge class="title-status-chip presence" variant="outline" data-status={snapshot.status} data-tauri-drag-region>
        <Shield size={12} />
        League; {presenceLabel(snapshot.status)}
      </Badge>
      <Badge class="title-status-chip client-state" variant="outline" data-tauri-drag-region>
        <ListChecks size={12} />
        Client State; {leagueClientStateLabel()}
      </Badge>
    </div>
  </div>
  <div class="window-controls">
    <Button class="window-button" variant="ghost" size="icon-sm" title="Minimize" onclick={minimizeWindow}>
      <Minus size={15} />
    </Button>
    <Button class="window-button" variant="ghost" size="icon-sm" title="Maximize" onclick={toggleMaximizeWindow}>
      <Maximize2 size={14} />
    </Button>
    <Button class="window-button close" variant="ghost" size="icon-sm" title="Close" onclick={closeWindow}>
      <X size={16} />
    </Button>
  </div>
</div>

<main class="shell">
  <header>
    <div>
      <h1>Ghosty</h1>
      <p>Riot presence masking for League, VALORANT, Runeterra, and 2XKO.</p>
    </div>
    <div class="header-status">
      <Badge class={`state-pill ${snapshot.running ? "online" : ""}`} variant="outline">
        <Activity size={16} />
        Proxy; {snapshot.running ? "Running" : "Stopped"}
      </Badge>
      <Badge class="state-pill client" variant="outline">
        <ListChecks size={16} />
        Client State; {leagueClientStateLabel()}
      </Badge>
    </div>
  </header>

  {#if error}
    <section class="error">{error}</section>
  {:else if refreshError}
    <section class="error">{refreshError}</section>
  {/if}

  {#if notice}
    <section class="notice">{notice}</section>
  {/if}

  {#if riotRestartDialogOpen}
    <div class="dialog-overlay">
      <div class="dialog-content" role="dialog" aria-modal="true" aria-labelledby="riot-restart-title" aria-describedby="riot-restart-description">
        <div class="dialog-header">
          <div class="dialog-icon">
            <Trash2 size={20} />
          </div>
          <div>
            <h2 id="riot-restart-title">Restart Riot Client?</h2>
            <p id="riot-restart-description">
              Ghosty needs to launch Riot Client itself so chat routes through the local proxy.
            </p>
          </div>
        </div>
        <div class="dialog-body">
          <p>These Riot/League processes are already running:</p>
          <div class="process-list">
            {#each pendingRiotProcesses as process}
              <span>{process}</span>
            {/each}
          </div>
        </div>
        <div class="dialog-footer">
          <Button disabled={busy} variant="outline" onclick={cancelRiotRestart}>Cancel</Button>
          <Button class="danger" disabled={busy} variant="destructive" onclick={() => runAction(confirmRiotRestart)}>
            <Trash2 size={16} /> Stop Riot and Start
          </Button>
        </div>
      </div>
    </div>
  {/if}

  <nav class="tabs" aria-label="Ghosty sections">
    <Button class={activeTab === "launch" ? "active" : ""} href="/" variant="ghost" size="sm">
      <Gamepad2 size={16} /> Launch
    </Button>
    <Button class={activeTab === "presence" ? "active" : ""} href="/presence" variant="ghost" size="sm">
      <Shield size={16} /> Presence
    </Button>
    <Button class={activeTab === "utility" ? "active" : ""} href="/utility" variant="ghost" size="sm">
      <ListChecks size={16} /> Utility
    </Button>
    <Button class={activeTab === "debug" ? "active" : ""} href="/debug" variant="ghost" size="sm">
      <HeartPulse size={16} /> Debug
    </Button>
  </nav>

  {#if activeTab === "debug"}
  <section class="status-grid">
    <div class="panel health-panel">
      <div class="panel-title compact">
        <HeartPulse size={18} />
        <h2>Connection Health</h2>
      </div>
      <div class="health-steps">
        {#each healthSteps(snapshot.health) as step}
          <div class="health-step" data-state={step.state}>
            <span class="health-dot"></span>
            <div>
              <strong>{step.label}</strong>
              <span>{step.detail}</span>
            </div>
          </div>
        {/each}
      </div>
      <div class="metrics">
        <span>{snapshot.health.activeConnections} active connection{snapshot.health.activeConnections === 1 ? "" : "s"}</span>
        <span>{snapshot.health.reconnectAttempts} reconnect attempt{snapshot.health.reconnectAttempts === 1 ? "" : "s"}</span>
      </div>
    </div>

    <div class="panel preflight-panel">
      <div class="panel-title compact">
        <ListChecks size={18} />
        <h2>Preflight</h2>
      </div>
      <Button class="wide-action" disabled={busy} variant="outline" onclick={() => runAction(runPreflight)}>
        {#if busy}<Spinner />{/if} Run Checks
      </Button>
      {#if preflightReport}
        <div class="checks" class:ok={preflightReport.ok}>
          {#each preflightReport.checks as check}
            <div class:ok={check.ok} class="check-row">
              <strong>{check.label}</strong>
              <span>{check.detail}</span>
            </div>
          {/each}
        </div>
      {/if}
    </div>

    <div class="panel hotkey-panel">
      <div class="panel-title compact">
        <Keyboard size={18} />
        <h2>Quick Actions</h2>
      </div>
      <div class="hotkeys">
        <span><Kbd>Ctrl</Kbd><Kbd>Alt</Kbd><Kbd>O</Kbd> Offline</span>
        <span><Kbd>Ctrl</Kbd><Kbd>Alt</Kbd><Kbd>M</Kbd> Mobile</span>
        <span><Kbd>Ctrl</Kbd><Kbd>Alt</Kbd><Kbd>N</Kbd> Online</span>
      </div>
      <p class="fine-print">Tray menu includes show, status changes, masking toggle, and quit.</p>
    </div>
  </section>
  {/if}

  <section class="grid" class:single={activeTab !== "debug"}>
    {#if activeTab === "launch"}
    <div class="panel launch-panel">
      <div class="panel-title">
        <Gamepad2 size={18} />
        <h2>Launch</h2>
      </div>

      <div class="game-grid">
        {#each games as game}
          <Button class={`game-tile ${selectedGame === game.id ? "selected" : ""}`} variant="outline" onclick={() => selectGame(game.id)}>
            <strong>{game.label}</strong>
            <span>{game.hint}</span>
          </Button>
        {/each}
      </div>

      <label class="field">
        <span>Patchline</span>
        <Input aria-invalid={patchlineError ? "true" : "false"} value={gamePatchline} oninput={(event) => updateGamePatchline(event.currentTarget.value)} />
        {#if patchlineError}
          <small class="field-error">{patchlineError}</small>
        {/if}
      </label>

      <label class="field">
        <span>Riot Client Params</span>
        <Input value={riotClientParams} oninput={(event) => updateRiotClientParams(event.currentTarget.value)} placeholder="--allow-multiple-clients" />
      </label>

      <label class="field">
        <span>Game Params</span>
        <Input value={gameParams} oninput={(event) => updateGameParams(event.currentTarget.value)} placeholder="optional arguments after --" />
      </label>

      <label class="switch">
        <input type="checkbox" checked={launchGame} onchange={(event) => updateLaunchGame(event.currentTarget.checked)} />
        <span>Launch Riot Client after starting proxy</span>
      </label>

      <div class="button-row">
        {#if snapshot.running}
          <Button class="primary danger" disabled={busy} variant="destructive" onclick={() => runAction(stop)}>
            <Square size={17} /> Stop
          </Button>
        {:else}
          <Button class="primary" disabled={launchBlocked} onclick={() => runAction(start)}>
            {#if busy}<Spinner />{:else}<Power size={17} />{/if} Start
          </Button>
        {/if}
        <Button disabled={busy} variant="outline" onclick={() => runAction(locate)} title="Locate Riot Client">
          <Search size={17} /> Locate
        </Button>
        <Button disabled={busy} variant="outline" onclick={() => runAction(killRiot)} title="Stop Riot processes">
          <Trash2 size={17} /> Kill Riot
        </Button>
        <Button disabled={launchBlocked} variant="outline" onclick={() => runAction(cleanRestart)} title="Stop Riot, restart proxy, launch selected game">
          <RefreshCcw size={17} /> Clean Restart
        </Button>
      </div>
    </div>
    {/if}

    {#if activeTab === "presence"}
    <div class="panel">
      <div class="panel-title">
        <Shield size={18} />
        <h2>Presence</h2>
      </div>

      <div class="segmented">
        {#each statuses as status}
          <Button class={snapshot.status === status.id ? "active" : ""} disabled={busy} variant="outline" onclick={() => runAction(() => setStatus(status.id))}>
            {status.label}
          </Button>
        {/each}
      </div>

      <label class="switch">
        <input checked={snapshot.enabled} disabled={busy} type="checkbox" onchange={(event) => toggleChecked(event.currentTarget, snapshot.enabled, setEnabled)} />
        <span>Mask outgoing presence</span>
      </label>

      <label class="switch">
        <input checked={snapshot.safeMode} disabled={busy} type="checkbox" onchange={(event) => toggleChecked(event.currentTarget, snapshot.safeMode, setSafeMode)} />
        <span>Safe mode</span>
      </label>

      <label class="switch">
        <input checked={snapshot.helperFriend} disabled={busy} type="checkbox" onchange={(event) => toggleChecked(event.currentTarget, snapshot.helperFriend, setHelperFriend)} />
        <span>Helper friend</span>
      </label>

      <label class="switch">
        <input checked={snapshot.connectToMuc} disabled={busy} type="checkbox" onchange={(event) => toggleChecked(event.currentTarget, snapshot.connectToMuc, setConnectToMuc)} />
        <span>Allow lobby and select chat</span>
      </label>

      <div class="subhead">Startup Status</div>
      <div class="startup-grid">
        {#each startupStatuses as startup}
          <Button class={snapshot.startupStatus === startup.id ? "active" : ""} disabled={busy} variant="outline" onclick={() => runAction(() => setStartupStatus(startup.id))}>
            {startup.label}
          </Button>
        {/each}
      </div>
    </div>
    {/if}

    {#if activeTab === "utility"}
    <div class="panel auto-accept-panel">
      <div class="panel-title">
        <ListChecks size={18} />
        <h2>Utility</h2>
      </div>

      <label class="switch">
        <input checked={snapshot.autoAccept} disabled={busy} type="checkbox" onchange={(event) => toggleChecked(event.currentTarget, snapshot.autoAccept, setAutoAccept)} />
        <span>Auto accept match found</span>
      </label>

      <div class="auto-accept-settings">
        <div class="setting-heading">
          <strong>Accept Delay</strong>
          <span>{autoAcceptDelayLabel(snapshot.autoAcceptDelayMs)}</span>
        </div>
        <input
          aria-label="Auto accept delay"
          disabled={busy}
          max="10000"
          min="0"
          step="250"
          type="range"
          value={snapshot.autoAcceptDelayMs}
          onchange={(event) => runAction(() => setAutoAcceptDelayMs(parseDelayInput(event.currentTarget.value)))}
        />
        <label class="compact-field">
          <span>Milliseconds</span>
          <Input
            disabled={busy}
            max="10000"
            min="0"
            step="250"
            type="number"
            value={snapshot.autoAcceptDelayMs}
            onchange={(event) => runAction(() => setAutoAcceptDelayMs(parseDelayInput(event.currentTarget.value)))}
          />
        </label>
        <div class="setting-status">{snapshot.autoAcceptState}</div>
      </div>

      <label class="field">
        <span>Discord Webhook</span>
        <Input
          disabled={busy}
          placeholder="https://discord.com/api/webhooks/..."
          type="url"
          value={snapshot.discordWebhookUrl}
          onchange={(event) => runAction(() => setDiscordWebhookUrl(event.currentTarget.value))}
        />
      </label>
    </div>
    {/if}

    {#if activeTab === "debug"}
    <div class="panel">
      <div class="panel-title">
        <RefreshCcw size={18} />
        <h2>Proxy</h2>
      </div>
      <dl>
        <div>
          <dt>Config URL</dt>
          <dd>{snapshot.configPort ? `http://127.0.0.1:${snapshot.configPort}` : "Not running"}</dd>
        </div>
        <div>
          <dt>Local Chat</dt>
          <dd>{snapshot.chatPort ? `127.0.0.1:${snapshot.chatPort}` : "Not running"}</dd>
        </div>
        <div>
          <dt>Riot Chat</dt>
          <dd>{snapshot.riotChatHost ? `${snapshot.riotChatHost}:${snapshot.riotChatPort}` : "Waiting for client config"}</dd>
        </div>
        <div>
          <dt>Riot Client</dt>
          <dd>{snapshot.riotClientPath ?? "Not found yet"}</dd>
        </div>
      </dl>
    </div>

    <div class="panel lcu-api-panel">
      <div class="panel-title">
        <Activity size={18} />
        <h2>League Client API</h2>
      </div>
      <label class="field">
        <span>Endpoint</span>
        <select value={lcuEndpoint} onchange={(event) => (lcuEndpoint = event.currentTarget.value)}>
          {#each lcuEndpoints as endpoint}
            <option value={endpoint.path}>{endpoint.label}</option>
          {/each}
        </select>
      </label>
      <label class="field">
        <span>Method</span>
        <select value={lcuMethod} onchange={(event) => (lcuMethod = event.currentTarget.value)}>
          {#each lcuMethods as method}
            <option value={method}>{method}</option>
          {/each}
        </select>
      </label>
      <label class="field">
        <span>Path</span>
        <Input value={lcuEndpoint} oninput={(event) => (lcuEndpoint = event.currentTarget.value)} placeholder="/lol-summoner/v1/current-summoner" />
      </label>
      {#if lcuMethod !== "GET" && lcuMethod !== "DELETE"}
        <label class="field">
          <span>JSON Body</span>
          <textarea value={lcuBody} oninput={(event) => (lcuBody = event.currentTarget.value)} placeholder={`{ "key": "value" }`}></textarea>
        </label>
      {/if}
      <div class="button-row">
        <Button class="primary" disabled={busy} onclick={() => runAction(callLcuApi)}>
          {#if busy}<Spinner />{:else}<Search size={17} />{/if} Call Endpoint
        </Button>
      </div>
      {#if lcuResponse}
        <div class="api-result">
          <div class="api-result-meta">
            <Badge class={lcuResponse.ok ? "ok" : ""} variant="outline">{lcuResponse.status}</Badge>
            <strong>{lcuResponse.method} {lcuResponse.endpoint}</strong>
          </div>
          <pre>{lcuResponseText()}</pre>
        </div>
      {/if}
    </div>

    <div class="panel log-panel">
      <div class="panel-title">
        <MessageSquare size={18} />
        <h2>Log</h2>
        <Button class="icon-action" disabled={busy} variant="ghost" size="icon-sm" title="Copy logs" onclick={() => runAction(copyLogs)}>
          <Clipboard size={16} />
        </Button>
      </div>
      <div class="log-filters">
        {#each ["all", "config", "chat", "launch", "error", "system"] as filter}
          <Button class={logFilter === filter ? "active" : ""} variant="outline" size="sm" onclick={() => (logFilter = filter as "all" | LogCategory)}>
            {filter}
          </Button>
        {/each}
      </div>
      <ScrollArea class="log">
        {#if filteredLogs().length}
          {#each filteredLogs() as line}
            <p data-category={line.category} data-level={line.level}>
              <span>{line.timestamp}</span>
              <b>{line.category}</b>
              {line.message}
            </p>
          {/each}
        {:else}
          <p>No proxy events yet.</p>
        {/if}
      </ScrollArea>
    </div>

    <div class="panel log-panel event-stream-panel">
      <div class="panel-title">
        <Activity size={18} />
        <h2>Event Stream</h2>
        <label class="auto-scroll-toggle" title="Keep event stream pinned to newest entries">
          <input type="checkbox" checked={streamAutoScroll} onchange={toggleStreamAutoScroll} />
          <span>Auto Scroll</span>
        </label>
        <Button class="icon-action" disabled={busy} variant="ghost" size="icon-sm" title="Copy event stream" onclick={() => runAction(copyStreamEvents)}>
          <Clipboard size={16} />
        </Button>
      </div>
      <ScrollArea class="log stream-log" bind:viewportRef={streamLogElement}>
        {#if streamEvents().length}
          {#each streamEvents() as event}
            <p>
              <span>{event.timestamp}</span>
              <b>{event.direction}</b>
              <em>{event.bytes} bytes</em>
              {event.preview}
            </p>
          {/each}
        {:else}
          <p>No stream events yet.</p>
        {/if}
      </ScrollArea>
    </div>
    {/if}
  </section>
</main>

