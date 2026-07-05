use std::{
    fmt,
    io::{Read, Write},
    net::{TcpListener, ToSocketAddrs},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
        mpsc::{self, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use native_tls::{Identity, Protocol, TlsAcceptor, TlsConnector, TlsStream};

use crate::{
    config_proxy::{self, PatchedChatServer, LOCALHOST_DOMAIN},
    models::{
        AppSnapshot, ConnectionHealth, HealthState, HealthStep, LaunchGame, LogCategory, LogEntry,
        LogLevel, PreflightCheck, PreflightReport, PresenceStatus, StartupStatus, StreamEvent,
    },
    persistence, presence, riot,
};

#[cfg(not(test))]
use crate::lcu_api;

const STREAM_EVENT_PREVIEW_CHARS: usize = 1_400;
const LOG_BUFFER_LIMIT: usize = 240;
const STREAM_EVENT_BUFFER_LIMIT: usize = 800;
const STARTUP_PRESENCE_REPLAY_IDLE_MS: u64 = 350;
const STARTUP_PRESENCE_REPLAY_MIN_AGE_MS: u64 = 1_200;

#[derive(Debug, Clone)]
pub struct StartOptions {
    pub game: LaunchGame,
    pub game_patchline: String,
    pub riot_client_params: Option<String>,
    pub game_params: Option<String>,
    pub launch_game: bool,
}

pub struct AppState {
    runtime: Option<ServiceRuntime>,
    enabled: Arc<AtomicBool>,
    safe_mode: Arc<AtomicBool>,
    helper_friend: Arc<AtomicBool>,
    auto_accept: Arc<AtomicBool>,
    auto_accept_delay_ms: Arc<AtomicU32>,
    auto_accept_state: Arc<Mutex<String>>,
    discord_webhook_url: Arc<Mutex<String>>,
    status: Arc<Mutex<PresenceStatus>>,
    startup_status: StartupStatus,
    connect_to_muc: Arc<AtomicBool>,
    health: Arc<Mutex<ConnectionHealth>>,
    logs: Arc<Mutex<Vec<LogEntry>>>,
    stream_events: Arc<Mutex<Vec<StreamEvent>>>,
}

struct ServiceRuntime {
    running: Arc<AtomicBool>,
    chat_port: u16,
    config_port: u16,
    riot_chat: Arc<Mutex<Option<PatchedChatServer>>>,
    active_connections: Arc<AtomicUsize>,
    reconnect_attempts: Arc<AtomicU32>,
    riot_client_path: Option<String>,
    active_game: LaunchGame,
}

impl AppState {
    pub fn load() -> Result<Self> {
        let startup_status = persistence::read_startup_status();
        let session_status = match startup_status {
            StartupStatus::Chat => PresenceStatus::Chat,
            StartupStatus::Offline => PresenceStatus::Offline,
            StartupStatus::Mobile => PresenceStatus::Mobile,
            StartupStatus::Last => persistence::read_session_status(),
        };

        let logs = Arc::new(Mutex::new(Vec::new()));
        let auto_accept = Arc::new(AtomicBool::new(persistence::read_auto_accept()));
        let auto_accept_delay_ms =
            Arc::new(AtomicU32::new(persistence::read_auto_accept_delay_ms()));
        let discord_webhook_url = Arc::new(Mutex::new(persistence::read_discord_webhook_url()));
        let auto_accept_state = Arc::new(Mutex::new(
            if auto_accept.load(Ordering::Relaxed) {
                "Waiting for League Client"
            } else {
                "Disabled"
            }
            .to_string(),
        ));
        #[cfg(not(test))]
        start_auto_accept_monitor(
            auto_accept.clone(),
            auto_accept_delay_ms.clone(),
            auto_accept_state.clone(),
            discord_webhook_url.clone(),
            logs.clone(),
        );

        Ok(Self {
            runtime: None,
            enabled: Arc::new(AtomicBool::new(true)),
            safe_mode: Arc::new(AtomicBool::new(false)),
            helper_friend: Arc::new(AtomicBool::new(persistence::read_helper_friend())),
            auto_accept,
            auto_accept_delay_ms,
            auto_accept_state,
            discord_webhook_url,
            status: Arc::new(Mutex::new(session_status)),
            startup_status,
            connect_to_muc: Arc::new(AtomicBool::new(true)),
            health: Arc::new(Mutex::new(ConnectionHealth::default())),
            logs,
            stream_events: Arc::new(Mutex::new(Vec::new())),
        })
    }

    pub fn snapshot(&self) -> AppSnapshot {
        let runtime = self.active_runtime();
        let riot_chat = runtime.and_then(|r| r.riot_chat.lock().ok()?.clone());
        let active_game = runtime.map(|r| r.active_game);

        if let Some(runtime) = runtime {
            set_health(&self.health, |health| {
                health.active_connections = runtime.active_connections.load(Ordering::Relaxed);
                health.reconnect_attempts = runtime.reconnect_attempts.load(Ordering::Relaxed);
            });
        } else if self
            .runtime
            .as_ref()
            .is_some_and(|runtime| !runtime.running.load(Ordering::Relaxed))
        {
            self.reset_health();
        }

        AppSnapshot {
            running: runtime.is_some(),
            enabled: self.enabled.load(Ordering::Relaxed),
            safe_mode: self.safe_mode.load(Ordering::Relaxed),
            helper_friend: self.helper_friend.load(Ordering::Relaxed),
            auto_accept: self.auto_accept.load(Ordering::Relaxed),
            auto_accept_delay_ms: self.auto_accept_delay_ms.load(Ordering::Relaxed),
            auto_accept_state: self
                .auto_accept_state
                .lock()
                .map(|state| state.clone())
                .unwrap_or_else(|_| "Unavailable".to_string()),
            discord_webhook_url: self
                .discord_webhook_url
                .lock()
                .map(|url| url.clone())
                .unwrap_or_default(),
            status: status_value(&self.status),
            startup_status: self.startup_status,
            connect_to_muc: self.connect_to_muc.load(Ordering::Relaxed),
            health: self.health.lock().map(|h| h.clone()).unwrap_or_default(),
            chat_port: runtime.map(|r| r.chat_port),
            config_port: runtime.map(|r| r.config_port),
            riot_chat_host: riot_chat.as_ref().map(|server| server.host.clone()),
            riot_chat_port: riot_chat.as_ref().map(|server| server.port),
            riot_client_path: runtime
                .and_then(|r| r.riot_client_path.clone())
                .or_else(|| riot::riot_client_path().map(|p| p.display().to_string())),
            active_game,
            active_game_label: active_game.map(|game| game.label().to_string()),
            logs: self
                .logs
                .lock()
                .map(|logs| logs.clone())
                .unwrap_or_default(),
            stream_events: self
                .stream_events
                .lock()
                .map(|events| events.clone())
                .unwrap_or_default(),
        }
    }

    pub fn start(&mut self, options: StartOptions) -> Result<()> {
        self.clear_stopped_runtime();
        if self.active_runtime().is_some() {
            return Err(anyhow!("Ghosty is already running"));
        }
        self.reset_health();

        let riot_client = riot::riot_client_path();
        if options.launch_game && riot_client.is_none() {
            riot::ensure_riot_client()?;
        }
        riot::validate_launch_params(
            options.game,
            &options.game_patchline,
            options.riot_client_params.as_deref(),
            options.game_params.as_deref(),
        )?;

        let running = Arc::new(AtomicBool::new(true));
        let riot_chat = Arc::new(Mutex::new(None));
        let active_connections = Arc::new(AtomicUsize::new(0));
        let reconnect_attempts = Arc::new(AtomicU32::new(0));
        let (patched_tx, patched_rx) = mpsc::channel();
        let (log_tx, log_rx) = mpsc::channel();
        let (stream_tx, stream_rx) = mpsc::channel();
        pump_logs(log_rx, self.logs.clone());
        pump_stream_events(stream_rx, self.stream_events.clone());

        let chat_listener =
            TcpListener::bind(("127.0.0.1", 0)).context("Unable to bind chat proxy")?;
        chat_listener.set_nonblocking(true)?;
        let chat_port = chat_listener.local_addr()?.port();

        let config_port =
            config_proxy::start(chat_port, running.clone(), patched_tx, log_tx.clone())?;
        set_health(&self.health, |health| {
            health.config_proxy.state = HealthState::Ready;
            health.config_proxy.detail = format!("127.0.0.1:{config_port}");
        });

        let status = self.status.clone();
        let enabled = self.enabled.clone();
        let safe_mode = self.safe_mode.clone();
        let helper_friend = self.helper_friend.clone();
        let auto_accept = self.auto_accept.clone();
        let auto_accept_state = self.auto_accept_state.clone();
        let connect_to_muc = self.connect_to_muc.clone();
        let runtime_running = running.clone();
        let runtime_riot_chat = riot_chat.clone();
        let runtime_health = self.health.clone();
        let runtime_connections = active_connections.clone();
        let runtime_reconnects = reconnect_attempts.clone();
        thread::spawn(move || {
            let certificate = match proxy_identity(&log_tx) {
                Ok(identity) => identity,
                Err(error) => {
                    runtime_running.store(false, Ordering::Relaxed);
                    log(
                        &log_tx,
                        LogCategory::Error,
                        LogLevel::Error,
                        format!("Unable to load proxy certificate: {error:#}"),
                    );
                    set_health(&runtime_health, |health| {
                        health.tls_connected.state = HealthState::Error;
                        health.tls_connected.detail = "Certificate load failed".to_string();
                    });
                    return;
                }
            };
            let acceptor = match TlsAcceptor::builder(certificate)
                .min_protocol_version(Some(Protocol::Tlsv12))
                .build()
            {
                Ok(acceptor) => acceptor,
                Err(error) => {
                    runtime_running.store(false, Ordering::Relaxed);
                    log(
                        &log_tx,
                        LogCategory::Error,
                        LogLevel::Error,
                        format!("Unable to create TLS acceptor: {error}"),
                    );
                    set_health(&runtime_health, |health| {
                        health.tls_connected.state = HealthState::Error;
                        health.tls_connected.detail = "TLS acceptor setup failed".to_string();
                    });
                    return;
                }
            };

            while runtime_running.load(Ordering::Relaxed) {
                let server = match patched_rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(server) => server,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(_) => {
                        runtime_running.store(false, Ordering::Relaxed);
                        set_health(&runtime_health, |health| {
                            health.config_patched.state = HealthState::Error;
                            health.config_patched.detail =
                                "Config proxy stopped before Riot chat settings arrived"
                                    .to_string();
                        });
                        return;
                    }
                };
                if !runtime_running.load(Ordering::Relaxed) {
                    return;
                }
                if let Ok(mut runtime_riot_chat) = runtime_riot_chat.lock() {
                    *runtime_riot_chat = Some(server.clone());
                } else {
                    log(
                        &log_tx,
                        LogCategory::Error,
                        LogLevel::Error,
                        "Unable to record Riot chat server because runtime state is poisoned",
                    );
                }
                set_health(&runtime_health, |health| {
                    health.config_patched.state = HealthState::Ready;
                    health.config_patched.detail =
                        "Riot config returned patched chat settings".to_string();
                    health.chat_server.state = HealthState::Ready;
                    health.chat_server.detail = format!("{}:{}", server.host, server.port);
                });
                log(
                    &log_tx,
                    LogCategory::Config,
                    LogLevel::Info,
                    format!(
                        "Original Riot chat server is {}:{}",
                        server.host, server.port
                    ),
                );
                let chat_context = ChatProxyContext {
                    running: runtime_running.clone(),
                    acceptor,
                    server,
                    enabled,
                    safe_mode,
                    helper_friend,
                    auto_accept,
                    auto_accept_state,
                    status,
                    connect_to_muc,
                    health: runtime_health,
                    active_connections: runtime_connections,
                    reconnect_attempts: runtime_reconnects,
                    log_tx,
                    stream_tx,
                };
                serve_chat(chat_listener, chat_context);
                return;
            }
        });

        if options.launch_game {
            if let Some(path) = riot_client.as_ref() {
                if let Err(error) = riot::launch_riot_client(
                    path,
                    config_port,
                    options.game,
                    &options.game_patchline,
                    options.riot_client_params.as_deref(),
                    options.game_params.as_deref(),
                ) {
                    running.store(false, Ordering::Relaxed);
                    self.reset_health();
                    self.note_warning(format!(
                        "Stopped Ghosty proxy because Riot Client launch failed: {error:#}"
                    ));
                    return Err(error);
                }
                log(
                    &self.log_sender(),
                    LogCategory::Launch,
                    LogLevel::Info,
                    format!("Launched Riot Client for {}", options.game.label()),
                );
            }
        } else {
            log(
                &self.log_sender(),
                LogCategory::Launch,
                LogLevel::Info,
                "Proxy started without launching Riot Client",
            );
        }

        self.runtime = Some(ServiceRuntime {
            running,
            chat_port,
            config_port,
            riot_chat,
            active_connections,
            reconnect_attempts,
            riot_client_path: riot_client.map(|p| p.display().to_string()),
            active_game: options.game,
        });

        Ok(())
    }

    fn active_runtime(&self) -> Option<&ServiceRuntime> {
        self.runtime
            .as_ref()
            .filter(|runtime| runtime.running.load(Ordering::Relaxed))
    }

    fn clear_stopped_runtime(&mut self) {
        if self
            .runtime
            .as_ref()
            .is_some_and(|runtime| !runtime.running.load(Ordering::Relaxed))
        {
            self.runtime = None;
        }
    }

    pub fn clean_restart(&mut self, options: StartOptions) -> Result<()> {
        self.stop();
        if let Err(error) = riot::kill_riot_processes() {
            self.note_warning(format!(
                "Unable to stop Riot processes before restart: {error:#}"
            ));
        }
        thread::sleep(Duration::from_secs(2));
        self.start(options)
    }

    pub fn kill_riot_processes(&mut self) -> Result<()> {
        match riot::kill_riot_processes() {
            Ok(()) => {
                self.push_log("Requested Riot process cleanup");
                Ok(())
            }
            Err(error) => {
                self.note_warning(format!("Unable to stop Riot processes: {error:#}"));
                Err(error)
            }
        }
    }

    pub fn stop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.running.store(false, Ordering::Relaxed);
            self.push_log("Stopped Ghosty proxy");
            self.reset_health();
        }
    }

    pub fn set_status(&mut self, status: PresenceStatus) -> Result<()> {
        persistence::write_session_status(status)?;
        *self.status.lock().map_err(|e| anyhow!(e.to_string()))? = status;
        self.push_log(format!("Presence target set to {}", status.as_xmpp()));
        Ok(())
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
        self.push_log(if enabled {
            "Presence masking enabled"
        } else {
            "Presence masking disabled"
        });
    }

    pub fn set_safe_mode(&mut self, safe_mode: bool) {
        self.safe_mode.store(safe_mode, Ordering::Relaxed);
        self.push_log(if safe_mode {
            "Safe mode enabled: chat is forwarded without presence rewriting"
        } else {
            "Safe mode disabled: presence rewriting is active"
        });
    }

    pub fn set_helper_friend(&mut self, helper_friend: bool) -> Result<()> {
        persistence::write_helper_friend(helper_friend)?;
        self.helper_friend.store(helper_friend, Ordering::Relaxed);
        self.push_log(if helper_friend {
            "Helper friend enabled; it will appear after Riot sends a roster refresh"
        } else {
            "Helper friend disabled"
        });
        Ok(())
    }

    pub fn set_auto_accept(&mut self, auto_accept: bool) -> Result<()> {
        persistence::write_auto_accept(auto_accept)?;
        self.auto_accept.store(auto_accept, Ordering::Relaxed);
        set_auto_accept_state(
            &self.auto_accept_state,
            if auto_accept {
                "Watching for ready check"
            } else {
                "Disabled"
            },
        );
        self.push_log(if auto_accept {
            "Auto accept enabled"
        } else {
            "Auto accept disabled"
        });
        Ok(())
    }

    pub fn set_auto_accept_delay_ms(&mut self, delay_ms: u32) -> Result<()> {
        let delay_ms = delay_ms.clamp(0, 10_000);
        persistence::write_auto_accept_delay_ms(delay_ms)?;
        self.auto_accept_delay_ms.store(delay_ms, Ordering::Relaxed);
        self.push_log(format!("Auto accept delay set to {delay_ms}ms"));
        Ok(())
    }

    pub fn set_discord_webhook_url(&mut self, url: String) -> Result<()> {
        let url = url.trim().to_string();
        persistence::write_discord_webhook_url(&url)?;
        if let Ok(mut webhook_url) = self.discord_webhook_url.lock() {
            *webhook_url = url;
        }
        self.push_log("Discord auto-accept webhook updated");
        Ok(())
    }

    pub fn set_connect_to_muc(&mut self, connect_to_muc: bool) {
        self.connect_to_muc.store(connect_to_muc, Ordering::Relaxed);
        self.push_log(if connect_to_muc {
            "Lobby chat passthrough enabled"
        } else {
            "Lobby chat passthrough disabled"
        });
    }

    pub fn set_startup_status(&mut self, startup_status: StartupStatus) -> Result<()> {
        persistence::write_startup_status(startup_status)?;
        self.startup_status = startup_status;
        self.push_log("Saved startup status preference");
        Ok(())
    }

    pub fn preflight(&self) -> PreflightReport {
        let checks = vec![
            check_riot_client(),
            check_localhost_resolution(),
            check_certificate(),
            check_port_available(),
            check_riot_processes(),
        ];
        let ok = checks.iter().all(|check| check.ok);
        PreflightReport { ok, checks }
    }

    pub fn note_warning(&self, message: impl Into<String>) {
        if let Ok(mut logs) = self.logs.lock() {
            logs.push(LogEntry::new(
                LogCategory::System,
                LogLevel::Warn,
                message.into(),
            ));
            keep_recent(&mut logs);
        }
    }

    fn push_log(&self, message: impl Into<String>) {
        if let Ok(mut logs) = self.logs.lock() {
            logs.push(LogEntry::new(
                LogCategory::System,
                LogLevel::Info,
                message.into(),
            ));
            keep_recent(&mut logs);
        }
    }

    fn log_sender(&self) -> Sender<LogEntry> {
        let logs = self.logs.clone();
        let (tx, rx) = mpsc::channel();
        pump_logs(rx, logs);
        tx
    }

    fn reset_health(&self) {
        if let Ok(mut health) = self.health.lock() {
            *health = ConnectionHealth::default();
        }
    }
}

#[cfg(not(test))]
fn start_auto_accept_monitor(
    enabled: Arc<AtomicBool>,
    delay_ms: Arc<AtomicU32>,
    state: Arc<Mutex<String>>,
    discord_webhook_url: Arc<Mutex<String>>,
    logs: Arc<Mutex<Vec<LogEntry>>>,
) {
    thread::spawn(move || {
        let mut ready_check_active = false;
        let mut last_phase = String::new();

        loop {
            if !enabled.load(Ordering::Relaxed) {
                ready_check_active = false;
                set_auto_accept_state(&state, "Disabled");
                thread::sleep(Duration::from_millis(750));
                continue;
            }

            match lcu_api::gameflow_phase() {
                Ok(phase) => {
                    if phase != last_phase {
                        last_phase = phase.clone();
                        if phase != "ReadyCheck" {
                            set_auto_accept_state(&state, format!("Watching: {phase}"));
                        }
                    }

                    if phase == "ReadyCheck" {
                        if !ready_check_active {
                            ready_check_active = true;
                            let delay = delay_ms.load(Ordering::Relaxed);
                            set_auto_accept_state(
                                &state,
                                format!("Ready check found; accepting in {delay}ms"),
                            );
                            push_log_to(
                                &logs,
                                LogCategory::System,
                                LogLevel::Info,
                                format!("Auto accept detected ready check; accepting in {delay}ms"),
                            );
                            sleep_auto_accept_delay(&enabled, delay);
                            if enabled.load(Ordering::Relaxed) {
                                match lcu_api::accept_ready_check() {
                                    Ok(response) if response.ok => {
                                        set_auto_accept_state(&state, "Accepted ready check");
                                        push_log_to(
                                            &logs,
                                            LogCategory::System,
                                            LogLevel::Info,
                                            "Auto accepted ready check",
                                        );
                                        notify_discord_auto_accept(&discord_webhook_url, &logs);
                                    }
                                    Ok(response) => {
                                        set_auto_accept_state(
                                            &state,
                                            format!("Accept returned HTTP {}", response.status),
                                        );
                                        push_log_to(
                                            &logs,
                                            LogCategory::System,
                                            LogLevel::Warn,
                                            format!(
                                                "Auto accept returned HTTP {}",
                                                response.status
                                            ),
                                        );
                                    }
                                    Err(error) => {
                                        set_auto_accept_state(&state, "Accept failed");
                                        push_log_to(
                                            &logs,
                                            LogCategory::System,
                                            LogLevel::Warn,
                                            format!("Auto accept failed: {error:#}"),
                                        );
                                    }
                                }
                            }
                        }
                        thread::sleep(Duration::from_millis(250));
                    } else {
                        ready_check_active = false;
                        thread::sleep(Duration::from_millis(750));
                    }
                }
                Err(_) => {
                    ready_check_active = false;
                    last_phase.clear();
                    set_auto_accept_state(&state, "Waiting for League Client");
                    thread::sleep(Duration::from_secs(2));
                }
            }
        }
    });
}

#[cfg(not(test))]
fn notify_discord_auto_accept(
    discord_webhook_url: &Arc<Mutex<String>>,
    logs: &Arc<Mutex<Vec<LogEntry>>>,
) {
    let webhook_url = discord_webhook_url
        .lock()
        .map(|url| url.trim().to_string())
        .unwrap_or_default();
    if webhook_url.is_empty() {
        return;
    }

    let ign = lcu_api::current_summoner_display_name().unwrap_or_else(|error| {
        push_log_to(
            logs,
            LogCategory::System,
            LogLevel::Warn,
            format!("Unable to resolve summoner name for Discord webhook: {error:#}"),
        );
        "Ghosty".to_string()
    });
    let content = format!("{ign} has auto accepted a game");
    match post_discord_webhook(&webhook_url, &content) {
        Ok(()) => push_log_to(
            logs,
            LogCategory::System,
            LogLevel::Info,
            "Posted auto accept notification to Discord",
        ),
        Err(error) => push_log_to(
            logs,
            LogCategory::System,
            LogLevel::Warn,
            format!("Discord auto accept webhook failed: {error:#}"),
        ),
    }
}

#[cfg(not(test))]
fn post_discord_webhook(webhook_url: &str, content: &str) -> Result<()> {
    let response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(6))
        .build()
        .context("Unable to build Discord webhook client")?
        .post(webhook_url)
        .json(&serde_json::json!({ "content": content }))
        .send()
        .context("Unable to post Discord webhook")?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(anyhow!("Discord returned HTTP {}", response.status()))
    }
}

#[cfg(not(test))]
fn sleep_auto_accept_delay(enabled: &Arc<AtomicBool>, delay_ms: u32) {
    let mut remaining = delay_ms;
    while remaining > 0 && enabled.load(Ordering::Relaxed) {
        let step = remaining.min(100);
        thread::sleep(Duration::from_millis(u64::from(step)));
        remaining -= step;
    }
}

fn set_auto_accept_state(state: &Arc<Mutex<String>>, value: impl Into<String>) {
    if let Ok(mut state) = state.lock() {
        *state = value.into();
    }
}

#[cfg(not(test))]
fn push_log_to(
    logs: &Arc<Mutex<Vec<LogEntry>>>,
    category: LogCategory,
    level: LogLevel,
    message: impl Into<String>,
) {
    if let Ok(mut logs) = logs.lock() {
        logs.push(LogEntry::new(category, level, message.into()));
        keep_recent(&mut logs);
    }
}

#[derive(Clone)]
struct ChatProxyContext {
    running: Arc<AtomicBool>,
    acceptor: TlsAcceptor,
    server: PatchedChatServer,
    enabled: Arc<AtomicBool>,
    safe_mode: Arc<AtomicBool>,
    helper_friend: Arc<AtomicBool>,
    auto_accept: Arc<AtomicBool>,
    auto_accept_state: Arc<Mutex<String>>,
    status: Arc<Mutex<PresenceStatus>>,
    connect_to_muc: Arc<AtomicBool>,
    health: Arc<Mutex<ConnectionHealth>>,
    active_connections: Arc<AtomicUsize>,
    reconnect_attempts: Arc<AtomicU32>,
    log_tx: Sender<LogEntry>,
    stream_tx: Sender<StreamEvent>,
}

fn serve_chat(listener: TcpListener, context: ChatProxyContext) {
    log(
        &context.log_tx,
        LogCategory::Chat,
        LogLevel::Info,
        "Chat proxy is ready for Riot Client connections",
    );

    while context.running.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((incoming, _)) => {
                let connection_context = context.clone();
                thread::spawn(move || {
                    if let Err(error) = proxy_connection(incoming, connection_context.clone()) {
                        record_proxy_connection_error(
                            &connection_context.running,
                            &connection_context.reconnect_attempts,
                            &connection_context.health,
                            &connection_context.log_tx,
                            error,
                        );
                    }
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(30));
            }
            Err(error) => log(
                &context.log_tx,
                LogCategory::Error,
                LogLevel::Error,
                format!("Chat proxy accept failed: {error}"),
            ),
        }
    }
}

fn record_proxy_connection_error(
    running: &Arc<AtomicBool>,
    reconnect_attempts: &Arc<AtomicU32>,
    health: &Arc<Mutex<ConnectionHealth>>,
    log_tx: &Sender<LogEntry>,
    error: anyhow::Error,
) {
    if !running.load(Ordering::Relaxed) {
        log(
            log_tx,
            LogCategory::Chat,
            LogLevel::Info,
            format!("Chat proxy connection closed after Ghosty stopped: {error:#}"),
        );
        return;
    }

    if error.downcast_ref::<CleanConnectionClose>().is_some() {
        log(
            log_tx,
            LogCategory::Chat,
            LogLevel::Info,
            "Chat proxy connection closed cleanly",
        );
        return;
    }

    reconnect_attempts.fetch_add(1, Ordering::Relaxed);
    set_health(health, |health| {
        health.tls_connected.state = HealthState::Warning;
        health.tls_connected.detail = "Last connection closed".to_string();
        health.xmpp_active.state = HealthState::Warning;
        health.xmpp_active.detail = "Waiting for Riot Client reconnect".to_string();
        health.reconnect_attempts = reconnect_attempts.load(Ordering::Relaxed);
    });
    log(
        log_tx,
        LogCategory::Chat,
        LogLevel::Warn,
        format!("Chat proxy connection closed: {error:#}"),
    );
}

#[derive(Debug)]
struct CleanConnectionClose;

impl fmt::Display for CleanConnectionClose {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("connection closed cleanly")
    }
}

impl std::error::Error for CleanConnectionClose {}

fn proxy_connection(incoming: std::net::TcpStream, context: ChatProxyContext) -> Result<()> {
    incoming
        .set_nonblocking(false)
        .context("Unable to switch incoming Riot Client socket to blocking mode")?;
    log_client_hello(&incoming, &context.log_tx);
    let mut incoming = context
        .acceptor
        .accept(incoming)
        .map_err(|error| anyhow!("incoming Riot Client TLS handshake failed: {error}"))?;
    incoming
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(250)))?;

    let outgoing_tcp =
        std::net::TcpStream::connect((context.server.host.as_str(), context.server.port))?;
    let mut outgoing = TlsConnector::new()?
        .connect(&context.server.host, outgoing_tcp)
        .map_err(|error| anyhow!("outgoing Riot chat TLS handshake failed: {error}"))?;
    outgoing
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(250)))?;

    let mut valorant_version = None;
    let mut buffer = [0_u8; 16 * 1024];
    let mut logged_client_flow = false;
    let mut logged_server_flow = false;
    let mut logged_client_presence = false;
    let mut logged_server_presence = false;
    let mut inserted_helper_friend = false;
    let mut sent_helper_roster = false;
    let mut sent_helper_presence = false;
    let mut refreshed_helper_presence_after_client_presence = false;
    let mut sent_helper_intro = false;
    let mut logged_roster_without_helper = false;
    let mut startup_roster_ready = false;
    let mut startup_presence_replay = StartupPresenceReplay::new();
    let helper_jid = presence::helper_jid_for_chat_identity(
        &context.server.host,
        context.server.affinity.as_deref(),
    );
    let mut helper_command_buffer = HelperCommandBuffer::new(helper_jid.clone());
    let mut presence_buffer = ClientPresenceBuffer::new();
    let mut server_presence_stats = PresenceStats::default();
    let mut roster_jid_domains = std::collections::BTreeMap::<String, String>::new();
    let mut roster_domains_logged = false;
    let mut logged_presence_domain_normalization = false;
    let mut incoming_text_buffer = Utf8StreamBuffer::new();
    let mut outgoing_text_buffer = Utf8StreamBuffer::new();
    let mut was_helper_friend_enabled = context.helper_friend.load(Ordering::Relaxed);
    context.active_connections.fetch_add(1, Ordering::Relaxed);
    let _connection_guard = ActiveConnectionGuard {
        active_connections: context.active_connections.clone(),
        health: context.health.clone(),
    };
    set_health(&context.health, |health| {
        health.tls_connected.state = HealthState::Active;
        health.tls_connected.detail = "TLS tunnel established".to_string();
        health.active_connections = context.active_connections.load(Ordering::Relaxed);
        health.reconnect_attempts = context.reconnect_attempts.load(Ordering::Relaxed);
    });

    log(
        &context.log_tx,
        LogCategory::Chat,
        LogLevel::Info,
        "Opened proxied Riot chat connection",
    );

    loop {
        if !context.running.load(Ordering::Relaxed) {
            log(
                &context.log_tx,
                LogCategory::Chat,
                LogLevel::Info,
                "Closing proxied Riot chat connection because Ghosty stopped",
            );
            return Ok(());
        }

        let mut made_progress = false;
        let helper_friend_enabled = context.helper_friend.load(Ordering::Relaxed);

        if !helper_friend_enabled && was_helper_friend_enabled {
            helper_command_buffer.clear();
        }
        was_helper_friend_enabled = helper_friend_enabled;

        match read_bytes(&mut incoming, &mut buffer)? {
            StreamRead::Data(bytes) => {
                made_progress = true;
                log_stream_bytes(&context.stream_tx, "client -> ghosty", &bytes);
                if !logged_client_flow {
                    logged_client_flow = true;
                    log(
                        &context.log_tx,
                        LogCategory::Chat,
                        LogLevel::Info,
                        "Client-to-server chat bytes are flowing",
                    );
                }
                let previous_valorant_version = valorant_version.clone();
                let mut saw_client_presence = false;
                match incoming_text_buffer.push(bytes) {
                    Utf8Chunk::Text { content, bytes } if helper_friend_enabled => {
                        saw_client_presence = content.contains("<presence");
                        match helper_command_buffer.push_with_auto_accept(
                            &content,
                            &context.enabled,
                            &context.status,
                            &context.auto_accept,
                            &context.auto_accept_state,
                        )? {
                            HelperCommandResult::NotHelper => {
                                forward_client_chat(
                                    &content,
                                    &bytes,
                                    &mut outgoing,
                                    &context,
                                    &mut presence_buffer,
                                    &mut valorant_version,
                                )?;
                            }
                            HelperCommandResult::Pending => {}
                            HelperCommandResult::Complete { reply, passthrough } => {
                                if !passthrough.is_empty() {
                                    let passthrough_content = String::from_utf8_lossy(&passthrough);
                                    forward_client_chat(
                                        &passthrough_content,
                                        &passthrough,
                                        &mut outgoing,
                                        &context,
                                        &mut presence_buffer,
                                        &mut valorant_version,
                                    )?;
                                }
                                if let Some(reply) = reply {
                                    let message =
                                        presence::helper_chat_message(&helper_jid, &reply);
                                    log_stream_bytes(
                                        &context.stream_tx,
                                        "ghosty -> client helper message",
                                        message.as_bytes(),
                                    );
                                    incoming.write_all(message.as_bytes())?;
                                }
                            }
                        }
                    }
                    Utf8Chunk::Text { content, bytes }
                        if !context.safe_mode.load(Ordering::Relaxed) =>
                    {
                        saw_client_presence = content.contains("<presence");
                        forward_client_chat(
                            &content,
                            &bytes,
                            &mut outgoing,
                            &context,
                            &mut presence_buffer,
                            &mut valorant_version,
                        )?;
                    }
                    Utf8Chunk::Text { bytes, .. } => {
                        let pending = presence_buffer.flush();
                        if !pending.is_empty() {
                            log_stream_bytes(&context.stream_tx, "ghosty -> riot", &pending);
                            outgoing.write_all(&pending)?;
                        }
                        log_stream_bytes(&context.stream_tx, "ghosty -> riot", &bytes);
                        outgoing.write_all(&bytes)?;
                    }
                    Utf8Chunk::Binary(bytes) => {
                        let pending = presence_buffer.flush();
                        if !pending.is_empty() {
                            log_stream_bytes(&context.stream_tx, "ghosty -> riot", &pending);
                            outgoing.write_all(&pending)?;
                        }
                        log_stream_bytes(&context.stream_tx, "ghosty -> riot", &bytes);
                        outgoing.write_all(&bytes)?;
                    }
                    Utf8Chunk::Pending => {}
                }
                if helper_friend_enabled
                    && inserted_helper_friend
                    && sent_helper_presence
                    && saw_client_presence
                    && !refreshed_helper_presence_after_client_presence
                {
                    refreshed_helper_presence_after_client_presence = true;
                    let helper_presence =
                        presence::helper_presence(&helper_jid, valorant_version.as_deref());
                    log_stream_bytes(
                        &context.stream_tx,
                        "ghosty -> client helper presence refresh",
                        helper_presence.as_bytes(),
                    );
                    incoming.write_all(helper_presence.as_bytes())?;
                    log(
                        &context.log_tx,
                        LogCategory::Chat,
                        LogLevel::Info,
                        "Refreshed Ghosty helper friend presence after client presence",
                    );
                }
                if helper_friend_enabled
                    && inserted_helper_friend
                    && sent_helper_presence
                    && valorant_version.is_some()
                    && valorant_version != previous_valorant_version
                {
                    let helper_presence =
                        presence::helper_presence(&helper_jid, valorant_version.as_deref());
                    log_stream_bytes(
                        &context.stream_tx,
                        "ghosty -> client helper presence update",
                        helper_presence.as_bytes(),
                    );
                    incoming.write_all(helper_presence.as_bytes())?;
                    log(
                        &context.log_tx,
                        LogCategory::Chat,
                        LogLevel::Info,
                        "Updated Ghosty helper friend presence",
                    );
                }
                if saw_client_presence && !logged_client_presence {
                    logged_client_presence = true;
                    log(
                        &context.log_tx,
                        LogCategory::Chat,
                        LogLevel::Info,
                        "Client-to-server presence stanzas are flowing",
                    );
                }
            }
            StreamRead::Closed => {
                let pending = incoming_text_buffer.flush();
                if !pending.is_empty() {
                    log_stream_bytes(&context.stream_tx, "ghosty -> riot", &pending);
                    outgoing.write_all(&pending)?;
                }
                let pending = presence_buffer.flush();
                if !pending.is_empty() {
                    log_stream_bytes(&context.stream_tx, "ghosty -> riot", &pending);
                    outgoing.write_all(&pending)?;
                }
                return Err(CleanConnectionClose.into());
            }
            StreamRead::WouldBlock => {}
        }

        match read_bytes(&mut outgoing, &mut buffer)? {
            StreamRead::Data(bytes) => {
                made_progress = true;
                log_stream_bytes(&context.stream_tx, "riot -> ghosty", &bytes);
                let mut bytes_to_client = Vec::new();
                match outgoing_text_buffer.push(bytes) {
                    Utf8Chunk::Text { content, bytes } => {
                        bytes_to_client = bytes;
                        if content.contains("<presence") && !logged_server_presence {
                            logged_server_presence = true;
                            log(
                                &context.log_tx,
                                LogCategory::Chat,
                                LogLevel::Info,
                                "Server-to-client presence stanzas are flowing",
                            );
                        }
                        if let Some(message) = server_presence_stats.record_server_batch(&content) {
                            log(&context.log_tx, LogCategory::Chat, LogLevel::Info, message);
                        }
                        if server_presence_stats.is_warmup_ready() {
                            if let Some(masked_presence) =
                                presence_buffer.take_warmup_masked_presence()
                            {
                                if !masked_presence.is_empty() {
                                    log_stream_bytes(
                                        &context.stream_tx,
                                        "ghosty -> riot presence warmup mask",
                                        &masked_presence,
                                    );
                                    outgoing.write_all(&masked_presence)?;
                                }
                                log(
                                    &context.log_tx,
                                    LogCategory::Chat,
                                    LogLevel::Info,
                                    "Finished initial presence warmup and restored masked status",
                                );
                            }
                        }
                        if helper_friend_enabled
                            && !startup_roster_ready
                            && presence::contains_roster_marker(&content)
                        {
                            startup_roster_ready = true;
                            let roster_items = presence::roster_item_count(&content);
                            roster_jid_domains = jid_domain_map(&content, "jid");
                            if !roster_domains_logged {
                                roster_domains_logged = true;
                                if let Some(summary) =
                                    jid_domain_summary(&content, "jid", "Roster JID domains")
                                {
                                    log(
                                        &context.log_tx,
                                        LogCategory::Chat,
                                        LogLevel::Info,
                                        summary,
                                    );
                                }
                            }
                            log(
                                &context.log_tx,
                                LogCategory::Chat,
                                LogLevel::Info,
                                format!(
                                    "Forwarded initial Riot roster unchanged ({roster_items} items in current chunk); Ghosty helper will be added after startup"
                                ),
                            );
                        } else if !helper_friend_enabled
                            && !logged_roster_without_helper
                            && presence::contains_roster_marker(&content)
                        {
                            logged_roster_without_helper = true;
                            startup_roster_ready = true;
                            let roster_items = presence::roster_item_count(&content);
                            if roster_jid_domains.is_empty() {
                                roster_jid_domains = jid_domain_map(&content, "jid");
                            }
                            log(
                                &context.log_tx,
                                LogCategory::Chat,
                                LogLevel::Info,
                                format!(
                                    "Roster marker passed while helper friend was disabled ({roster_items} items in current chunk)"
                                ),
                            );
                        }
                    }
                    Utf8Chunk::Binary(bytes) => {
                        bytes_to_client = bytes;
                    }
                    Utf8Chunk::Pending => {}
                }
                if !bytes_to_client.is_empty() {
                    if !roster_jid_domains.is_empty() {
                        if let Ok(content) = std::str::from_utf8(&bytes_to_client) {
                            if let Some(rewritten) = rewrite_jid_attribute_domains_from_map(
                                content,
                                "from",
                                &roster_jid_domains,
                            ) {
                                if !logged_presence_domain_normalization {
                                    logged_presence_domain_normalization = true;
                                    let before = jid_domains(content, "from");
                                    let after = jid_domains(&rewritten, "from");
                                    log(
                                        &context.log_tx,
                                        LogCategory::Chat,
                                        LogLevel::Info,
                                        format!(
                                            "Normalized server-to-client JID domains for roster matching: {before} -> {after}"
                                        ),
                                    );
                                }
                                bytes_to_client = rewritten.into_bytes();
                            }
                        }
                    }
                    if startup_roster_ready {
                        if let Ok(content) = std::str::from_utf8(&bytes_to_client) {
                            let added = startup_presence_replay.push(content);
                            if added > 0 && startup_presence_replay.stanza_count() == added {
                                log(
                                    &context.log_tx,
                                    LogCategory::Chat,
                                    LogLevel::Info,
                                    "Capturing initial server presence for startup replay",
                                );
                            }
                        }
                    }
                    log_stream_bytes(&context.stream_tx, "ghosty -> client", &bytes_to_client);
                    incoming.write_all(&bytes_to_client)?;
                }
                if helper_friend_enabled
                    && startup_presence_replay.replayed()
                    && !sent_helper_roster
                {
                    sent_helper_roster = true;
                    inserted_helper_friend = true;
                    let roster_push = presence::helper_roster_push(&helper_jid);
                    log_stream_bytes(
                        &context.stream_tx,
                        "ghosty -> client helper roster",
                        roster_push.as_bytes(),
                    );
                    incoming.write_all(roster_push.as_bytes())?;
                    log(
                        &context.log_tx,
                        LogCategory::Chat,
                        LogLevel::Info,
                        "Added Ghosty helper friend after startup roster and presence",
                    );
                }
                if helper_friend_enabled && inserted_helper_friend && !sent_helper_presence {
                    sent_helper_presence = true;
                    let helper_presence =
                        presence::helper_presence(&helper_jid, valorant_version.as_deref());
                    log_stream_bytes(
                        &context.stream_tx,
                        "ghosty -> client helper presence",
                        helper_presence.as_bytes(),
                    );
                    incoming.write_all(helper_presence.as_bytes())?;
                    let helper_message = presence::helper_chat_message(
                        &helper_jid,
                        &helper_intro_message(
                            &context.enabled,
                            &context.status,
                            &context.auto_accept,
                            &context.auto_accept_state,
                        ),
                    );
                    log_stream_bytes(
                        &context.stream_tx,
                        "ghosty -> client helper message",
                        helper_message.as_bytes(),
                    );
                    incoming.write_all(helper_message.as_bytes())?;
                    sent_helper_intro = true;
                    log(
                        &context.log_tx,
                        LogCategory::Chat,
                        LogLevel::Info,
                        "Sent Ghosty helper status message",
                    );
                } else if helper_friend_enabled
                    && inserted_helper_friend
                    && sent_helper_presence
                    && !sent_helper_intro
                {
                    let helper_message = presence::helper_chat_message(
                        &helper_jid,
                        &helper_intro_message(
                            &context.enabled,
                            &context.status,
                            &context.auto_accept,
                            &context.auto_accept_state,
                        ),
                    );
                    log_stream_bytes(
                        &context.stream_tx,
                        "ghosty -> client helper message",
                        helper_message.as_bytes(),
                    );
                    incoming.write_all(helper_message.as_bytes())?;
                    sent_helper_intro = true;
                    log(
                        &context.log_tx,
                        LogCategory::Chat,
                        LogLevel::Info,
                        "Sent Ghosty helper status message",
                    );
                }
                if !logged_server_flow {
                    logged_server_flow = true;
                    log(
                        &context.log_tx,
                        LogCategory::Chat,
                        LogLevel::Info,
                        "Server-to-client chat bytes are flowing",
                    );
                }
                set_health(&context.health, |health| {
                    health.xmpp_active.state = HealthState::Active;
                    health.xmpp_active.detail = "Riot chat traffic is flowing".to_string();
                    health.active_connections = context.active_connections.load(Ordering::Relaxed);
                });
            }
            StreamRead::Closed => {
                let pending = outgoing_text_buffer.flush();
                if !pending.is_empty() {
                    log_stream_bytes(&context.stream_tx, "ghosty -> client", &pending);
                    incoming.write_all(&pending)?;
                }
                return Err(CleanConnectionClose.into());
            }
            StreamRead::WouldBlock => {}
        }

        if !made_progress && startup_presence_replay.is_ready() {
            let replay = startup_presence_replay.take();
            if !replay.is_empty() {
                log_stream_bytes(
                    &context.stream_tx,
                    "ghosty -> client startup presence replay",
                    &replay,
                );
                incoming.write_all(&replay)?;
                log(
                    &context.log_tx,
                    LogCategory::Chat,
                    LogLevel::Info,
                    format!(
                        "Replayed {} startup presence stanzas after initial roster",
                        startup_presence_replay.replayed_count()
                    ),
                );
                made_progress = true;
            }
        }

        if !made_progress {
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn log_client_hello(stream: &std::net::TcpStream, log_tx: &Sender<LogEntry>) {
    let mut bytes = [0_u8; 1024];
    match stream.peek(&mut bytes) {
        Ok(count) if count > 0 => {
            let first = bytes[0];
            let sni = parse_tls_sni(&bytes[..count]).unwrap_or_else(|| "unknown".to_string());
            log(
                log_tx,
                LogCategory::Chat,
                LogLevel::Info,
                format!("Incoming client hello: first=0x{first:02x}, bytes={count}, sni={sni}"),
            );
        }
        Ok(_) => log(
            log_tx,
            LogCategory::Chat,
            LogLevel::Warn,
            "Incoming connection closed before TLS ClientHello",
        ),
        Err(error) => log(
            log_tx,
            LogCategory::Chat,
            LogLevel::Warn,
            format!("Unable to inspect incoming ClientHello: {error}"),
        ),
    }
}

fn parse_tls_sni(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 5 || bytes[0] != 0x16 {
        return None;
    }
    let mut pos = 5;
    if bytes.len() < pos + 4 || bytes[pos] != 0x01 {
        return None;
    }
    pos += 4;
    pos += 2 + 32;
    if bytes.len() < pos + 1 {
        return None;
    }
    let session_len = bytes[pos] as usize;
    pos += 1 + session_len;
    if bytes.len() < pos + 2 {
        return None;
    }
    let cipher_len = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]) as usize;
    pos += 2 + cipher_len;
    if bytes.len() < pos + 1 {
        return None;
    }
    let compression_len = bytes[pos] as usize;
    pos += 1 + compression_len;
    if bytes.len() < pos + 2 {
        return None;
    }
    let extensions_len = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]) as usize;
    pos += 2;
    let extensions_end = pos.saturating_add(extensions_len).min(bytes.len());

    while pos + 4 <= extensions_end {
        let ext_type = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]);
        let ext_len = u16::from_be_bytes([bytes[pos + 2], bytes[pos + 3]]) as usize;
        pos += 4;
        if pos + ext_len > extensions_end {
            return None;
        }
        if ext_type == 0x0000 {
            return parse_sni_extension(&bytes[pos..pos + ext_len]);
        }
        pos += ext_len;
    }

    None
}

fn parse_sni_extension(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 5 {
        return None;
    }
    let list_len = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
    let mut pos: usize = 2;
    let end = pos.saturating_add(list_len).min(bytes.len());
    while pos + 3 <= end {
        let name_type = bytes[pos];
        let name_len = u16::from_be_bytes([bytes[pos + 1], bytes[pos + 2]]) as usize;
        pos += 3;
        if pos + name_len > end {
            return None;
        }
        if name_type == 0 {
            return String::from_utf8(bytes[pos..pos + name_len].to_vec()).ok();
        }
        pos += name_len;
    }
    None
}

enum StreamRead {
    Data(Vec<u8>),
    WouldBlock,
    Closed,
}

fn read_bytes(
    stream: &mut TlsStream<std::net::TcpStream>,
    buffer: &mut [u8],
) -> Result<StreamRead> {
    match stream.read(buffer) {
        Ok(0) => Ok(StreamRead::Closed),
        Ok(count) => Ok(StreamRead::Data(buffer[..count].to_vec())),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            Ok(StreamRead::WouldBlock)
        }
        Err(error) => Err(error.into()),
    }
}

fn forward_client_chat(
    content: &str,
    bytes: &[u8],
    outgoing: &mut TlsStream<std::net::TcpStream>,
    context: &ChatProxyContext,
    presence_buffer: &mut ClientPresenceBuffer,
    valorant_version: &mut Option<String>,
) -> Result<()> {
    if context.safe_mode.load(Ordering::Relaxed) {
        let pending = presence_buffer.flush();
        if !pending.is_empty() {
            log_stream_bytes(&context.stream_tx, "ghosty -> riot", &pending);
            outgoing.write_all(&pending)?;
        }
        log_stream_bytes(&context.stream_tx, "ghosty -> riot", bytes);
        outgoing.write_all(bytes)?;
        return Ok(());
    }

    let bytes = presence_buffer.push(
        content,
        context.enabled.load(Ordering::Relaxed),
        status_value(&context.status),
        context.connect_to_muc.load(Ordering::Relaxed),
        valorant_version,
    )?;
    if !bytes.is_empty() {
        log_stream_bytes(&context.stream_tx, "ghosty -> riot", &bytes);
        outgoing.write_all(&bytes)?;
    }

    Ok(())
}

#[cfg(test)]
struct HelperFriendInjector {
    pending: String,
    helper_jid: String,
}

#[cfg(test)]
struct HelperInjection {
    bytes: Vec<u8>,
    inserted: bool,
    roster_items_before: Option<usize>,
    roster_items_after: Option<usize>,
}

#[cfg(test)]
impl HelperFriendInjector {
    fn new(helper_jid: String) -> Self {
        Self {
            pending: String::new(),
            helper_jid,
        }
    }

    fn push(&mut self, content: &str) -> HelperInjection {
        self.pending.push_str(content);

        if let Some(updated) = presence::insert_helper_friend(&self.pending, &self.helper_jid) {
            let roster_items_before = presence::roster_item_count(&self.pending);
            let roster_items_after = presence::roster_item_count(&updated);
            self.pending.clear();
            return HelperInjection {
                bytes: updated.into_bytes(),
                inserted: true,
                roster_items_before: Some(roster_items_before),
                roster_items_after: Some(roster_items_after),
            };
        }

        let tail_len = possible_roster_prefix_len(&self.pending);
        if tail_len == self.pending.len() {
            return HelperInjection {
                bytes: Vec::new(),
                inserted: false,
                roster_items_before: None,
                roster_items_after: None,
            };
        }

        let split_at = self.pending.len().saturating_sub(tail_len);
        let tail = self.pending.split_off(split_at);
        let flush = std::mem::replace(&mut self.pending, tail);
        HelperInjection {
            bytes: flush.into_bytes(),
            inserted: false,
            roster_items_before: None,
            roster_items_after: None,
        }
    }

    fn flush(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending).into_bytes()
    }

    fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

struct Utf8StreamBuffer {
    pending: Vec<u8>,
}

enum Utf8Chunk {
    Text { content: String, bytes: Vec<u8> },
    Binary(Vec<u8>),
    Pending,
}

impl Utf8StreamBuffer {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    fn push(&mut self, bytes: Vec<u8>) -> Utf8Chunk {
        let mut combined = std::mem::take(&mut self.pending);
        combined.extend(bytes);

        match std::str::from_utf8(&combined) {
            Ok(content) => Utf8Chunk::Text {
                content: content.to_string(),
                bytes: combined,
            },
            Err(error) if error.error_len().is_none() => {
                let valid_up_to = error.valid_up_to();
                let suffix = combined.split_off(valid_up_to);
                self.pending = suffix;
                if combined.is_empty() {
                    Utf8Chunk::Pending
                } else {
                    let content = String::from_utf8(combined.clone())
                        .expect("valid UTF-8 prefix should decode");
                    Utf8Chunk::Text {
                        content,
                        bytes: combined,
                    }
                }
            }
            Err(_) => Utf8Chunk::Binary(combined),
        }
    }

    fn flush(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    #[cfg(test)]
    fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

struct HelperCommandBuffer {
    pending: String,
    helper_jid: String,
}

#[derive(Default)]
struct PresenceStats {
    batches_logged: usize,
    total_presence: usize,
    total_unavailable: usize,
    total_show_chat: usize,
    total_show_dnd: usize,
    total_show_away: usize,
    total_show_offline: usize,
    total_show_mobile: usize,
    total_league_chat: usize,
    total_league_dnd: usize,
    total_league_away: usize,
    total_league_offline: usize,
    total_league_mobile: usize,
}

struct PresenceBatchStats {
    presence: usize,
    unavailable: usize,
    show_chat: usize,
    show_dnd: usize,
    show_away: usize,
    show_offline: usize,
    show_mobile: usize,
    league_chat: usize,
    league_dnd: usize,
    league_away: usize,
    league_offline: usize,
    league_mobile: usize,
}

impl PresenceStats {
    const MAX_LOGGED_BATCHES: usize = 12;
    const WARMUP_MIN_PRESENCE: usize = 40;
    const WARMUP_MIN_BATCHES: usize = 4;

    fn record_server_batch(&mut self, content: &str) -> Option<String> {
        let batch = PresenceBatchStats::from_content(content);
        if batch.presence == 0 {
            return None;
        }

        self.total_presence += batch.presence;
        self.total_unavailable += batch.unavailable;
        self.total_show_chat += batch.show_chat;
        self.total_show_dnd += batch.show_dnd;
        self.total_show_away += batch.show_away;
        self.total_show_offline += batch.show_offline;
        self.total_show_mobile += batch.show_mobile;
        self.total_league_chat += batch.league_chat;
        self.total_league_dnd += batch.league_dnd;
        self.total_league_away += batch.league_away;
        self.total_league_offline += batch.league_offline;
        self.total_league_mobile += batch.league_mobile;

        if self.batches_logged >= Self::MAX_LOGGED_BATCHES {
            return None;
        }
        self.batches_logged += 1;

        Some(format!(
            "Server presence batch #{batch_no}: batch={presence} unavailable={unavailable} show(chat/dnd/away/offline/mobile)={show_chat}/{show_dnd}/{show_away}/{show_offline}/{show_mobile} league_st(chat/dnd/away/offline/mobile)={league_chat}/{league_dnd}/{league_away}/{league_offline}/{league_mobile}; domains={domains}; totals presence={total_presence} unavailable={total_unavailable}",
            batch_no = self.batches_logged,
            presence = batch.presence,
            unavailable = batch.unavailable,
            show_chat = batch.show_chat,
            show_dnd = batch.show_dnd,
            show_away = batch.show_away,
            show_offline = batch.show_offline,
            show_mobile = batch.show_mobile,
            league_chat = batch.league_chat,
            league_dnd = batch.league_dnd,
            league_away = batch.league_away,
            league_offline = batch.league_offline,
            league_mobile = batch.league_mobile,
            domains = jid_domains(content, "from"),
            total_presence = self.total_presence,
            total_unavailable = self.total_unavailable,
        ))
    }

    fn is_warmup_ready(&self) -> bool {
        self.total_league_presence() > 0
            || self.total_presence >= Self::WARMUP_MIN_PRESENCE
            || self.batches_logged >= Self::WARMUP_MIN_BATCHES
    }

    fn total_league_presence(&self) -> usize {
        self.total_league_chat
            + self.total_league_dnd
            + self.total_league_away
            + self.total_league_offline
            + self.total_league_mobile
    }
}

impl PresenceBatchStats {
    fn from_content(content: &str) -> Self {
        Self {
            presence: count_matches(content, "<presence"),
            unavailable: count_matches(content, "type='unavailable'")
                + count_matches(content, "type=\"unavailable\""),
            show_chat: count_matches(content, "<show>chat</show>"),
            show_dnd: count_matches(content, "<show>dnd</show>"),
            show_away: count_matches(content, "<show>away</show>"),
            show_offline: count_matches(content, "<show>offline</show>"),
            show_mobile: count_matches(content, "<show>mobile</show>"),
            league_chat: count_league_st(content, "chat"),
            league_dnd: count_league_st(content, "dnd"),
            league_away: count_league_st(content, "away"),
            league_offline: count_league_st(content, "offline"),
            league_mobile: count_league_st(content, "mobile"),
        }
    }
}

struct StartupPresenceReplay {
    pending: String,
    replay: Vec<u8>,
    stanzas: usize,
    replayed_stanzas: usize,
    first_seen: Option<Instant>,
    last_seen: Option<Instant>,
    replayed: bool,
}

impl StartupPresenceReplay {
    const MAX_REPLAY_BYTES: usize = 768 * 1024;

    fn new() -> Self {
        Self {
            pending: String::new(),
            replay: Vec::new(),
            stanzas: 0,
            replayed_stanzas: 0,
            first_seen: None,
            last_seen: None,
            replayed: false,
        }
    }

    fn push(&mut self, content: &str) -> usize {
        if self.replayed {
            return 0;
        }

        self.pending.push_str(content);
        let mut added = 0;

        loop {
            let Some(start) = self.pending.find("<presence") else {
                self.trim_non_presence_prefix();
                break;
            };
            if start > 0 {
                self.pending.drain(..start);
            }

            let Some(end) = presence_stanza_end(&self.pending) else {
                break;
            };
            let stanza = self.pending[..end].to_string();
            self.pending.drain(..end);

            if self.replay.len() + stanza.len() > Self::MAX_REPLAY_BYTES {
                self.replayed = true;
                break;
            }

            let now = Instant::now();
            self.first_seen.get_or_insert(now);
            self.last_seen = Some(now);
            self.replay.extend_from_slice(stanza.as_bytes());
            self.stanzas += 1;
            added += 1;
        }

        added
    }

    fn stanza_count(&self) -> usize {
        self.stanzas
    }

    fn replayed_count(&self) -> usize {
        self.replayed_stanzas
    }

    fn replayed(&self) -> bool {
        self.replayed
    }

    fn is_ready(&self) -> bool {
        if self.replayed || self.replay.is_empty() {
            return false;
        }
        let (Some(first_seen), Some(last_seen)) = (self.first_seen, self.last_seen) else {
            return false;
        };

        first_seen.elapsed() >= Duration::from_millis(STARTUP_PRESENCE_REPLAY_MIN_AGE_MS)
            && last_seen.elapsed() >= Duration::from_millis(STARTUP_PRESENCE_REPLAY_IDLE_MS)
    }

    fn take(&mut self) -> Vec<u8> {
        self.replayed = true;
        self.replayed_stanzas = self.stanzas;
        self.pending.clear();
        std::mem::take(&mut self.replay)
    }

    fn trim_non_presence_prefix(&mut self) {
        let keep = partial_marker_tail_len(&self.pending, "<presence");
        if keep == 0 {
            self.pending.clear();
        } else if self.pending.len() > keep {
            let tail = self.pending[self.pending.len() - keep..].to_string();
            self.pending = tail;
        }
    }
}

fn presence_stanza_end(content: &str) -> Option<usize> {
    let open_end = content.find('>')? + 1;
    let opening = &content[..open_end];
    if opening.trim_end().ends_with("/>") {
        return Some(open_end);
    }

    content
        .find("</presence>")
        .map(|end| end + "</presence>".len())
}

enum HelperCommandResult {
    NotHelper,
    Pending,
    Complete {
        reply: Option<String>,
        passthrough: Vec<u8>,
    },
}

impl HelperCommandBuffer {
    const MAX_PENDING_BYTES: usize = 64 * 1024;
    const MESSAGE_MARKER: &'static str = "<message";

    fn new(helper_jid: String) -> Self {
        Self {
            pending: String::new(),
            helper_jid,
        }
    }

    #[cfg(test)]
    fn push(
        &mut self,
        content: &str,
        enabled: &Arc<AtomicBool>,
        status: &Arc<Mutex<PresenceStatus>>,
    ) -> Result<HelperCommandResult> {
        let auto_accept = Arc::new(AtomicBool::new(false));
        let auto_accept_state = Arc::new(Mutex::new("Disabled".to_string()));
        self.push_with_auto_accept(content, enabled, status, &auto_accept, &auto_accept_state)
    }

    fn push_with_auto_accept(
        &mut self,
        content: &str,
        enabled: &Arc<AtomicBool>,
        status: &Arc<Mutex<PresenceStatus>>,
        auto_accept: &Arc<AtomicBool>,
        auto_accept_state: &Arc<Mutex<String>>,
    ) -> Result<HelperCommandResult> {
        if self.pending.is_empty()
            && !presence::contains_helper_message(content, &self.helper_jid)
            && !should_buffer_message_fragment(content)
        {
            return Ok(HelperCommandResult::NotHelper);
        }

        self.pending.push_str(content);
        if let Some(intercept) = helper_command_intercept_from_buffer(
            &self.pending,
            enabled,
            status,
            auto_accept,
            auto_accept_state,
            &self.helper_jid,
        )? {
            self.pending.clear();
            return Ok(HelperCommandResult::Complete {
                reply: intercept.reply,
                passthrough: intercept.passthrough.into_bytes(),
            });
        }

        if self.pending.contains("</message>")
            || self.pending.len() > Self::MAX_PENDING_BYTES
            || !helper_command_buffer_should_wait(&self.pending)
        {
            let passthrough = self.flush_pending_if_complete_without_command();
            return Ok(HelperCommandResult::Complete {
                reply: None,
                passthrough,
            });
        }

        Ok(HelperCommandResult::Pending)
    }

    fn clear(&mut self) {
        self.pending.clear();
    }

    fn flush_pending_if_complete_without_command(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending).into_bytes()
    }
}

struct ClientPresenceBuffer {
    pending: String,
    settings: Option<BufferedPresenceSettings>,
    initial_presence_forwarded: bool,
    warmup_masked_presence: Option<Vec<u8>>,
}

#[derive(Clone, Copy)]
struct BufferedPresenceSettings {
    target_status: PresenceStatus,
    connect_to_muc: bool,
}

impl ClientPresenceBuffer {
    const MAX_PENDING_BYTES: usize = 64 * 1024;
    const PRESENCE_MARKER: &'static str = "<presence";

    fn new() -> Self {
        Self {
            pending: String::new(),
            settings: None,
            initial_presence_forwarded: false,
            warmup_masked_presence: None,
        }
    }

    fn push(
        &mut self,
        content: &str,
        enabled: bool,
        target_status: PresenceStatus,
        connect_to_muc: bool,
        valorant_version: &mut Option<String>,
    ) -> Result<Vec<u8>> {
        if !enabled {
            return Ok(self.push_or_flush_raw(content));
        }

        if self.pending.is_empty() && !should_buffer_presence_fragment(content) {
            return Ok(content.as_bytes().to_vec());
        }

        if self.pending.is_empty() {
            self.settings = Some(BufferedPresenceSettings {
                target_status,
                connect_to_muc,
            });
        }
        self.pending.push_str(content);
        let settings = self.settings.unwrap_or(BufferedPresenceSettings {
            target_status,
            connect_to_muc,
        });
        if settings.target_status != PresenceStatus::Chat
            && !self.initial_presence_forwarded
            && presence::contains_unaddressed_presence_fragment(&self.pending)
        {
            self.initial_presence_forwarded = true;
            self.warmup_masked_presence = presence::rewrite_unaddressed_presence_only_fragment(
                &self.pending,
                settings.target_status,
                valorant_version,
            )?;
            return Ok(self.flush());
        }

        if let Some(rewrite) = presence::rewrite_presence_fragment(
            &self.pending,
            true,
            settings.target_status,
            settings.connect_to_muc,
            valorant_version,
        )? {
            self.clear();
            return Ok(match rewrite {
                presence::PresenceRewrite::Forward(rewritten) => rewritten.into_bytes(),
                presence::PresenceRewrite::Drop => Vec::new(),
            });
        }

        if presence_buffer_should_wait(&self.pending) {
            return Ok(Vec::new());
        }

        Ok(self.flush())
    }

    fn push_or_flush_raw(&mut self, content: &str) -> Vec<u8> {
        if self.pending.is_empty() {
            content.as_bytes().to_vec()
        } else {
            self.pending.push_str(content);
            self.flush()
        }
    }

    fn flush(&mut self) -> Vec<u8> {
        self.settings = None;
        std::mem::take(&mut self.pending).into_bytes()
    }

    fn clear(&mut self) {
        self.pending.clear();
        self.settings = None;
    }

    fn take_warmup_masked_presence(&mut self) -> Option<Vec<u8>> {
        self.warmup_masked_presence.take()
    }

    #[cfg(test)]
    fn has_warmup_masked_presence(&self) -> bool {
        self.warmup_masked_presence.is_some()
    }
}

fn should_buffer_presence_fragment(content: &str) -> bool {
    content.contains(ClientPresenceBuffer::PRESENCE_MARKER)
        || partial_marker_tail_len(content, ClientPresenceBuffer::PRESENCE_MARKER) > 0
}

fn presence_buffer_should_wait(content: &str) -> bool {
    content.len() <= ClientPresenceBuffer::MAX_PENDING_BYTES
        && (content.contains(ClientPresenceBuffer::PRESENCE_MARKER)
            || partial_marker_tail_len(content, ClientPresenceBuffer::PRESENCE_MARKER) > 0)
        && !content.contains("</presence>")
}

fn should_buffer_message_fragment(content: &str) -> bool {
    partial_marker_tail_len(content, HelperCommandBuffer::MESSAGE_MARKER) > 0
        || (content.contains(HelperCommandBuffer::MESSAGE_MARKER)
            && !content.contains("</message>"))
}

fn helper_command_buffer_should_wait(content: &str) -> bool {
    content.len() <= HelperCommandBuffer::MAX_PENDING_BYTES
        && (content.contains(HelperCommandBuffer::MESSAGE_MARKER)
            || partial_marker_tail_len(content, HelperCommandBuffer::MESSAGE_MARKER) > 0)
        && !content.contains("</message>")
}

fn partial_marker_tail_len(content: &str, marker: &str) -> usize {
    let max = content.len().min(marker.len().saturating_sub(1));
    for len in (1..=max).rev() {
        if content.ends_with(&marker[..len]) {
            return len;
        }
    }
    0
}

fn count_matches(content: &str, needle: &str) -> usize {
    content.match_indices(needle).count()
}

fn count_league_st(content: &str, status: &str) -> usize {
    let needle = format!("<league_of_legends><st>{status}</st>");
    count_matches(content, &needle)
}

fn jid_domain_summary(content: &str, attribute: &str, label: &str) -> Option<String> {
    let domains = jid_domains(content, attribute);
    (domains != "none").then(|| format!("{label}: {domains}"))
}

fn jid_domains(content: &str, attribute: &str) -> String {
    let domains = jid_domain_counts(content, attribute);

    if domains.is_empty() {
        return "none".to_string();
    }

    domains
        .into_iter()
        .map(|(domain, count)| format!("{domain}={count}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn jid_domain_map(content: &str, attribute: &str) -> std::collections::BTreeMap<String, String> {
    let mut domains = std::collections::BTreeMap::<String, String>::new();
    collect_jid_domain_map(content, &format!("{attribute}='"), &mut domains);
    collect_jid_domain_map(content, &format!("{attribute}=\""), &mut domains);
    domains
}

fn jid_domain_counts(content: &str, attribute: &str) -> std::collections::BTreeMap<String, usize> {
    let mut domains = std::collections::BTreeMap::<String, usize>::new();
    collect_jid_domains(content, &format!("{attribute}='"), &mut domains);
    collect_jid_domains(content, &format!("{attribute}=\""), &mut domains);
    domains
}

fn collect_jid_domain_map(
    content: &str,
    marker: &str,
    domains: &mut std::collections::BTreeMap<String, String>,
) {
    let quote = marker
        .as_bytes()
        .last()
        .copied()
        .map(char::from)
        .unwrap_or('\'');
    let mut offset = 0;
    while let Some(found) = content[offset..].find(marker) {
        let value_start = offset + found + marker.len();
        let value = &content[value_start..];
        let Some(value_end) = value.find(quote) else {
            return;
        };
        if let Some((local, domain)) = jid_parts(&value[..value_end]) {
            domains
                .entry(local.to_string())
                .or_insert(domain.to_string());
        }
        offset = value_start + value_end + 1;
    }
}

fn collect_jid_domains(
    content: &str,
    marker: &str,
    domains: &mut std::collections::BTreeMap<String, usize>,
) {
    let quote = marker
        .as_bytes()
        .last()
        .copied()
        .map(char::from)
        .unwrap_or('\'');
    let mut offset = 0;
    while let Some(found) = content[offset..].find(marker) {
        let value_start = offset + found + marker.len();
        let value = &content[value_start..];
        let Some(value_end) = value.find(quote) else {
            return;
        };
        if let Some(domain) = jid_domain(&value[..value_end]) {
            *domains.entry(domain.to_string()).or_default() += 1;
        }
        offset = value_start + value_end + 1;
    }
}

fn rewrite_jid_attribute_domains_from_map(
    content: &str,
    attribute: &str,
    roster_domains: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    let mut output = String::with_capacity(content.len());
    let mut remaining = content;
    let mut changed = false;

    while let Some((start, quote, marker_len)) = next_attribute_marker(remaining, attribute) {
        output.push_str(&remaining[..start]);
        output.push_str(&remaining[start..start + marker_len]);
        let value_start = start + marker_len;
        let after_marker = &remaining[value_start..];
        let Some(value_end) = after_marker.find(quote) else {
            output.push_str(after_marker);
            return changed.then_some(output);
        };
        let value = &after_marker[..value_end];
        if let Some(rewritten) = rewrite_jid_domain_from_map(value, roster_domains) {
            output.push_str(&rewritten);
            changed = true;
        } else {
            output.push_str(value);
        }
        output.push(quote);
        remaining = &after_marker[value_end + quote.len_utf8()..];
    }

    output.push_str(remaining);
    changed.then_some(output)
}

fn next_attribute_marker(content: &str, attribute: &str) -> Option<(usize, char, usize)> {
    let single = format!("{attribute}='");
    let double = format!("{attribute}=\"");
    match (content.find(&single), content.find(&double)) {
        (Some(single_at), Some(double_at)) if single_at <= double_at => {
            Some((single_at, '\'', single.len()))
        }
        (Some(_), Some(double_at)) => Some((double_at, '"', double.len())),
        (Some(single_at), None) => Some((single_at, '\'', single.len())),
        (None, Some(double_at)) => Some((double_at, '"', double.len())),
        (None, None) => None,
    }
}

fn rewrite_jid_domain_from_map(
    jid: &str,
    roster_domains: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    let (local, current_domain) = jid_parts(jid)?;
    let target_domain = roster_domains.get(local)?;
    if current_domain == target_domain
        || current_domain == "conference.pvp.net"
        || !current_domain.ends_with(".pvp.net")
        || !target_domain.ends_with(".pvp.net")
    {
        return None;
    }

    let domain_start = jid.find('@')? + 1;
    let domain = &jid[domain_start..];
    let domain_end = domain.find('/').unwrap_or(domain.len());
    let mut rewritten = String::with_capacity(jid.len() + target_domain.len());
    rewritten.push_str(&jid[..domain_start]);
    rewritten.push_str(target_domain);
    rewritten.push_str(&domain[domain_end..]);
    Some(rewritten)
}

fn jid_parts(jid: &str) -> Option<(&str, &str)> {
    let domain_start = jid.find('@')?;
    let local = &jid[..domain_start];
    if local.is_empty() {
        return None;
    }
    let domain = &jid[domain_start + 1..];
    let domain_end = domain.find('/').unwrap_or(domain.len());
    let domain = &domain[..domain_end];
    (!domain.is_empty()).then_some((local, domain))
}

fn jid_domain(jid: &str) -> Option<&str> {
    let domain_start = jid.find('@')? + 1;
    let domain = &jid[domain_start..];
    let domain_end = domain.find('/').unwrap_or(domain.len());
    let domain = &domain[..domain_end];
    (!domain.is_empty()).then_some(domain)
}

#[cfg(test)]
fn possible_roster_prefix_len(content: &str) -> usize {
    if let Some(query_at) = content.rfind("<query") {
        let tail = &content[query_at..];
        if !tail.contains('>') || tail.contains(presence::ROSTER_NAMESPACE) {
            return tail.len();
        }
    }

    let marker = "<query";
    let max = content.len().min(marker.len().saturating_sub(1));
    for len in (1..=max).rev() {
        if content.ends_with(&marker[..len]) {
            return len;
        }
    }
    0
}

#[cfg(test)]
fn helper_command_reply(
    content: &str,
    enabled: &Arc<AtomicBool>,
    status: &Arc<Mutex<PresenceStatus>>,
) -> Result<Option<String>> {
    let auto_accept = Arc::new(AtomicBool::new(false));
    let auto_accept_state = Arc::new(Mutex::new("Disabled".to_string()));
    let Some(command) = helper_command_text(content) else {
        return Ok(None);
    };
    helper_command_reply_for_text(&command, enabled, status, &auto_accept, &auto_accept_state)
}

struct HelperCommandIntercept {
    reply: Option<String>,
    passthrough: String,
}

fn helper_command_intercept_from_buffer(
    content: &str,
    enabled: &Arc<AtomicBool>,
    status: &Arc<Mutex<PresenceStatus>>,
    auto_accept: &Arc<AtomicBool>,
    auto_accept_state: &Arc<Mutex<String>>,
    helper_jid: &str,
) -> Result<Option<HelperCommandIntercept>> {
    let Some(extract) = helper_command_extract_to(content, helper_jid) else {
        return Ok(None);
    };
    let reply = extract
        .command
        .as_deref()
        .map(|command| {
            helper_command_reply_for_text(command, enabled, status, auto_accept, auto_accept_state)
        })
        .transpose()?
        .flatten();
    Ok(Some(HelperCommandIntercept {
        reply,
        passthrough: extract.passthrough,
    }))
}

fn helper_command_reply_for_text(
    command: &str,
    enabled: &Arc<AtomicBool>,
    status: &Arc<Mutex<PresenceStatus>>,
    auto_accept: &Arc<AtomicBool>,
    auto_accept_state: &Arc<Mutex<String>>,
) -> Result<Option<String>> {
    let Some(command) = helper_command_kind(command) else {
        return Ok(None);
    };

    if command == "offline" {
        set_helper_status(status, PresenceStatus::Offline)?;
        Ok(Some("You are now appearing offline.".to_string()))
    } else if command == "mobile" {
        set_helper_status(status, PresenceStatus::Mobile)?;
        Ok(Some("You are now appearing mobile.".to_string()))
    } else if command == "online" {
        set_helper_status(status, PresenceStatus::Chat)?;
        Ok(Some("You are now appearing online.".to_string()))
    } else if command == "enable" {
        enabled.store(true, Ordering::Relaxed);
        Ok(Some("Ghosty is now enabled.".to_string()))
    } else if command == "disable" {
        enabled.store(false, Ordering::Relaxed);
        Ok(Some("Ghosty is now disabled.".to_string()))
    } else if command == "auto_accept_on" {
        persistence::write_auto_accept(true)?;
        auto_accept.store(true, Ordering::Relaxed);
        set_auto_accept_state(auto_accept_state, "Watching for ready check");
        Ok(Some("Auto accept is now enabled.".to_string()))
    } else if command == "auto_accept_off" {
        persistence::write_auto_accept(false)?;
        auto_accept.store(false, Ordering::Relaxed);
        set_auto_accept_state(auto_accept_state, "Disabled");
        Ok(Some("Auto accept is now disabled.".to_string()))
    } else if command == "auto_accept_status" {
        Ok(Some(helper_auto_accept_message(
            auto_accept,
            auto_accept_state,
        )?))
    } else if command == "opgg" {
        Ok(Some(current_user_opgg_link()?))
    } else if command == "opgg_multi" {
        Ok(Some(current_lobby_opgg_multisearch_link()?))
    } else if command == "friends_summary" {
        Ok(Some(current_friends_summary()?))
    } else if command == "status" {
        Ok(Some(helper_status_message(
            enabled,
            status,
            auto_accept,
            auto_accept_state,
        )?))
    } else if command == "help" {
        Ok(Some(helper_command_help_message().to_string()))
    } else {
        Ok(None)
    }
}

fn helper_command_kind(command: &str) -> Option<&'static str> {
    let command = command.to_ascii_lowercase();
    if contains_command_phrase(&command, &["auto", "accept"]) {
        if contains_command_word(&command, "off") || contains_command_word(&command, "disable") {
            return Some("auto_accept_off");
        }
        if contains_command_word(&command, "status") {
            return Some("auto_accept_status");
        }
        return Some("auto_accept_on");
    }
    if contains_command_word(&command, "friend") || contains_command_word(&command, "friends") {
        return Some("friends_summary");
    }
    if contains_command_word(&command, "multi")
        || contains_command_phrase(&command, &["lobby", "link"])
        || contains_command_phrase(&command, &["lobby", "opgg"])
        || contains_command_phrase(&command, &["lobby", "op", "gg"])
    {
        return Some("opgg_multi");
    }
    if contains_command_word(&command, "opgg")
        || contains_command_word(&command, "op")
        || contains_command_phrase(&command, &["op", "gg"])
    {
        return Some("opgg");
    }

    const COMMANDS: [&str; 7] = [
        "offline", "mobile", "online", "disable", "enable", "status", "help",
    ];
    COMMANDS
        .into_iter()
        .find(|keyword| contains_command_word(&command, keyword))
}

fn contains_command_phrase(command: &str, words: &[&str]) -> bool {
    let command_words = command
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    words.iter().all(|word| {
        command_words
            .iter()
            .any(|command_word| command_word == word)
    })
}

fn contains_command_word(command: &str, keyword: &str) -> bool {
    command
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|word| word == keyword)
}

fn helper_intro_message(
    enabled: &Arc<AtomicBool>,
    status: &Arc<Mutex<PresenceStatus>>,
    auto_accept: &Arc<AtomicBool>,
    auto_accept_state: &Arc<Mutex<String>>,
) -> String {
    helper_status_message(enabled, status, auto_accept, auto_accept_state)
        .unwrap_or_else(|_| format!("Ghosty is running. {}", helper_command_help_message()))
}

fn helper_status_message(
    enabled: &Arc<AtomicBool>,
    status: &Arc<Mutex<PresenceStatus>>,
    auto_accept: &Arc<AtomicBool>,
    auto_accept_state: &Arc<Mutex<String>>,
) -> Result<String> {
    let status = status
        .lock()
        .map_err(|e| anyhow!(e.to_string()))
        .map(|status| helper_status_label(*status))?;
    let masking = if enabled.load(Ordering::Relaxed) {
        "enabled"
    } else {
        "disabled"
    };
    let auto_accept = helper_auto_accept_message(auto_accept, auto_accept_state)?;
    Ok(format!(
        "You are appearing {status}. Presence masking is {masking}. {auto_accept} {}",
        helper_command_help_message()
    ))
}

fn helper_command_help_message() -> &'static str {
    "Send online, offline, mobile, enable, disable, status, friends, auto accept on, auto accept off, auto accept status, opgg, or opgg multi."
}

fn helper_auto_accept_message(
    auto_accept: &Arc<AtomicBool>,
    auto_accept_state: &Arc<Mutex<String>>,
) -> Result<String> {
    let enabled = if auto_accept.load(Ordering::Relaxed) {
        "enabled"
    } else {
        "disabled"
    };
    let state = auto_accept_state
        .lock()
        .map_err(|e| anyhow!(e.to_string()))?
        .clone();
    Ok(format!("Auto accept is {enabled}. Client state: {state}."))
}

#[cfg(not(test))]
fn current_user_opgg_link() -> Result<String> {
    lcu_api::current_summoner_opgg_link()
}

#[cfg(not(test))]
fn current_lobby_opgg_multisearch_link() -> Result<String> {
    lcu_api::current_lobby_opgg_multisearch_link()
}

#[cfg(not(test))]
fn current_friends_summary() -> Result<String> {
    lcu_api::current_friends_summary()
}

#[cfg(test)]
fn current_user_opgg_link() -> Result<String> {
    Ok("https://www.op.gg/summoners/na/Ghosty-NA1".to_string())
}

#[cfg(test)]
fn current_lobby_opgg_multisearch_link() -> Result<String> {
    Ok("https://op.gg/lol/multisearch/na?summoners=Ghosty%23NA1%2CDuo%23NA2".to_string())
}

#[cfg(test)]
fn current_friends_summary() -> Result<String> {
    Ok(
        "Friends: 3 total. Statuses: online 1, mobile 1, offline 1. Products: League 1, Riot Mobile 1, Unknown 1."
            .to_string(),
    )
}

fn helper_status_label(status: PresenceStatus) -> &'static str {
    match status {
        PresenceStatus::Chat => "online",
        PresenceStatus::Offline => "offline",
        PresenceStatus::Mobile => "mobile",
    }
}

fn set_helper_status(status: &Arc<Mutex<PresenceStatus>>, next: PresenceStatus) -> Result<()> {
    persistence::write_session_status(next)?;
    *status.lock().map_err(|e| anyhow!(e.to_string()))? = next;
    Ok(())
}

fn status_value(status: &Arc<Mutex<PresenceStatus>>) -> PresenceStatus {
    status
        .lock()
        .map(|status| *status)
        .unwrap_or(PresenceStatus::Offline)
}

#[cfg(test)]
fn helper_command_text(content: &str) -> Option<String> {
    let direct = xmltree::Element::parse(std::io::Cursor::new(content.as_bytes())).ok();
    if let Some(text) = direct.as_ref().and_then(message_body_text) {
        return Some(text);
    }

    let wrapped = format!("<xml>{content}</xml>");
    let root = xmltree::Element::parse_with_config(
        std::io::Cursor::new(wrapped.as_bytes()),
        xmltree::ParserConfig::new().whitespace_to_characters(true),
    )
    .ok()?;
    root.children.iter().find_map(|node| {
        let xmltree::XMLNode::Element(element) = node else {
            return None;
        };
        message_body_text(element)
    })
}

struct HelperCommandExtract {
    command: Option<String>,
    passthrough: String,
}

fn helper_command_extract_to(content: &str, helper_jid: &str) -> Option<HelperCommandExtract> {
    let direct = xmltree::Element::parse(std::io::Cursor::new(content.as_bytes())).ok();
    if let Some(command) = direct
        .as_ref()
        .and_then(|element| addressed_message_body_text(element, helper_jid))
    {
        return Some(HelperCommandExtract {
            command: Some(command),
            passthrough: String::new(),
        });
    }
    if direct
        .as_ref()
        .is_some_and(|element| is_addressed_helper_message(element, helper_jid))
    {
        return Some(HelperCommandExtract {
            command: None,
            passthrough: String::new(),
        });
    }

    let wrapped = format!("<xml>{content}</xml>");
    let root = xmltree::Element::parse_with_config(
        std::io::Cursor::new(wrapped.as_bytes()),
        xmltree::ParserConfig::new().whitespace_to_characters(true),
    )
    .ok()?;
    let mut command = None;
    let mut found_helper_message = false;
    let mut passthrough = String::new();

    for node in &root.children {
        let xmltree::XMLNode::Element(element) = node else {
            append_xml_node_text(node, &mut passthrough);
            continue;
        };
        if is_addressed_helper_message(element, helper_jid) {
            found_helper_message = true;
            if command.is_none() {
                command = message_body_text(element);
            }
            continue;
        }
        passthrough.push_str(&serialize_xml_element(element)?);
    }

    found_helper_message.then_some(HelperCommandExtract {
        command,
        passthrough,
    })
}

fn addressed_message_body_text(element: &xmltree::Element, helper_jid: &str) -> Option<String> {
    if !is_addressed_helper_message(element, helper_jid) {
        return None;
    }
    message_body_text(element)
}

fn is_addressed_helper_message(element: &xmltree::Element, helper_jid: &str) -> bool {
    if element.name != "message" {
        return false;
    }
    let Some(to) = element.attributes.get("to") else {
        return false;
    };
    to == helper_jid || to.starts_with(&format!("{helper_jid}/"))
}

fn serialize_xml_element(element: &xmltree::Element) -> Option<String> {
    let mut out = Vec::new();
    element
        .write_with_config(
            &mut out,
            xmltree::EmitterConfig::new()
                .write_document_declaration(false)
                .perform_indent(false),
        )
        .ok()?;
    String::from_utf8(out).ok()
}

fn append_xml_node_text(node: &xmltree::XMLNode, output: &mut String) {
    match node {
        xmltree::XMLNode::Text(text) | xmltree::XMLNode::CData(text) => output.push_str(text),
        _ => {}
    }
}

fn message_body_text(element: &xmltree::Element) -> Option<String> {
    if element.name != "message" {
        return None;
    }
    element
        .get_child("body")
        .and_then(|body| body.get_text())
        .map(|text| text.into_owned())
}

fn proxy_identity(log_tx: &Sender<LogEntry>) -> Result<Identity> {
    if let Some(bytes) = persistence::read_certificate() {
        match Identity::from_pkcs12(&bytes, "") {
            Ok(identity) => return Ok(identity),
            Err(error) => log(
                log_tx,
                LogCategory::System,
                LogLevel::Warn,
                format!("Cached localhost proxy certificate is invalid; redownloading: {error}"),
            ),
        }
    }

    let bytes = download_proxy_certificate(log_tx)?;
    Identity::from_pkcs12(&bytes, "").context("Unable to decode localhost.pfx")
}

fn download_proxy_certificate(log_tx: &Sender<LogEntry>) -> Result<Vec<u8>> {
    log(
        log_tx,
        LogCategory::System,
        LogLevel::Info,
        "Downloading localhost proxy certificate",
    );
    let bytes = reqwest::blocking::Client::new()
        .get("https://mln.cx/deceive/localhost.pfx")
        .header("User-Agent", "Ghosty/0.1.0")
        .send()?
        .error_for_status()?
        .bytes()?
        .to_vec();
    persistence::write_certificate(&bytes)?;
    Ok(bytes)
}

fn pump_logs(rx: mpsc::Receiver<LogEntry>, logs: Arc<Mutex<Vec<LogEntry>>>) {
    thread::spawn(move || {
        while let Ok(line) = rx.recv() {
            if let Ok(mut logs) = logs.lock() {
                logs.push(line);
                keep_recent(&mut logs);
            }
        }
    });
}

fn pump_stream_events(rx: mpsc::Receiver<StreamEvent>, events: Arc<Mutex<Vec<StreamEvent>>>) {
    thread::spawn(move || {
        while let Ok(event) = rx.recv() {
            if let Ok(mut events) = events.lock() {
                events.push(event);
                keep_recent_stream_events(&mut events);
            }
        }
    });
}

fn keep_recent(logs: &mut Vec<LogEntry>) {
    if logs.len() > LOG_BUFFER_LIMIT {
        let extra = logs.len() - LOG_BUFFER_LIMIT;
        logs.drain(0..extra);
    }
}

fn keep_recent_stream_events(events: &mut Vec<StreamEvent>) {
    if events.len() > STREAM_EVENT_BUFFER_LIMIT {
        let extra = events.len() - STREAM_EVENT_BUFFER_LIMIT;
        events.drain(0..extra);
    }
}

fn log(tx: &Sender<LogEntry>, category: LogCategory, level: LogLevel, message: impl Into<String>) {
    let _ = tx.send(LogEntry::new(category, level, message.into()));
}

fn log_stream_bytes(tx: &Sender<StreamEvent>, direction: &str, bytes: &[u8]) {
    let preview = stream_preview(bytes);
    let _ = tx.send(StreamEvent::new(direction, bytes.len(), preview));
}

fn stream_preview(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "(empty)".to_string();
    }

    let text = String::from_utf8_lossy(bytes);
    let sanitized = sanitize_stream_text(&text);
    let mut preview: String = sanitized.chars().take(STREAM_EVENT_PREVIEW_CHARS).collect();
    if sanitized.chars().count() > STREAM_EVENT_PREVIEW_CHARS {
        preview.push_str(" ... [truncated]");
    }
    preview
}

fn sanitize_stream_text(text: &str) -> String {
    let mut sanitized = text
        .replace('\r', "\\r")
        .replace('\n', "\\n")
        .replace('\t', "\\t");
    sanitized = redact_xml_element(&sanitized, "auth");
    sanitized = redact_xml_element(&sanitized, "password");
    sanitized = redact_xml_element(&sanitized, "token");
    sanitized = redact_attribute(&sanitized, "password");
    sanitized = redact_attribute(&sanitized, "token");
    sanitized = redact_attribute(&sanitized, "access_token");
    sanitized
}

fn redact_xml_element(text: &str, tag: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut remaining = text;
    let open_marker = format!("<{tag}");
    let close_marker = format!("</{tag}>");

    while let Some(start) = remaining.find(&open_marker) {
        output.push_str(&remaining[..start]);
        let after_start = &remaining[start..];
        let Some(open_end) = after_start.find('>') else {
            output.push_str(after_start);
            return output;
        };
        let element_start = start + open_end + 1;
        output.push_str(&remaining[start..element_start]);
        let after_open = &remaining[element_start..];
        if let Some(close_start) = after_open.find(&close_marker) {
            output.push_str("[redacted]");
            output.push_str(&after_open[close_start..close_start + close_marker.len()]);
            remaining = &after_open[close_start + close_marker.len()..];
        } else {
            output.push_str("[redacted]");
            return output;
        }
    }

    output.push_str(remaining);
    output
}

fn redact_attribute(text: &str, attr: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut remaining = text;

    loop {
        let Some(start) = find_attribute_start(remaining, attr) else {
            output.push_str(remaining);
            return output;
        };
        output.push_str(&remaining[..start]);
        let after_start = &remaining[start..];
        let Some(eq_index) = after_start.find('=') else {
            output.push_str(after_start);
            return output;
        };
        let after_eq = &after_start[eq_index + 1..];
        let Some(quote) = after_eq
            .chars()
            .next()
            .filter(|ch| *ch == '"' || *ch == '\'')
        else {
            output.push_str(&after_start[..=eq_index]);
            remaining = after_eq;
            continue;
        };
        let quote_len = quote.len_utf8();
        let value_start = eq_index + 1 + quote_len;
        output.push_str(&after_start[..value_start]);
        if let Some(value_end) = after_start[value_start..].find(quote) {
            output.push_str("[redacted]");
            output.push(quote);
            remaining = &after_start[value_start + value_end + quote_len..];
        } else {
            output.push_str("[redacted]");
            return output;
        }
    }
}

fn find_attribute_start(text: &str, attr: &str) -> Option<usize> {
    let attr_lower = attr.to_ascii_lowercase();
    let text_lower = text.to_ascii_lowercase();
    let mut offset = 0;

    while let Some(found) = text_lower[offset..].find(&attr_lower) {
        let index = offset + found;
        let before = text[..index].chars().next_back();
        let after = text[index + attr.len()..].chars().next();
        if before.is_none_or(|ch| ch.is_whitespace() || ch == '<')
            && after.is_some_and(|ch| ch.is_whitespace() || ch == '=')
        {
            return Some(index);
        }
        offset = index + attr.len();
    }

    None
}

fn set_health(health: &Arc<Mutex<ConnectionHealth>>, update: impl FnOnce(&mut ConnectionHealth)) {
    if let Ok(mut health) = health.lock() {
        update(&mut health);
    }
}

fn check_riot_client() -> PreflightCheck {
    match riot::riot_client_path() {
        Some(path) => PreflightCheck {
            label: "Riot Client".to_string(),
            ok: true,
            detail: path.display().to_string(),
        },
        None => PreflightCheck {
            label: "Riot Client".to_string(),
            ok: false,
            detail: "RiotClientServices.exe was not found".to_string(),
        },
    }
}

fn check_localhost_resolution() -> PreflightCheck {
    let resolves = (LOCALHOST_DOMAIN, 443)
        .to_socket_addrs()
        .map(|addrs| addrs.into_iter().any(|addr| addr.ip().is_loopback()))
        .unwrap_or(false);
    PreflightCheck {
        label: "Localhost DNS".to_string(),
        ok: resolves,
        detail: if resolves {
            format!("{LOCALHOST_DOMAIN} resolves to loopback")
        } else {
            format!("{LOCALHOST_DOMAIN} does not resolve to 127.0.0.1")
        },
    }
}

fn check_certificate() -> PreflightCheck {
    certificate_check(persistence::read_certificate().as_deref())
}

fn certificate_check(bytes: Option<&[u8]>) -> PreflightCheck {
    match bytes {
        Some(bytes) if Identity::from_pkcs12(bytes, "").is_ok() => PreflightCheck {
            label: "Proxy Certificate".to_string(),
            ok: true,
            detail: "Cached localhost certificate loads correctly".to_string(),
        },
        Some(_) => PreflightCheck {
            label: "Proxy Certificate".to_string(),
            ok: false,
            detail: "Cached localhost certificate is invalid; Ghosty will redownload it on start"
                .to_string(),
        },
        None => PreflightCheck {
            label: "Proxy Certificate".to_string(),
            ok: false,
            detail: "Certificate is not cached yet; start Ghosty once with internet access to download it".to_string(),
        },
    }
}

fn check_port_available() -> PreflightCheck {
    let ok = TcpListener::bind(("127.0.0.1", 0)).is_ok();
    PreflightCheck {
        label: "Local Ports".to_string(),
        ok,
        detail: if ok {
            "A local proxy port can be allocated".to_string()
        } else {
            "Unable to bind a local proxy port".to_string()
        },
    }
}

fn check_riot_processes() -> PreflightCheck {
    match riot::running_riot_processes() {
        Ok(processes) if processes.is_empty() => PreflightCheck {
            label: "Riot Processes".to_string(),
            ok: true,
            detail: "No Riot processes detected".to_string(),
        },
        Ok(processes) => PreflightCheck {
            label: "Riot Processes".to_string(),
            ok: false,
            detail: format!(
                "Riot is already running ({}); use Clean Restart before launching",
                processes.join(", ")
            ),
        },
        Err(error) => PreflightCheck {
            label: "Riot Processes".to_string(),
            ok: false,
            detail: format!("Unable to check Riot processes: {error:#}"),
        },
    }
}

impl Default for ConnectionHealth {
    fn default() -> Self {
        Self {
            config_proxy: HealthStep::new("configProxy", "Config proxy"),
            config_patched: HealthStep::new("configPatched", "Config patched"),
            chat_server: HealthStep::new("chatServer", "Chat server"),
            tls_connected: HealthStep::new("tlsConnected", "TLS connected"),
            xmpp_active: HealthStep::new("xmppActive", "XMPP active"),
            active_connections: 0,
            reconnect_attempts: 0,
        }
    }
}

impl HealthStep {
    fn new(key: &str, label: &str) -> Self {
        Self {
            key: key.to_string(),
            label: label.to_string(),
            state: HealthState::Waiting,
            detail: "Waiting".to_string(),
        }
    }
}

impl LogEntry {
    fn new(category: LogCategory, level: LogLevel, message: String) -> Self {
        Self {
            timestamp: chrono::Utc::now().format("%H:%M:%S").to_string(),
            category,
            level,
            message,
        }
    }
}

impl StreamEvent {
    fn new(direction: &str, bytes: usize, preview: String) -> Self {
        Self {
            timestamp: chrono::Utc::now().format("%H:%M:%S").to_string(),
            direction: direction.to_string(),
            bytes,
            preview,
        }
    }
}

struct ActiveConnectionGuard {
    active_connections: Arc<AtomicUsize>,
    health: Arc<Mutex<ConnectionHealth>>,
}

impl Drop for ActiveConnectionGuard {
    fn drop(&mut self) {
        self.active_connections.fetch_sub(1, Ordering::Relaxed);
        set_health(&self.health, |health| {
            health.active_connections = self.active_connections.load(Ordering::Relaxed);
            if health.active_connections == 0 {
                if health.tls_connected.state == HealthState::Active {
                    health.tls_connected.state = HealthState::Ready;
                    health.tls_connected.detail = "Waiting for Riot Client connection".to_string();
                }
                if health.xmpp_active.state == HealthState::Active {
                    health.xmpp_active.state = HealthState::Ready;
                    health.xmpp_active.detail = "Waiting for Riot chat traffic".to_string();
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_HELPER_JID: &str = "41c322a1-b328-495b-a004-5ccd3e45eae8@na2.pvp.net";

    fn app_state_with_runtime(running: bool) -> AppState {
        AppState {
            runtime: Some(ServiceRuntime {
                running: Arc::new(AtomicBool::new(running)),
                chat_port: 1234,
                config_port: 5678,
                riot_chat: Arc::new(Mutex::new(None)),
                active_connections: Arc::new(AtomicUsize::new(0)),
                reconnect_attempts: Arc::new(AtomicU32::new(0)),
                riot_client_path: None,
                active_game: LaunchGame::Lol,
            }),
            enabled: Arc::new(AtomicBool::new(true)),
            safe_mode: Arc::new(AtomicBool::new(false)),
            helper_friend: Arc::new(AtomicBool::new(false)),
            auto_accept: Arc::new(AtomicBool::new(false)),
            auto_accept_delay_ms: Arc::new(AtomicU32::new(2_000)),
            auto_accept_state: Arc::new(Mutex::new("Disabled".to_string())),
            discord_webhook_url: Arc::new(Mutex::new(String::new())),
            status: Arc::new(Mutex::new(PresenceStatus::Offline)),
            startup_status: StartupStatus::Last,
            connect_to_muc: Arc::new(AtomicBool::new(true)),
            health: Arc::new(Mutex::new(ConnectionHealth::default())),
            logs: Arc::new(Mutex::new(Vec::new())),
            stream_events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[test]
    fn snapshot_ignores_stopped_runtime() {
        let state = app_state_with_runtime(false);

        let snapshot = state.snapshot();

        assert!(!snapshot.running);
        assert_eq!(snapshot.chat_port, None);
        assert_eq!(snapshot.config_port, None);
    }

    #[test]
    fn snapshot_resets_health_for_stopped_runtime() {
        let state = app_state_with_runtime(false);
        set_health(&state.health, |health| {
            health.config_patched.state = HealthState::Error;
            health.config_patched.detail = "Config proxy stopped".to_string();
            health.tls_connected.state = HealthState::Error;
            health.tls_connected.detail = "TLS setup failed".to_string();
            health.active_connections = 2;
            health.reconnect_attempts = 3;
        });

        let snapshot = state.snapshot();

        assert!(!snapshot.running);
        assert_eq!(snapshot.health.config_patched.state, HealthState::Waiting);
        assert_eq!(snapshot.health.config_patched.detail, "Waiting");
        assert_eq!(snapshot.health.tls_connected.state, HealthState::Waiting);
        assert_eq!(snapshot.health.tls_connected.detail, "Waiting");
        assert_eq!(snapshot.health.active_connections, 0);
        assert_eq!(snapshot.health.reconnect_attempts, 0);
    }

    #[test]
    fn clear_stopped_runtime_removes_stale_runtime() {
        let mut state = app_state_with_runtime(false);

        state.clear_stopped_runtime();

        assert!(state.runtime.is_none());
    }

    #[test]
    fn clear_stopped_runtime_keeps_active_runtime() {
        let mut state = app_state_with_runtime(true);

        state.clear_stopped_runtime();

        assert!(state.runtime.is_some());
        assert!(state.snapshot().running);
    }

    #[test]
    fn start_failure_resets_stale_health_before_validation_error() {
        let mut state = app_state_with_runtime(false);
        set_health(&state.health, |health| {
            health.config_proxy.state = HealthState::Ready;
            health.config_proxy.detail = "stale proxy".to_string();
            health.active_connections = 3;
        });

        let result = state.start(StartOptions {
            game: LaunchGame::Lol,
            game_patchline: "live".to_string(),
            riot_client_params: Some("--flag \"unfinished".to_string()),
            game_params: None,
            launch_game: false,
        });

        assert!(result.is_err());
        let snapshot = state.snapshot();
        assert!(!snapshot.running);
        assert_eq!(snapshot.health.config_proxy.state, HealthState::Waiting);
        assert_eq!(snapshot.health.config_proxy.detail, "Waiting");
        assert_eq!(snapshot.health.active_connections, 0);
    }

    #[test]
    fn stopped_runtime_connection_error_does_not_dirty_reset_health() {
        let running = Arc::new(AtomicBool::new(false));
        let reconnect_attempts = Arc::new(AtomicU32::new(0));
        let health = Arc::new(Mutex::new(ConnectionHealth::default()));
        let (tx, rx) = mpsc::channel();

        record_proxy_connection_error(
            &running,
            &reconnect_attempts,
            &health,
            &tx,
            anyhow!("connection closed during stop"),
        );

        assert_eq!(reconnect_attempts.load(Ordering::Relaxed), 0);
        let health = health.lock().expect("health should not be poisoned");
        assert_eq!(health.tls_connected.state, HealthState::Waiting);
        assert_eq!(health.xmpp_active.state, HealthState::Waiting);
        drop(health);
        let line = rx.recv().expect("log line should be sent");
        assert_eq!(line.level, LogLevel::Info);
        assert!(line.message.contains("after Ghosty stopped"));
    }

    #[test]
    fn active_runtime_connection_error_marks_reconnect_warning() {
        let running = Arc::new(AtomicBool::new(true));
        let reconnect_attempts = Arc::new(AtomicU32::new(0));
        let health = Arc::new(Mutex::new(ConnectionHealth::default()));
        let (tx, rx) = mpsc::channel();

        record_proxy_connection_error(
            &running,
            &reconnect_attempts,
            &health,
            &tx,
            anyhow!("connection interrupted"),
        );

        assert_eq!(reconnect_attempts.load(Ordering::Relaxed), 1);
        let health = health.lock().expect("health should not be poisoned");
        assert_eq!(health.tls_connected.state, HealthState::Warning);
        assert_eq!(health.xmpp_active.state, HealthState::Warning);
        assert_eq!(health.reconnect_attempts, 1);
        drop(health);
        let line = rx.recv().expect("log line should be sent");
        assert_eq!(line.level, LogLevel::Warn);
        assert!(line.message.contains("connection interrupted"));
    }

    #[test]
    fn active_runtime_clean_close_does_not_mark_reconnect_warning() {
        let running = Arc::new(AtomicBool::new(true));
        let reconnect_attempts = Arc::new(AtomicU32::new(0));
        let health = Arc::new(Mutex::new(ConnectionHealth::default()));
        let (tx, rx) = mpsc::channel();

        record_proxy_connection_error(
            &running,
            &reconnect_attempts,
            &health,
            &tx,
            CleanConnectionClose.into(),
        );

        assert_eq!(reconnect_attempts.load(Ordering::Relaxed), 0);
        let health = health.lock().expect("health should not be poisoned");
        assert_eq!(health.tls_connected.state, HealthState::Waiting);
        assert_eq!(health.xmpp_active.state, HealthState::Waiting);
        drop(health);
        let line = rx.recv().expect("log line should be sent");
        assert_eq!(line.level, LogLevel::Info);
        assert!(line.message.contains("closed cleanly"));
    }

    #[test]
    fn active_connection_guard_clears_stale_active_health_on_last_close() {
        let active_connections = Arc::new(AtomicUsize::new(1));
        let health = Arc::new(Mutex::new(ConnectionHealth::default()));
        set_health(&health, |health| {
            health.active_connections = 1;
            health.tls_connected.state = HealthState::Active;
            health.tls_connected.detail = "TLS tunnel established".to_string();
            health.xmpp_active.state = HealthState::Active;
            health.xmpp_active.detail = "Riot chat traffic is flowing".to_string();
        });

        drop(ActiveConnectionGuard {
            active_connections,
            health: health.clone(),
        });

        let health = health.lock().expect("health should not be poisoned");
        assert_eq!(health.active_connections, 0);
        assert_eq!(health.tls_connected.state, HealthState::Ready);
        assert_eq!(
            health.tls_connected.detail,
            "Waiting for Riot Client connection"
        );
        assert_eq!(health.xmpp_active.state, HealthState::Ready);
        assert_eq!(health.xmpp_active.detail, "Waiting for Riot chat traffic");
    }

    #[test]
    fn active_connection_guard_preserves_warning_health_on_last_close() {
        let active_connections = Arc::new(AtomicUsize::new(1));
        let health = Arc::new(Mutex::new(ConnectionHealth::default()));
        set_health(&health, |health| {
            health.active_connections = 1;
            health.tls_connected.state = HealthState::Warning;
            health.tls_connected.detail = "Last connection closed".to_string();
            health.xmpp_active.state = HealthState::Warning;
            health.xmpp_active.detail = "Waiting for Riot Client reconnect".to_string();
        });

        drop(ActiveConnectionGuard {
            active_connections,
            health: health.clone(),
        });

        let health = health.lock().expect("health should not be poisoned");
        assert_eq!(health.active_connections, 0);
        assert_eq!(health.tls_connected.state, HealthState::Warning);
        assert_eq!(health.tls_connected.detail, "Last connection closed");
        assert_eq!(health.xmpp_active.state, HealthState::Warning);
        assert_eq!(
            health.xmpp_active.detail,
            "Waiting for Riot Client reconnect"
        );
    }

    #[test]
    fn certificate_check_reports_missing_cache_as_actionable() {
        let check = certificate_check(None);

        assert!(!check.ok);
        assert_eq!(check.label, "Proxy Certificate");
        assert!(check.detail.contains("not cached"));
        assert!(check.detail.contains("internet access"));
    }

    #[test]
    fn certificate_check_rejects_invalid_cache() {
        let check = certificate_check(Some(b"not a pkcs12 certificate"));

        assert!(!check.ok);
        assert_eq!(check.label, "Proxy Certificate");
        assert!(check.detail.contains("invalid"));
        assert!(check.detail.contains("redownload"));
    }

    #[test]
    fn helper_command_reply_reads_message_body_only() {
        let enabled = Arc::new(AtomicBool::new(true));
        let status = Arc::new(Mutex::new(PresenceStatus::Chat));

        let reply = helper_command_reply(
            "<message from='x'><subject>offline</subject><body>status</body></message>",
            &enabled,
            &status,
        )
        .expect("status command should not fail")
        .expect("status command should reply");

        assert_eq!(
            reply,
            "You are appearing online. Presence masking is enabled. Auto accept is disabled. Client state: Disabled. Send online, offline, mobile, enable, disable, status, friends, auto accept on, auto accept off, auto accept status, opgg, or opgg multi."
        );
        assert_eq!(
            *status.lock().expect("status poisoned"),
            PresenceStatus::Chat
        );
    }

    #[test]
    fn helper_command_status_reports_disabled_masking() {
        let enabled = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(PresenceStatus::Offline));

        let reply =
            helper_command_reply("<message><body>status</body></message>", &enabled, &status)
                .expect("status command should not fail")
                .expect("status command should reply");

        assert_eq!(
            reply,
            "You are appearing offline. Presence masking is disabled. Auto accept is disabled. Client state: Disabled. Send online, offline, mobile, enable, disable, status, friends, auto accept on, auto accept off, auto accept status, opgg, or opgg multi."
        );
    }

    #[test]
    fn helper_intro_message_reports_current_status() {
        let enabled = Arc::new(AtomicBool::new(true));
        let status = Arc::new(Mutex::new(PresenceStatus::Mobile));
        let auto_accept = Arc::new(AtomicBool::new(false));
        let auto_accept_state = Arc::new(Mutex::new("Disabled".to_string()));

        let message = helper_intro_message(&enabled, &status, &auto_accept, &auto_accept_state);

        assert_eq!(
            message,
            "You are appearing mobile. Presence masking is enabled. Auto accept is disabled. Client state: Disabled. Send online, offline, mobile, enable, disable, status, friends, auto accept on, auto accept off, auto accept status, opgg, or opgg multi."
        );
    }

    #[test]
    fn helper_command_reply_accepts_case_and_whitespace() {
        let enabled = Arc::new(AtomicBool::new(true));
        let status = Arc::new(Mutex::new(PresenceStatus::Chat));

        let reply = helper_command_reply(
            "<message><body>  OFFLINE  </body></message>",
            &enabled,
            &status,
        )
        .expect("offline command should not fail")
        .expect("offline command should reply");

        assert_eq!(reply, "You are now appearing offline.");
        assert_eq!(
            *status.lock().expect("status poisoned"),
            PresenceStatus::Offline
        );
    }

    #[test]
    fn helper_command_reply_accepts_command_words_in_message_text() {
        let enabled = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(PresenceStatus::Offline));

        let reply = helper_command_reply(
            "<message><body>please go online now</body></message>",
            &enabled,
            &status,
        )
        .expect("online command should not fail")
        .expect("online command should reply");

        assert_eq!(reply, "You are now appearing online.");
        assert_eq!(
            *status.lock().expect("status poisoned"),
            PresenceStatus::Chat
        );
    }

    #[test]
    fn helper_command_reply_replies_to_every_supported_command() {
        let commands = [
            ("online", "You are now appearing online."),
            ("offline", "You are now appearing offline."),
            ("mobile", "You are now appearing mobile."),
            ("enable", "Ghosty is now enabled."),
            ("disable", "Ghosty is now disabled."),
            (
                "status",
                "You are appearing mobile. Presence masking is disabled. Auto accept is disabled. Client state: Disabled. Send online, offline, mobile, enable, disable, status, friends, auto accept on, auto accept off, auto accept status, opgg, or opgg multi.",
            ),
            (
                "help",
                "Send online, offline, mobile, enable, disable, status, friends, auto accept on, auto accept off, auto accept status, opgg, or opgg multi.",
            ),
            ("auto accept", "Auto accept is now enabled."),
            ("auto accept on", "Auto accept is now enabled."),
            ("turn on auto accept", "Auto accept is now enabled."),
            ("auto accept off", "Auto accept is now disabled."),
            (
                "auto accept status",
                "Auto accept is disabled. Client state: Disabled.",
            ),
            (
                "op.gg",
                "https://www.op.gg/summoners/na/Ghosty-NA1",
            ),
            (
                "op.gg multi",
                "https://op.gg/lol/multisearch/na?summoners=Ghosty%23NA1%2CDuo%23NA2",
            ),
            (
                "friends status",
                "Friends: 3 total. Statuses: online 1, mobile 1, offline 1. Products: League 1, Riot Mobile 1, Unknown 1.",
            ),
        ];

        for (command, expected) in commands {
            let enabled = Arc::new(AtomicBool::new(false));
            let status = Arc::new(Mutex::new(PresenceStatus::Mobile));
            let mut buffer = HelperCommandBuffer::new(TEST_HELPER_JID.to_string());

            let result = buffer
                .push(
                    &format!(
                        "<message to='{TEST_HELPER_JID}/RC-Ghosty'><body>{command}</body></message>"
                    ),
                    &enabled,
                    &status,
                )
                .unwrap_or_else(|_| panic!("{command} command should not fail"));

            assert!(matches!(
                result,
                HelperCommandResult::Complete {
                    reply: Some(ref reply),
                    passthrough: ref pass
                } if reply == expected && pass.is_empty()
            ));
        }
    }

    #[test]
    fn helper_command_reply_ignores_unknown_words() {
        let enabled = Arc::new(AtomicBool::new(true));
        let status = Arc::new(Mutex::new(PresenceStatus::Chat));

        let reply = helper_command_reply(
            "<message><body>please wave back</body></message>",
            &enabled,
            &status,
        )
        .expect("unknown command should not fail");

        assert!(reply.is_none());
        assert_eq!(
            *status.lock().expect("status poisoned"),
            PresenceStatus::Chat
        );
    }

    #[test]
    fn helper_command_reply_decodes_xml_body_text_before_matching() {
        let enabled = Arc::new(AtomicBool::new(true));
        let status = Arc::new(Mutex::new(PresenceStatus::Chat));

        let reply = helper_command_reply(
            "<message><body>help &amp; status</body></message>",
            &enabled,
            &status,
        )
        .expect("status command should not fail")
        .expect("status command should reply");

        assert_eq!(
            reply,
            "You are appearing online. Presence masking is enabled. Auto accept is disabled. Client state: Disabled. Send online, offline, mobile, enable, disable, status, friends, auto accept on, auto accept off, auto accept status, opgg, or opgg multi."
        );
    }

    #[test]
    fn helper_command_reply_reads_message_from_batched_stanzas() {
        let enabled = Arc::new(AtomicBool::new(true));
        let status = Arc::new(Mutex::new(PresenceStatus::Chat));

        let reply = helper_command_reply(
            "<iq id='1'/><message><body>mobile</body></message>",
            &enabled,
            &status,
        )
        .expect("mobile command should not fail")
        .expect("mobile command should reply");

        assert_eq!(reply, "You are now appearing mobile.");
        assert_eq!(
            *status.lock().expect("status poisoned"),
            PresenceStatus::Mobile
        );
    }

    #[test]
    fn helper_command_reply_ignores_fragments_without_message_body() {
        let enabled = Arc::new(AtomicBool::new(true));
        let status = Arc::new(Mutex::new(PresenceStatus::Chat));

        let reply = helper_command_reply("<iq id='1'/><presence/>", &enabled, &status)
            .expect("missing command should not fail");

        assert!(reply.is_none());
        assert_eq!(
            *status.lock().expect("status poisoned"),
            PresenceStatus::Chat
        );
    }

    #[test]
    fn helper_command_buffer_waits_for_split_helper_message() {
        let enabled = Arc::new(AtomicBool::new(true));
        let status = Arc::new(Mutex::new(PresenceStatus::Chat));
        let mut buffer = HelperCommandBuffer::new(TEST_HELPER_JID.to_string());

        let first = buffer
            .push(
                &format!("<message to='{TEST_HELPER_JID}/RC-Ghosty'><body>off"),
                &enabled,
                &status,
            )
            .expect("first chunk should not fail");
        assert!(matches!(first, HelperCommandResult::Pending));

        let second = buffer
            .push("line</body></message>", &enabled, &status)
            .expect("second chunk should not fail");

        assert!(matches!(
            second,
            HelperCommandResult::Complete {
                reply: Some(ref reply),
                passthrough: ref pass
            }
                if reply == "You are now appearing offline."
                    && pass.is_empty()
        ));
        assert_eq!(
            *status.lock().expect("status poisoned"),
            PresenceStatus::Offline
        );
    }

    #[test]
    fn helper_command_buffer_waits_for_split_message_marker() {
        let enabled = Arc::new(AtomicBool::new(true));
        let status = Arc::new(Mutex::new(PresenceStatus::Chat));
        let mut buffer = HelperCommandBuffer::new(TEST_HELPER_JID.to_string());

        let first = buffer
            .push("<mess", &enabled, &status)
            .expect("first chunk should not fail");
        assert!(matches!(first, HelperCommandResult::Pending));

        let second = buffer
            .push(
                &format!("age to='{TEST_HELPER_JID}/RC-Ghosty'><body>mobile</body></message>"),
                &enabled,
                &status,
            )
            .expect("second chunk should not fail");

        assert!(matches!(
            second,
            HelperCommandResult::Complete {
                reply: Some(ref reply),
                passthrough: ref pass
            }
                if reply == "You are now appearing mobile."
                    && pass.is_empty()
        ));
        assert_eq!(
            *status.lock().expect("status poisoned"),
            PresenceStatus::Mobile
        );
    }

    #[test]
    fn helper_command_buffer_preserves_split_non_helper_message() {
        let enabled = Arc::new(AtomicBool::new(true));
        let status = Arc::new(Mutex::new(PresenceStatus::Chat));
        let mut buffer = HelperCommandBuffer::new(TEST_HELPER_JID.to_string());

        let first = buffer
            .push(
                "<message to='friend@na2.pvp.net'><body>hel",
                &enabled,
                &status,
            )
            .expect("first chunk should not fail");
        assert!(matches!(first, HelperCommandResult::Pending));

        let second = buffer
            .push("lo</body></message>", &enabled, &status)
            .expect("second chunk should not fail");

        let HelperCommandResult::Complete {
            reply: None,
            passthrough,
        } = second
        else {
            panic!("non-helper message should pass through once complete");
        };
        let passthrough = String::from_utf8(passthrough).expect("passthrough should be utf8");

        assert!(passthrough.contains("friend@na2.pvp.net"));
        assert!(passthrough.contains("<body>hello</body>"));
        assert_eq!(
            *status.lock().expect("status poisoned"),
            PresenceStatus::Chat
        );
    }

    #[test]
    fn helper_command_buffer_discards_unknown_complete_helper_message() {
        let enabled = Arc::new(AtomicBool::new(true));
        let status = Arc::new(Mutex::new(PresenceStatus::Chat));
        let mut buffer = HelperCommandBuffer::new(TEST_HELPER_JID.to_string());

        let result = buffer
            .push(
                &format!("<message to='{TEST_HELPER_JID}/RC-Ghosty'><body>wave</body></message>"),
                &enabled,
                &status,
            )
            .expect("unknown helper command should not fail");

        assert!(matches!(
            result,
            HelperCommandResult::Complete {
                reply: None,
                passthrough: ref pass
            } if pass.is_empty()
        ));
        assert_eq!(
            *status.lock().expect("status poisoned"),
            PresenceStatus::Chat
        );
    }

    #[test]
    fn helper_command_buffer_clear_drops_stale_partial_command() {
        let enabled = Arc::new(AtomicBool::new(true));
        let status = Arc::new(Mutex::new(PresenceStatus::Chat));
        let mut buffer = HelperCommandBuffer::new(TEST_HELPER_JID.to_string());

        let first = buffer
            .push(
                &format!("<message to='{TEST_HELPER_JID}/RC-Ghosty'><body>off"),
                &enabled,
                &status,
            )
            .expect("first helper chunk should not fail");
        assert!(matches!(first, HelperCommandResult::Pending));

        buffer.clear();

        let second = buffer
            .push("line</body></message>", &enabled, &status)
            .expect("stale suffix should not fail");

        assert!(matches!(second, HelperCommandResult::NotHelper));
        assert_eq!(
            *status.lock().expect("status poisoned"),
            PresenceStatus::Chat
        );
    }

    #[test]
    fn helper_command_buffer_ignores_non_helper_content() {
        let enabled = Arc::new(AtomicBool::new(true));
        let status = Arc::new(Mutex::new(PresenceStatus::Chat));
        let mut buffer = HelperCommandBuffer::new(TEST_HELPER_JID.to_string());

        let result = buffer
            .push(
                "<message to='friend@na2.pvp.net'><body>offline</body></message>",
                &enabled,
                &status,
            )
            .expect("non-helper message should not fail");

        assert!(matches!(result, HelperCommandResult::NotHelper));
    }

    #[test]
    fn helper_command_buffer_ignores_messages_from_helper() {
        let enabled = Arc::new(AtomicBool::new(true));
        let status = Arc::new(Mutex::new(PresenceStatus::Chat));
        let mut buffer = HelperCommandBuffer::new(TEST_HELPER_JID.to_string());

        let result = buffer
            .push(
                &format!(
                    "<message from='{TEST_HELPER_JID}/RC-Ghosty'><body>offline</body></message>"
                ),
                &enabled,
                &status,
            )
            .expect("message from helper should not fail");

        assert!(matches!(result, HelperCommandResult::NotHelper));
        assert_eq!(
            *status.lock().expect("status poisoned"),
            PresenceStatus::Chat
        );
    }

    #[test]
    fn helper_command_buffer_reads_targeted_message_from_batched_stanzas() {
        let enabled = Arc::new(AtomicBool::new(true));
        let status = Arc::new(Mutex::new(PresenceStatus::Chat));
        let mut buffer = HelperCommandBuffer::new(TEST_HELPER_JID.to_string());

        let result = buffer
            .push(
                &format!(
                    "<iq id='1'/><message to=\"{TEST_HELPER_JID}/RC-Ghosty\"><body>mobile</body></message>"
                ),
                &enabled,
                &status,
            )
            .expect("batched helper message should not fail");

        assert!(matches!(
            result,
            HelperCommandResult::Complete {
                reply: Some(ref reply),
                passthrough: ref pass
            }
                if reply == "You are now appearing mobile."
                    && !String::from_utf8_lossy(pass).contains(TEST_HELPER_JID)
                    && String::from_utf8_lossy(pass).contains("<iq")
        ));
        assert_eq!(
            *status.lock().expect("status poisoned"),
            PresenceStatus::Mobile
        );
    }

    #[test]
    fn helper_command_buffer_preserves_non_helper_stanzas_around_command() {
        let enabled = Arc::new(AtomicBool::new(true));
        let status = Arc::new(Mutex::new(PresenceStatus::Chat));
        let mut buffer = HelperCommandBuffer::new(TEST_HELPER_JID.to_string());

        let result = buffer
            .push(
                &format!(
                    "<presence><show>chat</show></presence><message to='{TEST_HELPER_JID}/RC-Ghosty'><body>offline</body></message><message to='friend@na2.pvp.net'><body>hello</body></message>"
                ),
                &enabled,
                &status,
            )
            .expect("batched helper command should not fail");

        let HelperCommandResult::Complete {
            reply: Some(reply),
            passthrough,
        } = result
        else {
            panic!("helper command should complete with passthrough");
        };
        let passthrough = String::from_utf8(passthrough).expect("passthrough should be utf8");

        assert_eq!(reply, "You are now appearing offline.");
        assert!(passthrough.contains("<presence"));
        assert!(passthrough.contains("friend@na2.pvp.net"));
        assert!(!passthrough.contains(TEST_HELPER_JID));
        assert_eq!(
            *status.lock().expect("status poisoned"),
            PresenceStatus::Offline
        );
    }

    #[test]
    fn helper_command_buffer_preserves_text_between_passthrough_stanzas() {
        let enabled = Arc::new(AtomicBool::new(true));
        let status = Arc::new(Mutex::new(PresenceStatus::Chat));
        let mut buffer = HelperCommandBuffer::new(TEST_HELPER_JID.to_string());

        let result = buffer
            .push(
                &format!(
                    "<presence><show>chat</show></presence>\n<message to='{TEST_HELPER_JID}/RC-Ghosty'><body>status</body></message>\n<iq id='2'/>"
                ),
                &enabled,
                &status,
            )
            .expect("batched helper command should not fail");

        let HelperCommandResult::Complete {
            reply: Some(reply),
            passthrough,
        } = result
        else {
            panic!("helper command should complete with passthrough");
        };
        let passthrough = String::from_utf8(passthrough).expect("passthrough should be utf8");

        assert_eq!(
            reply,
            "You are appearing online. Presence masking is enabled. Auto accept is disabled. Client state: Disabled. Send online, offline, mobile, enable, disable, status, friends, auto accept on, auto accept off, auto accept status, opgg, or opgg multi."
        );
        assert!(passthrough.contains("</presence>"));
        assert!(passthrough.contains('\n'));
        assert!(passthrough.contains("<iq"));
        assert!(!passthrough.contains(TEST_HELPER_JID));
    }

    #[test]
    fn utf8_stream_buffer_waits_for_split_multibyte_character() {
        let mut buffer = Utf8StreamBuffer::new();
        let mut bytes = "olé".as_bytes().to_vec();
        let suffix = bytes.split_off(bytes.len() - 1);

        let first = buffer.push(bytes);
        assert!(matches!(first, Utf8Chunk::Text { ref content, .. } if content == "ol"));
        assert!(buffer.has_pending());

        let second = buffer.push(suffix);
        assert!(matches!(second, Utf8Chunk::Text { ref content, .. } if content == "é"));
        assert!(!buffer.has_pending());
    }

    #[test]
    fn utf8_stream_buffer_flushes_incomplete_suffix_on_close() {
        let mut buffer = Utf8StreamBuffer::new();
        let mut bytes = "olé".as_bytes().to_vec();
        let expected_pending = vec![bytes[bytes.len() - 2]];
        let _suffix = bytes.split_off(bytes.len() - 1);

        let first = buffer.push(bytes);
        assert!(matches!(first, Utf8Chunk::Text { ref content, .. } if content == "ol"));
        assert!(buffer.has_pending());

        assert_eq!(buffer.flush(), expected_pending);
        assert!(!buffer.has_pending());
    }

    #[test]
    fn helper_injector_handles_roster_after_split_multibyte_character() {
        let mut utf8 = Utf8StreamBuffer::new();
        let mut injector = HelperFriendInjector::new(TEST_HELPER_JID.to_string());
        let content = "olé<iq type='result'><query xmlns='jabber:iq:riotgames:roster'><item jid='friend@na2.pvp.net'/></query></iq>";
        let split_at = content
            .as_bytes()
            .windows("é".len())
            .position(|window| window == "é".as_bytes())
            .expect("test string should include accented character")
            + 1;
        let first_bytes = content.as_bytes()[..split_at].to_vec();
        let second_bytes = content.as_bytes()[split_at..].to_vec();

        let first = match utf8.push(first_bytes) {
            Utf8Chunk::Text { content, .. } => injector.push(&content),
            _ => panic!("first chunk should include a valid UTF-8 prefix"),
        };
        assert!(!first.inserted);
        assert_eq!(first.bytes, b"ol");

        let second = match utf8.push(second_bytes) {
            Utf8Chunk::Text { content, .. } => injector.push(&content),
            _ => panic!("second chunk should complete the split character"),
        };
        let mut output_bytes = first.bytes;
        output_bytes.extend(second.bytes);
        let output = String::from_utf8(output_bytes).expect("injected output should be utf8");

        assert!(second.inserted);
        assert!(output.contains("olé"));
        assert!(output.contains(TEST_HELPER_JID));
        assert!(output.contains("friend@na2.pvp.net"));
    }

    #[test]
    fn client_presence_buffer_rewrites_after_split_multibyte_character() {
        let mut utf8 = Utf8StreamBuffer::new();
        let mut presence = ClientPresenceBuffer::new();
        presence.initial_presence_forwarded = true;
        let mut valorant_version = None;
        let content = "<presence><show>chat</show><status>olé</status></presence>";
        let split_at = content
            .as_bytes()
            .windows("é".len())
            .position(|window| window == "é".as_bytes())
            .expect("test string should include accented character")
            + 1;
        let first_bytes = content.as_bytes()[..split_at].to_vec();
        let second_bytes = content.as_bytes()[split_at..].to_vec();

        let first = match utf8.push(first_bytes) {
            Utf8Chunk::Text { content, .. } => presence
                .push(
                    &content,
                    true,
                    PresenceStatus::Offline,
                    true,
                    &mut valorant_version,
                )
                .expect("valid prefix should not fail"),
            _ => panic!("first chunk should include a valid UTF-8 prefix"),
        };
        assert!(first.is_empty());

        let second = match utf8.push(second_bytes) {
            Utf8Chunk::Text { content, .. } => presence
                .push(
                    &content,
                    true,
                    PresenceStatus::Offline,
                    true,
                    &mut valorant_version,
                )
                .expect("completed UTF-8 suffix should not fail"),
            _ => panic!("second chunk should complete the split character"),
        };
        let rewritten = String::from_utf8(second).expect("presence should remain utf8");

        assert!(rewritten.contains("<show>offline</show>"));
        assert!(!rewritten.contains("<status>ol"));
    }

    #[test]
    fn client_presence_buffer_rewrites_split_presence() {
        let mut buffer = ClientPresenceBuffer::new();
        buffer.initial_presence_forwarded = true;
        let mut valorant_version = None;

        let first = buffer
            .push(
                "<presence><games><league_of_legends><st>chat</st></league_of_legends></games>",
                true,
                PresenceStatus::Offline,
                true,
                &mut valorant_version,
            )
            .expect("first presence chunk should not fail");
        assert!(first.is_empty());

        let second = buffer
            .push(
                "<show>chat</show><status>hello</status></presence>",
                true,
                PresenceStatus::Offline,
                true,
                &mut valorant_version,
            )
            .expect("second presence chunk should not fail");
        let rewritten = String::from_utf8(second).expect("presence should remain utf8");

        assert!(rewritten.contains("<show>offline</show>"));
        assert!(rewritten.contains("<league_of_legends>"));
        assert!(rewritten.contains("<st>offline</st>"));
        assert!(!rewritten.contains("<status>"));
    }

    #[test]
    fn client_presence_buffer_forwards_initial_presence_then_stores_masked_followup() {
        let mut buffer = ClientPresenceBuffer::new();
        let mut valorant_version = None;

        let output = buffer
            .push(
                "<iq type='get' id='1'/><presence><games><league_of_legends><st>chat</st></league_of_legends></games><show>chat</show><status>hello</status></presence>",
                true,
                PresenceStatus::Offline,
                true,
                &mut valorant_version,
            )
            .expect("initial presence should not fail");
        let output = String::from_utf8(output).expect("presence should remain utf8");

        assert!(output.contains("<iq type='get' id='1'/>"));
        assert!(output.contains("<show>chat</show>"));
        assert!(output.contains("<status>hello</status>"));
        assert!(buffer.has_warmup_masked_presence());

        let masked = String::from_utf8(
            buffer
                .take_warmup_masked_presence()
                .expect("masked followup should be cached"),
        )
        .expect("masked followup should remain utf8");

        assert!(masked.contains("<show>offline</show>"));
        assert!(masked.contains("<st>offline</st>"));
        assert!(!masked.contains("<iq"));
        assert!(!masked.contains("<status>hello</status>"));
    }

    #[test]
    fn client_presence_buffer_uses_initial_status_for_split_presence() {
        let mut buffer = ClientPresenceBuffer::new();
        buffer.initial_presence_forwarded = true;
        let mut valorant_version = None;

        let first = buffer
            .push(
                "<presence><show>chat</show>",
                true,
                PresenceStatus::Offline,
                true,
                &mut valorant_version,
            )
            .expect("first presence chunk should not fail");
        assert!(first.is_empty());

        let second = buffer
            .push(
                "</presence>",
                true,
                PresenceStatus::Mobile,
                true,
                &mut valorant_version,
            )
            .expect("second presence chunk should not fail");
        let rewritten = String::from_utf8(second).expect("presence should remain utf8");

        assert!(rewritten.contains("<show>offline</show>"));
        assert!(!rewritten.contains("<show>mobile</show>"));
    }

    #[test]
    fn client_presence_buffer_uses_initial_muc_setting_for_split_presence() {
        let mut buffer = ClientPresenceBuffer::new();
        let mut valorant_version = None;

        let first = buffer
            .push(
                "<presence to='room@conference.pvp.net'><show>chat</show>",
                true,
                PresenceStatus::Offline,
                false,
                &mut valorant_version,
            )
            .expect("first presence chunk should not fail");
        assert!(first.is_empty());

        let second = buffer
            .push(
                "</presence>",
                true,
                PresenceStatus::Offline,
                true,
                &mut valorant_version,
            )
            .expect("second presence chunk should not fail");

        assert!(second.is_empty());
    }

    #[test]
    fn client_presence_buffer_preserves_addressed_presence_when_muc_enabled() {
        let mut buffer = ClientPresenceBuffer::new();
        let mut valorant_version = None;

        let output = buffer
            .push(
                "<presence to='room@conference.pvp.net'><games><league_of_legends><st>chat</st></league_of_legends></games><show>chat</show><status>hello</status></presence>",
                true,
                PresenceStatus::Offline,
                true,
                &mut valorant_version,
            )
            .expect("addressed presence should not fail");
        let output = String::from_utf8(output).expect("presence should remain utf8");

        assert!(output.contains("to=\"room@conference.pvp.net\""));
        assert!(output.contains("<show>chat</show>"));
        assert!(output.contains("<status>hello</status>"));
        assert!(output.contains("<league_of_legends>"));
        assert!(!output.contains("<show>offline</show>"));
    }

    #[test]
    fn client_presence_buffer_waits_for_split_presence_marker() {
        let mut buffer = ClientPresenceBuffer::new();
        buffer.initial_presence_forwarded = true;
        let mut valorant_version = None;

        let first = buffer
            .push(
                "<pre",
                true,
                PresenceStatus::Mobile,
                true,
                &mut valorant_version,
            )
            .expect("partial marker should not fail");
        assert!(first.is_empty());

        let second = buffer
            .push(
                "sence><show>chat</show></presence>",
                true,
                PresenceStatus::Mobile,
                true,
                &mut valorant_version,
            )
            .expect("completed marker should not fail");
        let rewritten = String::from_utf8(second).expect("presence should remain utf8");

        assert!(rewritten.contains("<show>mobile</show>"));
    }

    #[test]
    fn client_presence_buffer_forwards_non_presence_content() {
        let mut buffer = ClientPresenceBuffer::new();
        let mut valorant_version = None;

        let output = buffer
            .push(
                "<message><body>hello</body></message>",
                true,
                PresenceStatus::Offline,
                true,
                &mut valorant_version,
            )
            .expect("ordinary message should not fail");

        assert_eq!(output, b"<message><body>hello</body></message>");
    }

    #[test]
    fn client_presence_buffer_rewrites_presence_inside_batched_stanzas() {
        let mut buffer = ClientPresenceBuffer::new();
        buffer.initial_presence_forwarded = true;
        let mut valorant_version = None;

        let output = buffer
            .push(
                "<message id='1'/><presence><show>chat</show><status>hello</status></presence>",
                true,
                PresenceStatus::Offline,
                true,
                &mut valorant_version,
            )
            .expect("batched stanza should not fail");
        let rewritten = String::from_utf8(output).expect("batched stanza should remain utf8");

        assert!(rewritten.contains("<message"));
        assert!(rewritten.contains("<show>offline</show>"));
        assert!(!rewritten.contains("<status>hello</status>"));
    }

    #[test]
    fn server_presence_stats_summarizes_friend_status_batches() {
        let mut stats = PresenceStats::default();
        let message = stats
            .record_server_batch(
                "<presence from='a@na2.pvp.net'><games><league_of_legends><st>chat</st></league_of_legends></games><show>chat</show></presence>\
                 <presence from='b@na2.pvp.net' type='unavailable'/>\
                 <presence from='c@na2.pvp.net'><games><league_of_legends><st>dnd</st></league_of_legends></games><show>dnd</show></presence>",
            )
            .expect("presence batch should be logged");

        assert!(message.contains("batch=3"));
        assert!(message.contains("unavailable=1"));
        assert!(message.contains("show(chat/dnd/away/offline/mobile)=1/1/0/0/0"));
        assert!(message.contains("league_st(chat/dnd/away/offline/mobile)=1/1/0/0/0"));
        assert!(message.contains("domains=na2.pvp.net=3"));
        assert_eq!(stats.total_presence, 3);
        assert_eq!(stats.total_unavailable, 1);
    }

    #[test]
    fn server_presence_stats_marks_warmup_ready_after_presence_fanout() {
        let mut stats = PresenceStats::default();

        assert!(!stats.is_warmup_ready());
        let _ = stats.record_server_batch(
            "<presence from='a@na2.pvp.net'><show>chat</show></presence>\
             <presence from='b@na2.pvp.net'><show>dnd</show></presence>",
        );
        assert!(!stats.is_warmup_ready());
        let _ = stats.record_server_batch(
            "<presence from='c@na2.pvp.net'><games><league_of_legends><st>chat</st></league_of_legends></games><show>chat</show></presence>",
        );

        assert!(stats.is_warmup_ready());
    }

    #[test]
    fn startup_presence_replay_extracts_only_presence_stanzas() {
        let mut replay = StartupPresenceReplay::new();

        let added = replay.push(
            "<iq id='1'/><presence from='a@na1.pvp.net'><show>chat</show></presence><message/>\
             <presence from='b@na1.pvp.net' type='unavailable'/>",
        );
        let body = String::from_utf8(replay.take()).expect("replay should be utf8");

        assert_eq!(added, 2);
        assert_eq!(replay.replayed_count(), 2);
        assert!(body.contains("from='a@na1.pvp.net'"));
        assert!(body.contains("from='b@na1.pvp.net'"));
        assert!(!body.contains("<iq"));
        assert!(!body.contains("<message"));
    }

    #[test]
    fn startup_presence_replay_waits_for_split_stanza() {
        let mut replay = StartupPresenceReplay::new();

        assert_eq!(
            replay.push("<presence from='a@na1.pvp.net'><show>chat</show>"),
            0
        );
        assert_eq!(replay.push("</presence><iq/>"), 1);
        let body = String::from_utf8(replay.take()).expect("replay should be utf8");

        assert_eq!(
            body,
            "<presence from='a@na1.pvp.net'><show>chat</show></presence>"
        );
    }

    #[test]
    fn jid_domains_summarizes_roster_and_presence_domains() {
        assert_eq!(
            jid_domains(
                "<item jid='one@na1.pvp.net'/><item jid=\"two@na2.pvp.net/resource\"/><presence from='three@na1.pvp.net/RC'/>",
                "jid"
            ),
            "na1.pvp.net=1,na2.pvp.net=1"
        );
        assert_eq!(
            jid_domains(
                "<item jid='one@na1.pvp.net'/><presence from='three@na1.pvp.net/RC'/>",
                "from"
            ),
            "na1.pvp.net=1"
        );
    }

    #[test]
    fn rewrites_server_jid_domains_to_match_roster_jids_per_user() {
        let roster_domains = jid_domain_map(
            "<item jid='friend@na1.pvp.net'/><item jid='other@eu1.pvp.net'/>",
            "jid",
        );
        let rewritten = rewrite_jid_attribute_domains_from_map(
            "<presence from='friend@na2.pvp.net/RC'><show>chat</show></presence>\
             <message from=\"other@na2.pvp.net/mobile\"/>\
             <message from='unknown@na2.pvp.net/mobile'/>\
             <presence from='room@conference.pvp.net/user'/>",
            "from",
            &roster_domains,
        )
        .expect("domain mismatch should be rewritten");

        assert!(rewritten.contains("friend@na1.pvp.net/RC"));
        assert!(rewritten.contains("other@eu1.pvp.net/mobile"));
        assert!(rewritten.contains("unknown@na2.pvp.net/mobile"));
        assert!(rewritten.contains("room@conference.pvp.net/user"));
        assert!(!rewritten.contains("friend@na2.pvp.net"));
    }

    #[test]
    fn jid_domain_map_preserves_each_roster_jid_domain() {
        let domains = jid_domain_map(
            "<item jid='a@na1.pvp.net'/><item jid='b@na1.pvp.net'/><item jid='c@na2.pvp.net'/>",
            "jid",
        );
        assert_eq!(domains.get("a").map(String::as_str), Some("na1.pvp.net"));
        assert_eq!(domains.get("c").map(String::as_str), Some("na2.pvp.net"));
    }

    #[test]
    fn stream_preview_redacts_entitlement_tokens() {
        let preview = stream_preview(
            b"<iq><entitlements><token>super-secret.jwt.payload</token></entitlements></iq>",
        );

        assert!(preview.contains("<token>[redacted]</token>"));
        assert!(!preview.contains("super-secret"));
    }

    #[test]
    fn possible_roster_prefix_keeps_only_partial_marker_tail() {
        let prefix_len = possible_roster_prefix_len("abc<query xmlns='jabber:iq");

        assert_eq!(prefix_len, "<query xmlns='jabber:iq".len());
    }

    #[test]
    fn possible_roster_prefix_keeps_split_query_tag_with_extra_attributes() {
        let content = "abc<query ver='2' xmlns=\"jabber:iq";

        assert_eq!(
            possible_roster_prefix_len(content),
            "<query ver='2' xmlns=\"jabber:iq".len()
        );
    }

    #[test]
    fn possible_roster_prefix_ignores_unrelated_text() {
        assert_eq!(possible_roster_prefix_len("abc<message/>"), 0);
    }

    #[test]
    fn helper_injector_inserts_when_roster_marker_is_split() {
        let mut injector = HelperFriendInjector::new(TEST_HELPER_JID.to_string());

        let first = injector.push("<iq type='result'><query xmlns='jabber:");
        assert!(!first.inserted);
        assert!(!first.bytes.is_empty());

        let second =
            injector.push("iq:riotgames:roster'><item jid='friend@na2.pvp.net'/></query></iq>");
        let output = String::from_utf8(second.bytes).expect("injected output should be utf-8");

        assert!(second.inserted);
        assert!(output.contains("Ghosty Active!"));
        assert!(output.contains("friend@na2.pvp.net"));
    }

    #[test]
    fn helper_injector_buffers_a_marker_split_at_the_first_byte() {
        let mut injector = HelperFriendInjector::new(TEST_HELPER_JID.to_string());

        let first = injector.push("<");
        assert!(!first.inserted);
        assert!(first.bytes.is_empty());

        let second = injector.push(
            "query xmlns='jabber:iq:riotgames:roster'><item jid='friend@na2.pvp.net'/></query>",
        );
        let output = String::from_utf8(second.bytes).expect("injected output should be utf-8");

        assert!(second.inserted);
        assert!(output.contains("Ghosty Active!"));
        assert!(output.contains("friend@na2.pvp.net"));
    }

    #[test]
    fn helper_injector_inserts_when_roster_query_has_double_quotes_and_extra_attributes() {
        let mut injector = HelperFriendInjector::new(TEST_HELPER_JID.to_string());

        let first = injector.push("<iq type='result'><query ver='2' xmlns=\"jabber:");
        assert!(!first.inserted);
        assert!(!first.bytes.is_empty());

        let second =
            injector.push("iq:riotgames:roster\"><item jid='friend@na2.pvp.net'/></query></iq>");
        let output = String::from_utf8(second.bytes).expect("injected output should be utf-8");

        assert!(second.inserted);
        assert!(output.contains("Ghosty Active!"));
        assert!(output.contains("friend@na2.pvp.net"));
    }

    #[test]
    fn helper_injector_preserves_large_roster_split_across_many_chunks() {
        let mut injector = HelperFriendInjector::new(TEST_HELPER_JID.to_string());
        let friend_count = 500;
        let mut roster =
            "<iq type='result' id='roster'><query xmlns='jabber:iq:riotgames:roster'>".to_string();
        for index in 0..friend_count {
            roster.push_str(&format!(
                "<item jid='friend-{index}@na2.pvp.net' name='Friend {index}' subscription='both'/>"
            ));
        }
        roster.push_str("</query></iq>");

        let mut forwarded = Vec::new();
        let mut inserted = None;
        for chunk in roster.as_bytes().chunks(37) {
            let content = std::str::from_utf8(chunk).expect("test roster should stay utf-8");
            let injection = injector.push(content);
            if injection.inserted {
                inserted = Some((injection.roster_items_before, injection.roster_items_after));
            }
            forwarded.extend(injection.bytes);
        }
        forwarded.extend(injector.flush());
        let output = String::from_utf8(forwarded).expect("forwarded roster should stay utf-8");

        assert_eq!(inserted, Some((Some(friend_count), Some(friend_count + 1))));
        assert_eq!(presence::roster_item_count(&output), friend_count + 1);
        for index in 0..friend_count {
            assert!(output.contains(&format!("friend-{index}@na2.pvp.net")));
        }
        assert!(output.contains("Ghosty Active!"));
        assert!(output.contains("</query></iq>"));
    }

    #[test]
    fn helper_injector_flushes_buffered_partial_marker() {
        let mut injector = HelperFriendInjector::new(TEST_HELPER_JID.to_string());

        let first = injector.push("<");
        assert!(!first.inserted);
        assert!(first.bytes.is_empty());

        assert_eq!(injector.flush(), b"<");
        assert!(injector.is_empty());
    }
}
