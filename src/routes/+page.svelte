<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { onMount } from "svelte";
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
  type AppTab = "launch" | "presence" | "debug";
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

  type PreflightReport = {
    ok: boolean;
    checks: Array<{ label: string; ok: boolean; detail: string }>;
  };

  type AppSnapshot = {
    running: boolean;
    enabled: boolean;
    safeMode: boolean;
    helperFriend: boolean;
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

  let snapshot = $state<AppSnapshot>({
    running: false,
    enabled: true,
    safeMode: false,
    helperFriend: false,
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
  let activeTab = $state<AppTab>("launch");
  let logFilter = $state<"all" | LogCategory>("all");
  let preflightReport = $state<PreflightReport | null>(null);
  let snapshotRequestId = 0;
  let launchFormInitialized = false;
  let busy = $derived(pendingActions > 0);
  let patchlineError = $derived(validatePatchline(gamePatchline, selectedGame));
  let launchBlocked = $derived(busy || patchlineError !== "");
  const appWindow = getCurrentWindow();

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
    }
  }

  async function start() {
    if (patchlineError) {
      error = patchlineError;
      return;
    }
    persistLaunchForm();
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

<div class="titlebar">
  <div class="drag-region" data-tauri-drag-region role="presentation" ondblclick={toggleMaximizeWindow}>
    <img class="brand-mark" data-tauri-drag-region src="/icon.png" alt="" />
    <div class="title-copy" data-tauri-drag-region>
      <strong data-tauri-drag-region>Ghosty</strong>
      <span data-tauri-drag-region>{snapshot.running ? "Masking console active" : "Masking console idle"}</span>
    </div>
  </div>
  <div class="window-controls">
    <button class="window-button" type="button" title="Minimize" onclick={minimizeWindow}>
      <Minus size={15} />
    </button>
    <button class="window-button" type="button" title="Maximize" onclick={toggleMaximizeWindow}>
      <Maximize2 size={14} />
    </button>
    <button class="window-button close" type="button" title="Close" onclick={closeWindow}>
      <X size={16} />
    </button>
  </div>
</div>

<main class="shell">
  <header>
    <div>
      <h1>Ghosty</h1>
      <p>Riot presence masking for League, VALORANT, Runeterra, and 2XKO.</p>
    </div>
    <div class:online={snapshot.running} class="state-pill">
      <Activity size={16} />
      {snapshot.running ? "Proxy Running" : "Proxy Stopped"}
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

  <nav class="tabs" aria-label="Ghosty sections">
    <button class:active={activeTab === "launch"} type="button" onclick={() => (activeTab = "launch")}>
      <Gamepad2 size={16} /> Launch
    </button>
    <button class:active={activeTab === "presence"} type="button" onclick={() => (activeTab = "presence")}>
      <Shield size={16} /> Presence
    </button>
    <button class:active={activeTab === "debug"} type="button" onclick={() => (activeTab = "debug")}>
      <HeartPulse size={16} /> Debug
    </button>
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
      <button class="wide-action" disabled={busy} type="button" onclick={() => runAction(runPreflight)}>
        Run Checks
      </button>
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
        <span>Ctrl+Alt+O Offline</span>
        <span>Ctrl+Alt+M Mobile</span>
        <span>Ctrl+Alt+N Online</span>
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
          <button class:selected={selectedGame === game.id} class="game-tile" type="button" onclick={() => selectGame(game.id)}>
            <strong>{game.label}</strong>
            <span>{game.hint}</span>
          </button>
        {/each}
      </div>

      <label class="field">
        <span>Patchline</span>
        <input aria-invalid={patchlineError ? "true" : "false"} value={gamePatchline} oninput={(event) => updateGamePatchline(event.currentTarget.value)} />
        {#if patchlineError}
          <small class="field-error">{patchlineError}</small>
        {/if}
      </label>

      <label class="field">
        <span>Riot Client Params</span>
        <input value={riotClientParams} oninput={(event) => updateRiotClientParams(event.currentTarget.value)} placeholder="--allow-multiple-clients" />
      </label>

      <label class="field">
        <span>Game Params</span>
        <input value={gameParams} oninput={(event) => updateGameParams(event.currentTarget.value)} placeholder="optional arguments after --" />
      </label>

      <label class="switch">
        <input type="checkbox" checked={launchGame} onchange={(event) => updateLaunchGame(event.currentTarget.checked)} />
        <span>Launch Riot Client after starting proxy</span>
      </label>

      <div class="button-row">
        {#if snapshot.running}
          <button class="primary danger" disabled={busy} type="button" onclick={() => runAction(stop)}>
            <Square size={17} /> Stop
          </button>
        {:else}
          <button class="primary" disabled={launchBlocked} type="button" onclick={() => runAction(start)}>
            <Power size={17} /> Start
          </button>
        {/if}
        <button disabled={busy} type="button" onclick={() => runAction(locate)} title="Locate Riot Client">
          <Search size={17} /> Locate
        </button>
        <button disabled={busy} type="button" onclick={() => runAction(killRiot)} title="Stop Riot processes">
          <Trash2 size={17} /> Kill Riot
        </button>
        <button disabled={launchBlocked} type="button" onclick={() => runAction(cleanRestart)} title="Stop Riot, restart proxy, launch selected game">
          <RefreshCcw size={17} /> Clean Restart
        </button>
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
          <button class:active={snapshot.status === status.id} disabled={busy} type="button" onclick={() => runAction(() => setStatus(status.id))}>
            {status.label}
          </button>
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
          <button class:active={snapshot.startupStatus === startup.id} disabled={busy} type="button" onclick={() => runAction(() => setStartupStatus(startup.id))}>
            {startup.label}
          </button>
        {/each}
      </div>
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

    <div class="panel log-panel">
      <div class="panel-title">
        <MessageSquare size={18} />
        <h2>Log</h2>
        <button class="icon-action" disabled={busy} type="button" title="Copy logs" onclick={() => runAction(copyLogs)}>
          <Clipboard size={16} />
        </button>
      </div>
      <div class="log-filters">
        {#each ["all", "config", "chat", "launch", "error", "system"] as filter}
          <button class:active={logFilter === filter} type="button" onclick={() => (logFilter = filter as "all" | LogCategory)}>
            {filter}
          </button>
        {/each}
      </div>
      <div class="log">
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
      </div>
    </div>

    <div class="panel log-panel event-stream-panel">
      <div class="panel-title">
        <Activity size={18} />
        <h2>Event Stream</h2>
        <button class="icon-action" disabled={busy} type="button" title="Copy event stream" onclick={() => runAction(copyStreamEvents)}>
          <Clipboard size={16} />
        </button>
      </div>
      <div class="log stream-log">
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
      </div>
    </div>
    {/if}
  </section>
</main>

<style>
  :global(*) {
    box-sizing: border-box;
  }

  :global(body) {
    margin: 0;
    min-width: 360px;
    min-height: 100vh;
    color: #1c2430;
    background: #f2f5f7;
    scrollbar-color: #93a2b1 #e2e7ec;
    scrollbar-width: thin;
    font-family:
      Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
  }

  :global(::-webkit-scrollbar) {
    width: 12px;
    height: 12px;
  }

  :global(::-webkit-scrollbar-track) {
    background: #e2e7ec;
  }

  :global(::-webkit-scrollbar-thumb) {
    min-height: 42px;
    border: 3px solid #e2e7ec;
    border-radius: 999px;
    background: #93a2b1;
  }

  :global(::-webkit-scrollbar-thumb:hover) {
    background: #6f7f90;
  }

  :global(::-webkit-scrollbar-corner) {
    background: #e2e7ec;
  }

  button,
  input {
    font: inherit;
  }

  .titlebar {
    position: fixed;
    z-index: 20;
    top: 0;
    left: 0;
    right: 0;
    height: 44px;
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    border-bottom: 1px solid #d7dde3;
    background: rgba(255, 255, 255, 0.96);
    user-select: none;
  }

  .drag-region {
    display: flex;
    align-items: center;
    gap: 10px;
    min-width: 0;
    padding: 0 12px;
  }

  .brand-mark {
    width: 24px;
    height: 24px;
    border-radius: 7px;
    object-fit: cover;
  }

  .title-copy {
    display: flex;
    align-items: baseline;
    gap: 9px;
    min-width: 0;
  }

  .title-copy strong {
    color: #172232;
    font-size: 13px;
    letter-spacing: 0;
  }

  .title-copy span {
    min-width: 0;
    overflow: hidden;
    color: #657382;
    font-size: 12px;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .window-controls {
    display: flex;
    align-items: stretch;
  }

  .window-button {
    display: grid;
    place-items: center;
    width: 44px;
    min-height: 0;
    height: 44px;
    padding: 0;
    border: 0;
    border-radius: 0;
    background: transparent;
    color: #344252;
  }

  .window-button:hover {
    background: #e8eef4;
  }

  .window-button.close:hover {
    background: #c73744;
    color: #fff;
  }

  .shell {
    width: min(1120px, calc(100vw - 32px));
    margin: 0 auto;
    padding: 68px 0 24px;
  }

  header {
    display: flex;
    align-items: end;
    justify-content: space-between;
    gap: 16px;
    margin-bottom: 18px;
  }

  h1,
  h2,
  p {
    margin: 0;
  }

  h1 {
    font-size: 30px;
    line-height: 1.1;
    letter-spacing: 0;
  }

  header p {
    margin-top: 5px;
    color: #5c6875;
  }

  .state-pill {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    min-height: 34px;
    padding: 0 12px;
    border: 1px solid #c9d1da;
    border-radius: 8px;
    background: #fff;
    color: #6b2630;
    font-weight: 700;
    white-space: nowrap;
  }

  .state-pill.online {
    color: #14633f;
    border-color: #9fd0b7;
  }

  .error {
    margin-bottom: 14px;
    padding: 10px 12px;
    border: 1px solid #e1a4a4;
    border-radius: 8px;
    background: #fff3f3;
    color: #8b1e1e;
  }

  .notice {
    margin-bottom: 14px;
    padding: 10px 12px;
    border: 1px solid #9fd0b7;
    border-radius: 8px;
    background: #effaf4;
    color: #14633f;
  }

  .tabs {
    display: inline-grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: 6px;
    margin-bottom: 14px;
    padding: 5px;
    border: 1px solid #d7dde3;
    border-radius: 8px;
    background: #fff;
  }

  .tabs button {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: 7px;
    min-width: 128px;
    min-height: 34px;
    border-color: transparent;
    background: transparent;
  }

  .tabs button.active {
    border-color: #2364aa;
    background: #e8f1fb;
    color: #123d6d;
  }

  .grid {
    display: grid;
    grid-template-columns: minmax(360px, 1.25fr) minmax(300px, 0.9fr);
    gap: 14px;
  }

  .grid.single {
    grid-template-columns: minmax(360px, 760px);
  }

  .status-grid {
    display: grid;
    grid-template-columns: minmax(360px, 1.4fr) minmax(260px, 0.9fr) minmax(230px, 0.75fr);
    gap: 14px;
    margin-bottom: 14px;
  }

  .panel {
    border: 1px solid #d7dde3;
    border-radius: 8px;
    background: #fff;
    padding: 16px;
    box-shadow: 0 1px 2px rgba(25, 35, 45, 0.06);
  }

  .launch-panel,
  .log-panel {
    grid-row: span 2;
  }

  .panel-title {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 14px;
  }

  .panel-title.compact {
    margin-bottom: 10px;
  }

  .panel-title .icon-action {
    margin-left: auto;
  }

  h2 {
    font-size: 17px;
    line-height: 1.2;
  }

  .game-grid {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: 8px;
    margin-bottom: 14px;
  }

  .game-tile {
    height: 64px;
    text-align: left;
    border: 1px solid #d6dde5;
    border-radius: 8px;
    background: #f8fafb;
    color: #1b2430;
    padding: 9px 10px;
    cursor: pointer;
  }

  .game-tile strong,
  .game-tile span {
    display: block;
  }

  .game-tile span {
    margin-top: 3px;
    color: #697686;
    font-size: 12px;
  }

  .game-tile.selected,
  .segmented button.active,
  .startup-grid button.active {
    border-color: #2364aa;
    background: #e8f1fb;
    color: #123d6d;
  }

  .field {
    display: grid;
    gap: 6px;
    margin-top: 10px;
    color: #4b5968;
    font-size: 13px;
    font-weight: 700;
  }

  .field input {
    width: 100%;
    min-height: 38px;
    border: 1px solid #ccd4dd;
    border-radius: 7px;
    background: #fff;
    color: #1c2430;
    padding: 0 10px;
  }

  .field input[aria-invalid="true"] {
    border-color: #c84d4d;
    background: #fff7f7;
  }

  .field-error {
    color: #8b1e1e;
    font-size: 12px;
    font-weight: 650;
    line-height: 1.3;
  }

  .switch {
    display: flex;
    align-items: center;
    gap: 9px;
    min-height: 34px;
    margin-top: 12px;
    color: #334252;
    font-weight: 650;
  }

  .switch input {
    width: 17px;
    height: 17px;
  }

  .button-row {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    margin-top: 16px;
  }

  .wide-action {
    width: 100%;
  }

  button {
    min-height: 38px;
    border: 1px solid #ccd4dd;
    border-radius: 8px;
    background: #fff;
    color: #263442;
    padding: 0 12px;
    cursor: pointer;
    font-weight: 700;
  }

  button:disabled {
    opacity: 0.62;
    cursor: default;
  }

  .button-row button,
  .primary {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: 7px;
  }

  .primary {
    background: #2364aa;
    border-color: #2364aa;
    color: #fff;
  }

  .primary.danger {
    background: #a8323e;
    border-color: #a8323e;
  }

  .segmented,
  .startup-grid,
  .log-filters {
    display: grid;
    gap: 8px;
  }

  .segmented {
    grid-template-columns: repeat(3, minmax(0, 1fr));
  }

  .startup-grid {
    grid-template-columns: repeat(2, minmax(0, 1fr));
  }

  .log-filters {
    grid-template-columns: repeat(6, minmax(0, 1fr));
    margin-bottom: 8px;
  }

  .log-filters button {
    min-height: 30px;
    padding: 0 7px;
    font-size: 12px;
    text-transform: capitalize;
  }

  .log-filters button.active {
    border-color: #2364aa;
    background: #e8f1fb;
    color: #123d6d;
  }

  .icon-action {
    display: grid;
    place-items: center;
    width: 34px;
    min-height: 34px;
    padding: 0;
  }

  .health-steps {
    display: grid;
    gap: 8px;
  }

  .health-step {
    display: grid;
    grid-template-columns: 10px minmax(0, 1fr);
    align-items: start;
    gap: 9px;
  }

  .health-dot {
    width: 10px;
    height: 10px;
    margin-top: 5px;
    border-radius: 999px;
    background: #a8b3bf;
  }

  .health-step[data-state="ready"] .health-dot {
    background: #2364aa;
  }

  .health-step[data-state="active"] .health-dot,
  .check-row.ok::before {
    background: #23885a;
  }

  .health-step[data-state="warning"] .health-dot {
    background: #b47b1f;
  }

  .health-step[data-state="error"] .health-dot,
  .check-row::before {
    background: #b13845;
  }

  .health-step strong,
  .check-row strong {
    display: block;
    color: #263442;
    font-size: 13px;
    line-height: 1.25;
  }

  .health-step span:not(.health-dot),
  .check-row span,
  .fine-print {
    color: #657382;
    font-size: 12px;
    line-height: 1.35;
  }

  .metrics {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    margin-top: 12px;
  }

  .metrics span,
  .hotkeys span {
    border: 1px solid #d6dde5;
    border-radius: 7px;
    background: #f8fafb;
    color: #405060;
    padding: 5px 8px;
    font-size: 12px;
    font-weight: 750;
  }

  .checks {
    display: grid;
    gap: 8px;
    margin-top: 10px;
  }

  .check-row {
    display: grid;
    grid-template-columns: 8px minmax(0, 1fr);
    column-gap: 8px;
  }

  .check-row::before {
    content: "";
    width: 8px;
    height: 8px;
    margin-top: 5px;
    border-radius: 999px;
  }

  .check-row strong,
  .check-row span {
    grid-column: 2;
  }

  .hotkeys {
    display: grid;
    gap: 7px;
  }

  .fine-print {
    margin-top: 10px;
  }

  .subhead {
    margin: 16px 0 8px;
    color: #5c6875;
    font-size: 12px;
    font-weight: 800;
    text-transform: uppercase;
  }

  dl {
    display: grid;
    gap: 10px;
    margin: 0;
  }

  dl div {
    display: grid;
    gap: 3px;
  }

  dt {
    color: #657382;
    font-size: 12px;
    font-weight: 800;
    text-transform: uppercase;
  }

  dd {
    margin: 0;
    overflow-wrap: anywhere;
    color: #1f2d3b;
    font-size: 13px;
  }

  .log {
    height: 358px;
    overflow: auto;
    border: 1px solid #d6dde5;
    border-radius: 8px;
    background: #101820;
    padding: 10px;
    scrollbar-color: #52657a #121c26;
    scrollbar-width: thin;
  }

  .log::-webkit-scrollbar {
    width: 11px;
    height: 11px;
  }

  .log::-webkit-scrollbar-track {
    border-radius: 0 8px 8px 0;
    background: #121c26;
  }

  .log::-webkit-scrollbar-thumb {
    min-height: 38px;
    border: 3px solid #121c26;
    border-radius: 999px;
    background: #52657a;
  }

  .log::-webkit-scrollbar-thumb:hover {
    background: #7890aa;
  }

  .log p {
    margin: 0 0 6px;
    color: #d7e2ee;
    font-family: "Cascadia Mono", Consolas, monospace;
    font-size: 12px;
    line-height: 1.45;
    overflow-wrap: anywhere;
  }

  .log p[data-level="warn"] {
    color: #f3d28a;
  }

  .log p[data-level="error"] {
    color: #ffb5bd;
  }

  .log p span,
  .log p b,
  .log p em {
    margin-right: 7px;
    color: #8ea0b4;
    font-weight: 800;
    font-style: normal;
  }

  .log p b {
    color: #9dbfe8;
  }

  .stream-log p {
    color: #b8ead4;
  }

  .stream-log p b {
    color: #78d1a9;
  }

  @media (max-width: 860px) {
    header {
      align-items: start;
      flex-direction: column;
    }

    .grid {
      grid-template-columns: 1fr;
    }

    .grid.single {
      grid-template-columns: 1fr;
    }

    .status-grid {
      grid-template-columns: 1fr;
    }

    .launch-panel,
    .log-panel {
      grid-row: auto;
    }
  }

  @media (max-width: 520px) {
    .shell {
      width: min(100vw - 20px, 1120px);
      padding: 14px 0;
    }

    .game-grid,
    .segmented,
    .startup-grid,
    .log-filters,
    .tabs {
      grid-template-columns: 1fr;
    }

    .tabs {
      display: grid;
    }

    .state-pill {
      white-space: normal;
    }
  }
</style>
