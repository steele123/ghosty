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
    time::Duration,
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

const STREAM_EVENT_PREVIEW_CHARS: usize = 1_400;
const LOG_BUFFER_LIMIT: usize = 240;
const STREAM_EVENT_BUFFER_LIMIT: usize = 800;

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

        Ok(Self {
            runtime: None,
            enabled: Arc::new(AtomicBool::new(true)),
            safe_mode: Arc::new(AtomicBool::new(false)),
            helper_friend: Arc::new(AtomicBool::new(persistence::read_helper_friend())),
            status: Arc::new(Mutex::new(session_status)),
            startup_status,
            connect_to_muc: Arc::new(AtomicBool::new(true)),
            health: Arc::new(Mutex::new(ConnectionHealth::default())),
            logs: Arc::new(Mutex::new(Vec::new())),
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

#[derive(Clone)]
struct ChatProxyContext {
    running: Arc<AtomicBool>,
    acceptor: TlsAcceptor,
    server: PatchedChatServer,
    enabled: Arc<AtomicBool>,
    safe_mode: Arc<AtomicBool>,
    helper_friend: Arc<AtomicBool>,
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
    let mut sent_helper_presence = false;
    let mut refreshed_helper_presence_after_client_presence = false;
    let mut sent_helper_intro = false;
    let mut logged_roster_without_helper = false;
    let helper_jid = presence::helper_jid_for_chat_identity(
        &context.server.host,
        context.server.affinity.as_deref(),
    );
    let mut helper_injector = HelperFriendInjector::new(helper_jid.clone());
    let mut helper_command_buffer = HelperCommandBuffer::new(helper_jid.clone());
    let mut presence_buffer = ClientPresenceBuffer::new();
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

        if !helper_friend_enabled && was_helper_friend_enabled && !helper_injector.is_empty() {
            incoming.write_all(&helper_injector.flush())?;
        }
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
                        match helper_command_buffer.push(
                            &content,
                            &context.enabled,
                            &context.status,
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
                        if helper_friend_enabled && !inserted_helper_friend {
                            let injection = helper_injector.push(&content);
                            if injection.inserted {
                                inserted_helper_friend = true;
                                log(
                                    &context.log_tx,
                                    LogCategory::Chat,
                                    LogLevel::Info,
                                    "Inserted Ghosty helper friend into roster",
                                );
                            }
                            bytes_to_client = injection.bytes;
                        } else if !helper_friend_enabled
                            && !logged_roster_without_helper
                            && presence::contains_roster_marker(&content)
                        {
                            logged_roster_without_helper = true;
                            log(
                                &context.log_tx,
                                LogCategory::Chat,
                                LogLevel::Info,
                                "Roster marker passed while helper friend was disabled",
                            );
                        }
                    }
                    Utf8Chunk::Binary(bytes) => {
                        bytes_to_client = bytes;
                        if !helper_injector.is_empty() {
                            let mut pending = helper_injector.flush();
                            pending.extend_from_slice(&bytes_to_client);
                            bytes_to_client = pending;
                        }
                    }
                    Utf8Chunk::Pending => {}
                }
                if !bytes_to_client.is_empty() {
                    log_stream_bytes(&context.stream_tx, "ghosty -> client", &bytes_to_client);
                    incoming.write_all(&bytes_to_client)?;
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
                        &helper_intro_message(&context.enabled, &context.status),
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
                        &helper_intro_message(&context.enabled, &context.status),
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
                let mut pending = outgoing_text_buffer.flush();
                if !helper_injector.is_empty() {
                    let mut helper_pending = helper_injector.flush();
                    helper_pending.extend_from_slice(&pending);
                    pending = helper_pending;
                }
                if !pending.is_empty() {
                    log_stream_bytes(&context.stream_tx, "ghosty -> client", &pending);
                    incoming.write_all(&pending)?;
                }
                return Err(CleanConnectionClose.into());
            }
            StreamRead::WouldBlock => {}
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

struct HelperFriendInjector {
    pending: String,
    helper_jid: String,
}

struct HelperInjection {
    bytes: Vec<u8>,
    inserted: bool,
}

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
            self.pending.clear();
            return HelperInjection {
                bytes: updated.into_bytes(),
                inserted: true,
            };
        }

        let tail_len = possible_roster_prefix_len(&self.pending);
        if tail_len == self.pending.len() {
            return HelperInjection {
                bytes: Vec::new(),
                inserted: false,
            };
        }

        let split_at = self.pending.len().saturating_sub(tail_len);
        let tail = self.pending.split_off(split_at);
        let flush = std::mem::replace(&mut self.pending, tail);
        HelperInjection {
            bytes: flush.into_bytes(),
            inserted: false,
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

    fn push(
        &mut self,
        content: &str,
        enabled: &Arc<AtomicBool>,
        status: &Arc<Mutex<PresenceStatus>>,
    ) -> Result<HelperCommandResult> {
        if self.pending.is_empty()
            && !presence::contains_helper_message(content, &self.helper_jid)
            && !should_buffer_message_fragment(content)
        {
            return Ok(HelperCommandResult::NotHelper);
        }

        self.pending.push_str(content);
        if let Some(intercept) =
            helper_command_intercept_from_buffer(&self.pending, enabled, status, &self.helper_jid)?
        {
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
    let Some(command) = helper_command_text(content) else {
        return Ok(None);
    };
    helper_command_reply_for_text(&command, enabled, status)
}

struct HelperCommandIntercept {
    reply: Option<String>,
    passthrough: String,
}

fn helper_command_intercept_from_buffer(
    content: &str,
    enabled: &Arc<AtomicBool>,
    status: &Arc<Mutex<PresenceStatus>>,
    helper_jid: &str,
) -> Result<Option<HelperCommandIntercept>> {
    let Some(extract) = helper_command_extract_to(content, helper_jid) else {
        return Ok(None);
    };
    let reply = extract
        .command
        .as_deref()
        .map(|command| helper_command_reply_for_text(command, enabled, status))
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
    } else if command == "status" {
        Ok(Some(helper_status_message(enabled, status)?))
    } else if command == "help" {
        Ok(Some(
            "Send online, offline, mobile, enable, disable, or status.".to_string(),
        ))
    } else {
        Ok(None)
    }
}

fn helper_command_kind(command: &str) -> Option<&'static str> {
    const COMMANDS: [&str; 7] = [
        "offline", "mobile", "online", "disable", "enable", "status", "help",
    ];
    let command = command.to_ascii_lowercase();
    COMMANDS
        .into_iter()
        .find(|keyword| contains_command_word(&command, keyword))
}

fn contains_command_word(command: &str, keyword: &str) -> bool {
    command
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|word| word == keyword)
}

fn helper_intro_message(enabled: &Arc<AtomicBool>, status: &Arc<Mutex<PresenceStatus>>) -> String {
    helper_status_message(enabled, status).unwrap_or_else(|_| {
        "Ghosty is running. Send online, offline, mobile, enable, disable, or status.".to_string()
    })
}

fn helper_status_message(
    enabled: &Arc<AtomicBool>,
    status: &Arc<Mutex<PresenceStatus>>,
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
    Ok(format!(
        "You are appearing {status}. Presence masking is {masking}. Send online, offline, mobile, enable, disable, or status."
    ))
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
            "You are appearing online. Presence masking is enabled. Send online, offline, mobile, enable, disable, or status."
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
            "You are appearing offline. Presence masking is disabled. Send online, offline, mobile, enable, disable, or status."
        );
    }

    #[test]
    fn helper_intro_message_reports_current_status() {
        let enabled = Arc::new(AtomicBool::new(true));
        let status = Arc::new(Mutex::new(PresenceStatus::Mobile));

        let message = helper_intro_message(&enabled, &status);

        assert_eq!(
            message,
            "You are appearing mobile. Presence masking is enabled. Send online, offline, mobile, enable, disable, or status."
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
                "You are appearing mobile. Presence masking is disabled. Send online, offline, mobile, enable, disable, or status.",
            ),
            (
                "help",
                "Send online, offline, mobile, enable, disable, or status.",
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
            "You are appearing online. Presence masking is enabled. Send online, offline, mobile, enable, disable, or status."
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
            "You are appearing online. Presence masking is enabled. Send online, offline, mobile, enable, disable, or status."
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
        assert!(!rewritten.contains("league_of_legends"));
        assert!(!rewritten.contains("<status>"));
    }

    #[test]
    fn client_presence_buffer_uses_initial_status_for_split_presence() {
        let mut buffer = ClientPresenceBuffer::new();
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
    fn helper_injector_flushes_buffered_partial_marker() {
        let mut injector = HelperFriendInjector::new(TEST_HELPER_JID.to_string());

        let first = injector.push("<");
        assert!(!first.inserted);
        assert!(first.bytes.is_empty());

        assert_eq!(injector.flush(), b"<");
        assert!(injector.is_empty());
    }
}
