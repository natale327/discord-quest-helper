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

/// Result of adding an account from a captured desktop-client session.
///
/// An already-known account is reported without changing its saved or active
/// state. The captured token is intentionally never part of this IPC DTO.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddCdpResultDto {
    pub user: DiscordUser,
    pub already_known: bool,
}

/// Opaque, validated account identifier backed by a Discord snowflake.
///
/// Deterministic from [`DiscordUser::id`], serde-transparent (so it serializes as
/// a bare string) but validated on deserialization. It intentionally carries no
/// secret material.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct AccountId(String);

/// Maximum accepted length for an account id. Discord snowflakes are currently
/// 17-20 digits; the bound leaves headroom while rejecting pathological values.
const MAX_ACCOUNT_ID_LEN: usize = 32;

/// The supplied value is not a valid Discord account id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountIdError;

impl std::fmt::Display for AccountIdError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("The account id is not a valid Discord snowflake.")
    }
}

impl std::error::Error for AccountIdError {}

impl AccountId {
    /// Validate and normalize a raw Discord user id.
    pub fn parse(raw: &str) -> Result<Self, AccountIdError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.len() > MAX_ACCOUNT_ID_LEN {
            return Err(AccountIdError);
        }
        if !trimmed.chars().all(|character| character.is_ascii_digit()) {
            return Err(AccountIdError);
        }
        // Discord snowflakes are never all-zero.
        if trimmed.chars().all(|character| character == '0') {
            return Err(AccountIdError);
        }
        Ok(Self(trimmed.to_string()))
    }

    /// Build the account id for an authenticated Discord user.
    pub fn from_user(user: &DiscordUser) -> Result<Self, AccountIdError> {
        Self::parse(&user.id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AccountId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AccountId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        AccountId::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// Non-secret account presentation/binding metadata for a later account switcher.
///
/// Deliberately contains no token, password, proxy credential, or
/// super-properties field: those are process-memory only and must never be
/// persisted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccountProfile {
    pub id: AccountId,
    pub username: String,
    pub discriminator: String,
    pub avatar: Option<String>,
    pub global_name: Option<String>,
    /// Last CDP port this account was observed/logged in on, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_cdp_port: Option<u16>,
    /// Epoch milliseconds of the last login/use recorded for this account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at_ms: Option<u64>,
}

impl AccountProfile {
    pub fn from_user(user: &DiscordUser) -> Result<Self, AccountIdError> {
        Ok(Self {
            id: AccountId::from_user(user)?,
            username: user.username.clone(),
            discriminator: user.discriminator.clone(),
            avatar: user.avatar.clone(),
            global_name: user.global_name.clone(),
            last_cdp_port: None,
            last_used_at_ms: None,
        })
    }

    /// Refresh the presentation fields, last-known CDP port, and last-used
    /// timestamp from a successful authentication. Pure so the login path can
    /// build the complete candidate document before committing it.
    pub fn apply_authentication(
        &mut self,
        user: &DiscordUser,
        cdp_port: Option<u16>,
        used_at_ms: u64,
    ) {
        self.username = user.username.clone();
        self.discriminator = user.discriminator.clone();
        self.avatar = user.avatar.clone();
        self.global_name = user.global_name.clone();
        if cdp_port.is_some() {
            self.last_cdp_port = cdp_port;
        }
        self.last_used_at_ms = Some(used_at_ms);
    }
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

/// One-way input to set an account-scoped proxy override.
///
/// Every override field is optional: an absent field inherits the global proxy
/// policy. Credentials are supplied once and written only to the OS credential
/// store under an account-scoped reference. Deliberately not `Clone`, and its
/// `Debug` redacts the endpoint/no-proxy/credentials (they are unvalidated at
/// receive time).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountProxyOverrideInput {
    #[serde(default)]
    pub mode: Option<ProxyMode>,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub no_proxy: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

impl std::fmt::Debug for AccountProxyOverrideInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccountProxyOverrideInput")
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

/// Secret-free view of one account's proxy state: the explicit override (if any)
/// and the effective inherited policy. Never contains a credential reference,
/// username, or password.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccountProxySettingsDto {
    pub account_id: String,
    pub has_override: bool,
    pub override_mode: Option<ProxyMode>,
    pub override_endpoint: Option<String>,
    pub override_no_proxy: Option<String>,
    pub override_has_credentials: bool,
    pub effective_mode: ProxyMode,
    pub effective_endpoint: Option<String>,
    pub effective_no_proxy: Option<String>,
    pub effective_has_credentials: bool,
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

/// Secret-free account presentation/binding summary for the account surface.
/// Never contains a token, client, or credential reference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccountSummaryDto {
    pub id: String,
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discriminator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_cdp_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at_ms: Option<u64>,
    /// Whether this runtime currently holds an authenticated client. An activated
    /// offline (persisted-only) profile is `false`.
    pub is_authenticated: bool,
}

/// The full account list plus the active account id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccountsSnapshotDto {
    pub accounts: Vec<AccountSummaryDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_account_id: Option<String>,
}

/// Typed envelope for every account-scoped quest event. Carries the exact
/// account/quest/run identity snapped at start time; never contains account
/// secrets.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct QuestEventEnvelope {
    pub account_id: String,
    pub quest_id: String,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{AddCdpResultDto, DiscordUser};

    #[test]
    fn add_cdp_result_uses_camel_case_and_contains_no_token_field() {
        let result = AddCdpResultDto {
            user: DiscordUser {
                id: "123456789012345678".to_string(),
                username: "alice".to_string(),
                discriminator: "0".to_string(),
                avatar: None,
                global_name: Some("Alice".to_string()),
                premium_type: None,
            },
            already_known: true,
        };

        let value = serde_json::to_value(result).unwrap();
        assert_eq!(value["alreadyKnown"].as_bool(), Some(true));
        assert_eq!(value["user"]["username"], "alice");
        assert!(value.get("already_known").is_none());
        assert!(value.get("token").is_none());
    }
}
