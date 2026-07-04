use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum LaunchGame {
    Lol,
    Lor,
    Valorant,
    Lion,
    RiotClient,
}

impl LaunchGame {
    pub fn launch_product(self) -> Option<&'static str> {
        match self {
            Self::Lol => Some("league_of_legends"),
            Self::Lor => Some("bacon"),
            Self::Valorant => Some("valorant"),
            Self::Lion => Some("lion"),
            Self::RiotClient => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Lol => "League of Legends",
            Self::Lor => "Legends of Runeterra",
            Self::Valorant => "VALORANT",
            Self::Lion => "2XKO",
            Self::RiotClient => "Riot Client",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PresenceStatus {
    Chat,
    Offline,
    Mobile,
}

impl PresenceStatus {
    pub fn as_xmpp(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Offline => "offline",
            Self::Mobile => "mobile",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum StartupStatus {
    Chat,
    Offline,
    Mobile,
    Last,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSnapshot {
    pub running: bool,
    pub enabled: bool,
    pub safe_mode: bool,
    pub helper_friend: bool,
    pub status: PresenceStatus,
    pub startup_status: StartupStatus,
    pub connect_to_muc: bool,
    pub health: ConnectionHealth,
    pub chat_port: Option<u16>,
    pub config_port: Option<u16>,
    pub riot_chat_host: Option<String>,
    pub riot_chat_port: Option<u16>,
    pub riot_client_path: Option<String>,
    pub active_game: Option<LaunchGame>,
    pub active_game_label: Option<String>,
    pub logs: Vec<LogEntry>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum HealthState {
    Waiting,
    Ready,
    Active,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthStep {
    pub key: String,
    pub label: String,
    pub state: HealthState,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionHealth {
    pub config_proxy: HealthStep,
    pub config_patched: HealthStep,
    pub chat_server: HealthStep,
    pub tls_connected: HealthStep,
    pub xmpp_active: HealthStep,
    pub active_connections: usize,
    pub reconnect_attempts: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum LogCategory {
    Config,
    Chat,
    Launch,
    Error,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub timestamp: String,
    pub level: LogLevel,
    pub category: LogCategory,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreflightCheck {
    pub label: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreflightReport {
    pub ok: bool,
    pub checks: Vec<PreflightCheck>,
}
