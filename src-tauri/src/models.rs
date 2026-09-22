use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordUser {
    pub id: String,
    pub username: String,
    pub discriminator: String,
    pub avatar: Option<String>,
    pub global_name: Option<String>,
    /// Nitro subscription type: 0=None, 1=Nitro Classic, 2=Nitro, 3=Nitro Basic
    #[serde(default)]
    pub premium_type: Option<u8>,
}

/// Simplified Quest model for frontend display
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct Quest {
    pub id: String,
    pub name: String,
    pub description: String,
    pub progress: f64,
    pub seconds_needed: u32,
    pub task_type: String,
    pub application_id: String,
    pub application_name: String,
    pub application_icon: Option<String>,
    pub expires_at: Option<String>,
    pub enrolled: bool,
    pub completed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectableGame {
    pub id: String,
    pub name: String,
    pub executables: Vec<GameExecutable>,
    #[serde(alias = "icon_hash")]
    pub icon: Option<String>,
    pub type_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameExecutable {
    pub name: String,
    pub os: String,
}

// Discord API response types (legacy, kept for reference)
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct QuestsResponse {
    pub quests: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct VideoProgressPayload {
    pub timestamp: u64,
}

#[derive(Debug, Serialize)]
pub struct HeartbeatPayload {
    pub stream_key: String,
}

#[derive(Debug, Serialize)]
pub struct GameHeartbeatPayload {
    pub application_id: String,
    pub terminal: bool,
}

#[derive(Debug, Serialize)]
pub struct PlayActivityHeartbeatPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application_id: Option<String>,
    pub terminal: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayActivityHeartbeatStatus {
    pub progress_seconds: f64,
    pub completed: bool,
}

impl PlayActivityHeartbeatStatus {
    pub fn progress_percentage(self, target_seconds: u32) -> f64 {
        if self.completed {
            return 100.0;
        }
        if target_seconds == 0 {
            return 0.0;
        }
        (self.progress_seconds / target_seconds as f64 * 100.0).clamp(0.0, 99.0)
    }

    pub fn reached_target(self, target_seconds: u32) -> bool {
        self.completed || self.progress_seconds >= target_seconds as f64
    }
}

/// One live quest run as reported by `list_quest_runs`. Serialized with
/// camelCase to match the existing DTO conventions in this module.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct QuestRunDto {
    pub account_id: String,
    pub quest_id: String,
    pub run_id: String,
    pub kind: String,
    pub transport: String,
    pub phase: String,
    pub progress: f64,
}

/// Outcome of a targeted `stop_quest_run` request. `status` is one of
/// `stopped`, `alreadyFinished`, `stopTimeout`, or `runIdMismatch`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StopQuestResult {
    pub quest_id: String,
    pub run_id: Option<String>,
    pub status: String,
}

/// Outcome of `stop_all_quests`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct StopAllResult {
    pub completed: Vec<String>,
    pub timed_out: Vec<String>,
    pub cleanup_failed: Vec<String>,
}

/// Backend-owned manual CDP game simulation session. The frontend can query
/// this after remounting the simulator view so an injected game never becomes
/// impossible to stop merely because the user changed tabs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManualCdpGameSimulation {
    pub app_id: String,
    pub app_name: String,
    pub cdp_port: u16,
}

/// Machine-readable authentication progress sent over a command-scoped IPC
/// channel. Deliberately contains no token, user ID, path, or error detail.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthProgress {
    pub phase: AuthProgressPhase,
    pub current: Option<usize>,
    pub total: Option<usize>,
    pub valid_accounts: Option<usize>,
}

impl AuthProgress {
    pub fn phase(phase: AuthProgressPhase) -> Self {
        Self {
            phase,
            current: None,
            total: None,
            valid_accounts: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthProgressPhase {
    CapturingCdpSession,
    ValidatingCdpSession,
    PreparingSession,
    Complete,
}

/// Global HTTP(S) proxy mode. `System` honors OS/environment proxy detection,
/// `Direct` never uses a proxy, `Custom` uses an explicit endpoint.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum ProxyMode {
    #[default]
    System,
    Direct,
    Custom,
}

/// Public, secret-free proxy settings returned to the WebView. Never contains a
/// username, password, or credential reference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProxySettingsDto {
    pub mode: ProxyMode,
    pub endpoint: Option<String>,
    pub no_proxy: Option<String>,
    pub has_credentials: bool,
}

/// One-way input for `set_proxy_settings`. Credentials may be supplied once and
/// are written only to the OS credential store. This type is intentionally not
/// `Clone` and its `Debug` impl redacts the endpoint, no-proxy list, and
/// credentials: none of these are validated yet at construction/receive time, so
/// a stray log must not be able to print attacker-controlled values.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxySettingsInput {
    pub mode: ProxyMode,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub no_proxy: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

impl std::fmt::Debug for ProxySettingsInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProxySettingsInput")
            .field("mode", &self.mode)
            .field("endpoint", &self.endpoint.as_ref().map(|_| "<redacted>"))
            .field("no_proxy", &self.no_proxy.as_ref().map(|_| "<redacted>"))
            .field(
                "credentials",
                &if self.username.is_some() || self.password.is_some() {
                    "<redacted>"
                } else {
                    "<none>"
                },
            )
            .finish()
    }
}

/// Result of the read-only proxy connectivity probe. Contains no credentials or
/// arbitrary error text.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProxyTestResult {
    pub ok: bool,
    pub status: Option<u16>,
    pub message: String,
}
