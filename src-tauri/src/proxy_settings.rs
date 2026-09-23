//! Global HTTP(S) proxy settings (Phase 5A, hardened).
//!
//! The persisted file (`proxy.v1.json`) contains only non-secret data: mode,
//! endpoint, no-proxy list, an opaque credential reference, and a list of
//! opaque references pending deletion. Optional proxy username/password live
//! exclusively in the OS credential store (Windows Credential Manager, macOS
//! Keychain, Linux Secret Service). Secrets are never serialized, logged,
//! returned through a DTO, or held in a `Debug`/`Display`.
//!
//! Hardening guarantees:
//! * Only a *missing* settings file may default to `System`. A corrupt,
//!   unreadable, inconsistent, or unsupported-version document fails closed with
//!   an actionable redacted error, blocking proxy-dependent network activity
//!   until the settings UI repairs it.
//! * Every read/store/validate/persist/delete transaction is serialized through a
//!   runtime mutex; writes use a unique same-directory temp file so concurrent
//!   transactions cannot clobber each other.
//! * Clearing/rolling back credentials is staged: the document is validated and
//!   committed before a secret is deleted, and any deletion failure is retained
//!   as a durable pending-cleanup reference rather than losing the only handle.
//!
//! All keyring calls are blocking; callers must invoke the `ProxyRuntime`
//! methods on a blocking thread (the Tauri command layer wraps them with
//! `spawn_blocking`), which matters on Linux where the Secret Service backend is
//! synchronous.

use crate::models::{
    AccountId, AccountProxyOverrideInput, AccountProxySettingsDto, ProxyMode, ProxySettingsDto,
    ProxySettingsInput,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use zeroize::Zeroizing;

/// Fixed keyring service identifier. Derived from the app bundle identifier and
/// never influenced by user input, so credentials can only ever be read back by
/// this application.
pub const PROXY_KEYRING_SERVICE: &str = "com.masterain.discord-quest-helper.proxy";

const PROXY_SETTINGS_FILE: &str = "proxy.v1.json";
const PROXY_SETTINGS_VERSION: u32 = 1;
const MAX_ENDPOINT_LEN: usize = 2048;
const MAX_NO_PROXY_LEN: usize = 2048;
/// Bounded length of an opaque credential reference. Account-scoped references
/// prefix the owning account id, so this is larger than the global-only bound.
const MAX_CREDENTIAL_REF_LEN: usize = 96;
const MAX_CREDENTIAL_FIELD_LEN: usize = 512;

/// Settings file path inside the app config directory.
pub fn proxy_settings_path(app_config_dir: &Path) -> PathBuf {
    app_config_dir.join(PROXY_SETTINGS_FILE)
}

// ============================================================================
// Errors (never carry secrets)
// ============================================================================

/// Failures from the OS credential store, redacted for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialError {
    /// No saved credential exists for the reference.
    NoEntry,
    /// The credential store exists but is locked or unavailable (e.g. a locked
    /// keychain). The caller cannot proceed without user action.
    Locked,
    /// The credential store could not be reached at all.
    Unavailable,
    /// The store rejected the request (invalid attributes, malformed blob, ...).
    Rejected,
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            CredentialError::NoEntry => "No saved proxy credentials were found.",
            CredentialError::Locked => {
                "The OS credential store is locked or unavailable. Unlock your keychain and try again."
            }
            CredentialError::Unavailable => {
                "The OS credential store is unavailable on this system, so proxy credentials cannot be saved."
            }
            CredentialError::Rejected => {
                "The OS credential store rejected the proxy credential request."
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for CredentialError {}

/// Errors from the proxy settings layer. All variants are safe to surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxySettingsError {
    /// Caller-provided value failed validation.
    Invalid(String),
    /// Persisted settings could not be read or are structurally unusable.
    ConfigInvalid(String),
    /// Persisted settings could not be read/written on disk.
    ConfigUnavailable(String),
    /// The OS credential store failed. Never a reason to fall back to plaintext.
    Credential(CredentialError),
}

impl std::fmt::Display for ProxySettingsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProxySettingsError::Invalid(message) => formatter.write_str(message),
            ProxySettingsError::ConfigInvalid(message) => formatter.write_str(message),
            ProxySettingsError::ConfigUnavailable(message) => formatter.write_str(message),
            ProxySettingsError::Credential(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for ProxySettingsError {}

fn invalid(message: &str) -> ProxySettingsError {
    ProxySettingsError::Invalid(message.to_string())
}

// ============================================================================
// Validation / normalization (pure)
// ============================================================================

/// Validate and normalize a custom proxy endpoint. Only `http`/`https` URLs with
/// a host, no embedded userinfo, no port 0, and no query/fragment/path are
/// accepted.
pub fn validate_endpoint(raw: &str) -> Result<String, ProxySettingsError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(invalid("A proxy endpoint is required for custom mode."));
    }
    if trimmed.len() > MAX_ENDPOINT_LEN {
        return Err(invalid("The proxy endpoint is too long."));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(invalid("The proxy endpoint contains control characters."));
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err(invalid("The proxy endpoint must not contain whitespace."));
    }

    let parsed =
        url::Url::parse(trimmed).map_err(|_| invalid("The proxy endpoint is not a valid URL."))?;
    match parsed.scheme() {
        "http" | "https" => {}
        _ => {
            return Err(invalid(
                "The proxy endpoint must use the http or https scheme.",
            ))
        }
    }
    if parsed.host_str().map(str::is_empty).unwrap_or(true) {
        return Err(invalid("The proxy endpoint is missing a host."));
    }
    if parsed.port() == Some(0) {
        return Err(invalid(
            "The proxy endpoint port must be between 1 and 65535.",
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(invalid(
            "The proxy endpoint must not embed a username or password; save credentials separately.",
        ));
    }
    if parsed.query().is_some() {
        return Err(invalid(
            "The proxy endpoint must not include a query string.",
        ));
    }
    if parsed.fragment().is_some() {
        return Err(invalid("The proxy endpoint must not include a fragment."));
    }
    if !parsed.path().is_empty() && parsed.path() != "/" {
        return Err(invalid("The proxy endpoint must not include a path."));
    }

    let normalized = parsed.as_str().trim_end_matches('/').to_string();
    if normalized.is_empty() {
        return Err(invalid("The proxy endpoint is not a valid URL."));
    }
    Ok(normalized)
}

/// Validate an optional no-proxy list. Empty becomes `None`. Bounded, free of
/// control characters/whitespace, and every comma-separated entry must be a
/// conservative host, IP, host:port, CIDR, or `*`.
pub fn validate_no_proxy(raw: Option<&str>) -> Result<Option<String>, ProxySettingsError> {
    let Some(value) = raw else { return Ok(None) };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.len() > MAX_NO_PROXY_LEN {
        return Err(invalid("The no-proxy list is too long."));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(invalid("The no-proxy list contains control characters."));
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err(invalid("The no-proxy list must not contain whitespace."));
    }
    for entry in trimmed.split(',') {
        if !is_valid_no_proxy_entry(entry) {
            return Err(invalid("The no-proxy list contains an invalid entry."));
        }
    }
    Ok(Some(trimmed.to_string()))
}

fn is_valid_no_proxy_entry(entry: &str) -> bool {
    if entry.is_empty() {
        return false;
    }
    if entry == "*" {
        return true;
    }
    if entry.contains('@')
        || entry.contains('?')
        || entry.contains('#')
        || entry.contains('\\')
        || entry.contains(' ')
    {
        return false;
    }
    // Bare IPv4/IPv6 literal.
    if entry.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    // CIDR: IPv4 or IPv6 with a bounded prefix.
    if let Some((host, prefix)) = entry.split_once('/') {
        let Ok(address) = host.parse::<std::net::IpAddr>() else {
            return false;
        };
        let Ok(prefix) = prefix.parse::<u8>() else {
            return false;
        };
        return match address {
            std::net::IpAddr::V4(_) => prefix <= 32,
            std::net::IpAddr::V6(_) => prefix <= 128,
        };
    }
    // host:port or [ipv6]:port.
    if let Some((host, port)) = split_host_port(entry) {
        let Ok(port) = port.parse::<u16>() else {
            return false;
        };
        return port > 0 && is_valid_host_token(host);
    }
    is_valid_host_token(entry)
}

fn split_host_port(entry: &str) -> Option<(&str, &str)> {
    if let Some(rest) = entry.strip_prefix('[') {
        let (host, remainder) = rest.split_once(']')?;
        let port = remainder.strip_prefix(':')?;
        return Some((host, port));
    }
    let (host, port) = entry.rsplit_once(':')?;
    if host.is_empty() || port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((host, port))
}

fn is_valid_host_token(host: &str) -> bool {
    if host.is_empty() {
        return false;
    }
    if host == "*" {
        return true;
    }
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let host = host.strip_prefix('.').unwrap_or(host);
    if host.is_empty() {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// Validate an optional credential field (username/password).
fn validate_credential_field(raw: &str, label: &str) -> Result<String, ProxySettingsError> {
    if raw.is_empty() {
        return Err(invalid(&format!("The proxy {label} must not be empty.")));
    }
    if raw.len() > MAX_CREDENTIAL_FIELD_LEN {
        return Err(invalid(&format!("The proxy {label} is too long.")));
    }
    if raw.chars().any(char::is_control) {
        return Err(invalid(&format!(
            "The proxy {label} contains control characters."
        )));
    }
    Ok(raw.to_string())
}

/// Generate a fresh opaque credential reference. `uuid` v4 with the simple
/// (32 hex) form, so it can never contain a `.` and is safe as a Windows keyring
/// target fragment.
pub fn generate_credential_ref() -> String {
    format!("c{}", uuid::Uuid::new_v4().simple())
}

/// Generate a fresh opaque, account-scoped credential reference. The account id
/// and a `-` delimiter prefix a unique token, so two accounts can never share or
/// collide on a keyring entry and the reference stays delimiter-safe (no `.`).
pub fn generate_account_credential_ref(account_id: &AccountId) -> String {
    format!("a{}-{}", account_id.as_str(), generate_credential_ref())
}

/// Normalize/validate a stored credential reference. Rejects `.` (Windows keyring
/// target collisions) and any character outside `[A-Za-z0-9_-]`.
pub fn normalize_credential_ref(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_CREDENTIAL_REF_LEN {
        return None;
    }
    if trimmed.contains('.') {
        return None;
    }
    if !trimmed
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '_')
    {
        return None;
    }
    Some(trimmed.to_string())
}

// ============================================================================
// Secrets
// ============================================================================

/// A proxy username/password pair held in zeroizing memory. Nothing here is
/// serializable to the WebView or printable.
#[derive(Clone)]
pub struct ProxyCredentials {
    username: Zeroizing<String>,
    password: Zeroizing<String>,
}

impl ProxyCredentials {
    pub fn new(username: String, password: String) -> Self {
        Self {
            username: Zeroizing::new(username),
            password: Zeroizing::new(password),
        }
    }

    pub fn username(&self) -> &str {
        self.username.as_str()
    }

    pub fn password(&self) -> &str {
        self.password.as_str()
    }

    /// Serialize to the small secret blob stored in the OS keyring. The returned
    /// string zeroizes on drop.
    pub(crate) fn to_stored_json(&self) -> Zeroizing<String> {
        #[derive(Serialize)]
        struct Stored<'a> {
            username: &'a str,
            password: &'a str,
        }
        let json = serde_json::to_string(&Stored {
            username: self.username(),
            password: self.password(),
        })
        .unwrap_or_else(|_| String::from("{}"));
        Zeroizing::new(json)
    }

    pub(crate) fn from_stored_json(raw: &str) -> Result<Self, CredentialError> {
        #[derive(Deserialize)]
        struct Stored {
            username: String,
            password: String,
        }
        let parsed: Stored = serde_json::from_str(raw).map_err(|_| CredentialError::Rejected)?;
        Ok(Self::new(parsed.username, parsed.password))
    }
}

impl std::fmt::Debug for ProxyCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ProxyCredentials { username: <redacted>, password: <redacted> }")
    }
}

// ============================================================================
// Credential store abstraction
// ============================================================================

/// Injectable credential store so tests never touch a real OS keychain.
pub trait CredentialStore: Send + Sync {
    fn store(&self, reference: &str, credentials: &ProxyCredentials)
        -> Result<(), CredentialError>;
    fn load(&self, reference: &str) -> Result<Option<ProxyCredentials>, CredentialError>;
    /// Idempotent: deleting a missing entry is success.
    fn delete(&self, reference: &str) -> Result<(), CredentialError>;
}

/// Production store backed by the OS credential store via `keyring`.
pub struct KeyringCredentialStore;

impl KeyringCredentialStore {
    fn entry(reference: &str) -> Result<keyring::Entry, CredentialError> {
        keyring::Entry::new(PROXY_KEYRING_SERVICE, reference).map_err(map_keyring_error)
    }
}

fn map_keyring_error(error: keyring::Error) -> CredentialError {
    match error {
        keyring::Error::NoEntry => CredentialError::NoEntry,
        keyring::Error::NoStorageAccess(_) | keyring::Error::NoDefaultStore => {
            CredentialError::Locked
        }
        keyring::Error::PlatformFailure(_) | keyring::Error::NotSupportedByStore(_) => {
            CredentialError::Unavailable
        }
        keyring::Error::TooLong(..)
        | keyring::Error::Invalid(..)
        | keyring::Error::BadEncoding(_)
        | keyring::Error::BadDataFormat(..)
        | keyring::Error::BadStoreFormat(_)
        | keyring::Error::Ambiguous(_) => CredentialError::Rejected,
        _ => CredentialError::Unavailable,
    }
}

impl CredentialStore for KeyringCredentialStore {
    fn store(
        &self,
        reference: &str,
        credentials: &ProxyCredentials,
    ) -> Result<(), CredentialError> {
        let entry = Self::entry(reference)?;
        let secret = credentials.to_stored_json();
        entry
            .set_password(secret.as_str())
            .map_err(map_keyring_error)
    }

    fn load(&self, reference: &str) -> Result<Option<ProxyCredentials>, CredentialError> {
        let entry = Self::entry(reference)?;
        match entry.get_password() {
            Ok(secret) => {
                let secret = Zeroizing::new(secret);
                ProxyCredentials::from_stored_json(secret.as_str()).map(Some)
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(map_keyring_error(error)),
        }
    }

    fn delete(&self, reference: &str) -> Result<(), CredentialError> {
        let entry = Self::entry(reference)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(map_keyring_error(error)),
        }
    }
}

// ============================================================================
// Persisted shape (secret-free, versioned)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PersistedGlobalProxy {
    #[serde(default)]
    pub mode: ProxyMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_proxy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,
}

impl Default for PersistedGlobalProxy {
    fn default() -> Self {
        Self {
            mode: ProxyMode::System,
            endpoint: None,
            no_proxy: None,
            credential_ref: None,
        }
    }
}

/// A per-account proxy override. Every field is optional; an absent field
/// inherits the global policy (see [`effective_for_account`]). Unknown fields are
/// round-tripped untouched in `extra` so a newer document is never silently
/// destroyed on a repair/save.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccountProxyOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<ProxyMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_proxy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Versioned persisted document. `accounts` is a typed `accountId -> override`
/// map. `pending_cleanup` holds opaque references whose deletion failed and that
/// must be retried; it never contains a secret.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PersistedProxySettings {
    #[serde(default = "default_settings_version")]
    pub version: u32,
    #[serde(default)]
    pub global: PersistedGlobalProxy,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_cleanup: Vec<String>,
    #[serde(
        default,
        skip_serializing_if = "HashMap::is_empty",
        deserialize_with = "deserialize_accounts"
    )]
    pub accounts: HashMap<AccountId, AccountProxyOverride>,
}

/// Accept both a missing/`null` reserved slot (older documents) and a typed map.
/// Any other shape (array/string/number) or a malformed account id fails the
/// whole document closed as a config error.
fn deserialize_accounts<'de, D>(
    deserializer: D,
) -> Result<HashMap<AccountId, AccountProxyOverride>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<HashMap<AccountId, AccountProxyOverride>>::deserialize(deserializer)?;
    Ok(value.unwrap_or_default())
}

fn default_settings_version() -> u32 {
    PROXY_SETTINGS_VERSION
}

impl Default for PersistedProxySettings {
    fn default() -> Self {
        Self {
            version: PROXY_SETTINGS_VERSION,
            global: PersistedGlobalProxy::default(),
            pending_cleanup: Vec::new(),
            accounts: HashMap::new(),
        }
    }
}

fn push_unique(references: &mut Vec<String>, reference: &str) {
    if !references.iter().any(|existing| existing == reference) {
        references.push(reference.to_string());
    }
}

// ============================================================================
// Validated persisted state
// ============================================================================

/// The persisted document after structural validation. Public DTOs and effective
/// configurations are only ever derived from this, never from raw file content.
#[derive(Debug, Clone)]
struct ValidatedGlobal {
    mode: ProxyMode,
    endpoint: Option<String>,
    no_proxy: Option<String>,
    credential_ref: Option<String>,
    pending_cleanup: Vec<String>,
}

fn validated_global(
    persisted: &PersistedProxySettings,
) -> Result<ValidatedGlobal, ProxySettingsError> {
    if persisted.version != PROXY_SETTINGS_VERSION {
        return Err(ProxySettingsError::ConfigInvalid(
            "The saved proxy settings use an unsupported version. Open proxy settings to migrate them."
                .to_string(),
        ));
    }

    let mut pending_cleanup = Vec::with_capacity(persisted.pending_cleanup.len());
    for reference in &persisted.pending_cleanup {
        let normalized = normalize_credential_ref(reference).ok_or_else(|| {
            ProxySettingsError::ConfigInvalid(
                "The saved proxy settings contain an invalid cleanup reference.".to_string(),
            )
        })?;
        push_unique(&mut pending_cleanup, &normalized);
    }

    let global = &persisted.global;
    match global.mode {
        ProxyMode::System | ProxyMode::Direct => {
            if global.endpoint.is_some()
                || global.no_proxy.is_some()
                || global.credential_ref.is_some()
            {
                return Err(ProxySettingsError::ConfigInvalid(
                    "The saved proxy settings are inconsistent with the selected mode.".to_string(),
                ));
            }
            Ok(ValidatedGlobal {
                mode: global.mode,
                endpoint: None,
                no_proxy: None,
                credential_ref: None,
                pending_cleanup,
            })
        }
        ProxyMode::Custom => {
            let endpoint = global.endpoint.as_deref().ok_or_else(|| {
                ProxySettingsError::ConfigInvalid(
                    "The saved custom proxy is missing its endpoint.".to_string(),
                )
            })?;
            let endpoint = validate_endpoint(endpoint)?;
            let no_proxy = validate_no_proxy(global.no_proxy.as_deref())?;
            let credential_ref = match global.credential_ref.as_deref() {
                Some(reference) => Some(normalize_credential_ref(reference).ok_or_else(|| {
                    ProxySettingsError::ConfigInvalid(
                        "The saved proxy credential reference is invalid.".to_string(),
                    )
                })?),
                None => None,
            };
            Ok(ValidatedGlobal {
                mode: ProxyMode::Custom,
                endpoint: Some(endpoint),
                no_proxy,
                credential_ref,
                pending_cleanup,
            })
        }
    }
}

// ============================================================================
// Account overrides: validation, inheritance, effective resolution
// ============================================================================

/// A structurally validated per-account override (endpoint/no-proxy/ref
/// normalized and bounded).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ValidatedAccountOverride {
    mode: Option<ProxyMode>,
    endpoint: Option<String>,
    no_proxy: Option<String>,
    credential_ref: Option<String>,
}

/// The fully validated document: global policy plus typed account overrides.
#[derive(Debug, Clone)]
struct ValidatedSettings {
    global: ValidatedGlobal,
    accounts: HashMap<AccountId, ValidatedAccountOverride>,
}

/// The effective (inherited) policy for one account, before credentials are
/// loaded from the OS store. Contains no secret material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveProxy {
    pub mode: ProxyMode,
    pub endpoint: Option<String>,
    pub no_proxy: Option<String>,
    pub credential_ref: Option<String>,
}

/// Inheritance semantics (documented, fail-closed):
/// * `mode` absent -> effective mode is the global mode.
/// * System/Direct effective mode clears endpoint/no-proxy/credentials; an
///   override carrying an endpoint or credential with a non-custom effective mode
///   is a config error.
/// * Custom effective mode: `endpoint`/`no_proxy`/`credential_ref` each fall back
///   to the global value when absent. A custom policy with no resolvable endpoint
///   is a config error.
fn effective_for_account(
    global: &ValidatedGlobal,
    override_: Option<&ValidatedAccountOverride>,
) -> Result<EffectiveProxy, ProxySettingsError> {
    let mode = override_
        .and_then(|value| value.mode)
        .unwrap_or(global.mode);
    let endpoint_override = override_.and_then(|value| value.endpoint.clone());
    let no_proxy_override = override_.and_then(|value| value.no_proxy.clone());
    let credential_override = override_.and_then(|value| value.credential_ref.clone());

    match mode {
        ProxyMode::System | ProxyMode::Direct => {
            if endpoint_override.is_some() || credential_override.is_some() {
                return Err(ProxySettingsError::ConfigInvalid(
                    "A proxy override sets an endpoint or credential for a non-custom mode."
                        .to_string(),
                ));
            }
            Ok(EffectiveProxy {
                mode,
                endpoint: None,
                no_proxy: None,
                credential_ref: None,
            })
        }
        ProxyMode::Custom => {
            let endpoint = endpoint_override.or_else(|| global.endpoint.clone());
            let endpoint = endpoint.ok_or_else(|| {
                ProxySettingsError::ConfigInvalid(
                    "A custom proxy has no endpoint and the global policy has none to inherit."
                        .to_string(),
                )
            })?;
            Ok(EffectiveProxy {
                mode: ProxyMode::Custom,
                endpoint: Some(endpoint),
                no_proxy: no_proxy_override.or_else(|| global.no_proxy.clone()),
                credential_ref: credential_override.or_else(|| global.credential_ref.clone()),
            })
        }
    }
}

/// Pure core: resolve the effective policy for an account from validated state.
/// Never touches the credential store, so it is fully unit-testable.
fn effective_for_account_document(
    settings: &ValidatedSettings,
    account_id: &AccountId,
) -> Result<EffectiveProxy, ProxySettingsError> {
    effective_for_account(&settings.global, settings.accounts.get(account_id))
}

/// Validate the whole document (global plus every account override). Any
/// malformed/unknown-version/partial override fails the document closed.
fn validated_settings(
    persisted: &PersistedProxySettings,
) -> Result<ValidatedSettings, ProxySettingsError> {
    let global = validated_global(persisted)?;
    let mut accounts = HashMap::with_capacity(persisted.accounts.len());
    for (account_id, raw) in &persisted.accounts {
        let endpoint = match raw.endpoint.as_deref() {
            Some(value) => Some(validate_endpoint(value)?),
            None => None,
        };
        let no_proxy = validate_no_proxy(raw.no_proxy.as_deref())?;
        let credential_ref = match raw.credential_ref.as_deref() {
            Some(reference) => {
                let normalized = normalize_credential_ref(reference).ok_or_else(|| {
                    ProxySettingsError::ConfigInvalid(
                        "A saved account proxy credential reference is invalid.".to_string(),
                    )
                })?;
                // Ownership check: an account may only reference a credential
                // stored under its own documented `a{accountId}-` prefix. A
                // syntax-valid but forged/cross-account reference fails closed.
                let prefix = format!("a{}-", account_id.as_str());
                if !normalized.starts_with(&prefix) {
                    return Err(ProxySettingsError::ConfigInvalid(
                        "A saved account proxy credential reference does not belong to this account."
                            .to_string(),
                    ));
                }
                Some(normalized)
            }
            None => None,
        };
        let candidate = ValidatedAccountOverride {
            mode: raw.mode,
            endpoint,
            no_proxy,
            credential_ref,
        };
        // This also validates inheritance against the global document.
        effective_for_account(&global, Some(&candidate))?;
        accounts.insert(account_id.clone(), candidate);
    }
    Ok(ValidatedSettings { global, accounts })
}

fn account_dto(
    account_id: &AccountId,
    raw: Option<&AccountProxyOverride>,
    override_: Option<&ValidatedAccountOverride>,
    effective: &EffectiveProxy,
) -> AccountProxySettingsDto {
    AccountProxySettingsDto {
        account_id: account_id.as_str().to_string(),
        has_override: override_.is_some(),
        override_mode: raw.and_then(|value| value.mode),
        override_endpoint: override_.and_then(|value| value.endpoint.clone()),
        override_no_proxy: override_.and_then(|value| value.no_proxy.clone()),
        override_has_credentials: override_
            .and_then(|value| value.credential_ref.as_ref())
            .is_some(),
        effective_mode: effective.mode,
        effective_endpoint: effective.endpoint.clone(),
        effective_no_proxy: effective.no_proxy.clone(),
        effective_has_credentials: effective.credential_ref.is_some(),
    }
}

fn dto_from_validated(validated: &ValidatedGlobal, has_credentials: bool) -> ProxySettingsDto {
    ProxySettingsDto {
        mode: validated.mode,
        endpoint: validated.endpoint.clone(),
        no_proxy: validated.no_proxy.clone(),
        has_credentials,
    }
}

// ============================================================================
// Effective configuration
// ============================================================================

/// Custom proxy details including credentials resolved from the OS store.
#[derive(Clone)]
pub struct CustomProxyConfiguration {
    pub endpoint: String,
    pub no_proxy: Option<String>,
    pub credentials: Option<Arc<ProxyCredentials>>,
}

/// Effective proxy policy handed to the HTTP client builder.
#[derive(Clone)]
pub enum ProxyConfiguration {
    /// Honor OS/environment detection (existing behavior).
    System,
    /// Never use any proxy.
    Direct,
    Custom(CustomProxyConfiguration),
}

impl ProxyConfiguration {
    pub fn mode(&self) -> ProxyMode {
        match self {
            ProxyConfiguration::System => ProxyMode::System,
            ProxyConfiguration::Direct => ProxyMode::Direct,
            ProxyConfiguration::Custom(_) => ProxyMode::Custom,
        }
    }

    #[allow(dead_code)]
    pub fn has_credentials(&self) -> bool {
        matches!(
            self,
            ProxyConfiguration::Custom(CustomProxyConfiguration {
                credentials: Some(_),
                ..
            })
        )
    }
}

impl std::fmt::Debug for ProxyConfiguration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProxyConfiguration::System => formatter.write_str("ProxyConfiguration::System"),
            ProxyConfiguration::Direct => formatter.write_str("ProxyConfiguration::Direct"),
            ProxyConfiguration::Custom(custom) => formatter
                .debug_struct("ProxyConfiguration::Custom")
                .field("endpoint", &custom.endpoint)
                .field("no_proxy", &custom.no_proxy)
                .field(
                    "credentials",
                    &if custom.credentials.is_some() {
                        "<redacted>"
                    } else {
                        "<none>"
                    },
                )
                .finish(),
        }
    }
}

/// An opaque, already-built transport handed back by a backend during a
/// transaction. `proxy_settings` never inspects it.
pub struct PreparedProxyTransport(Box<dyn std::any::Any + Send>);

impl PreparedProxyTransport {
    pub fn new<T: Send + 'static>(value: T) -> Self {
        Self(Box::new(value))
    }

    pub fn into_inner<T: Send + 'static>(self) -> Option<T> {
        self.0.downcast::<T>().ok().map(|boxed| *boxed)
    }
}

/// A replacement transport staged for one live account during a batch rebuild.
struct PreparedAccountClient {
    account_id: AccountId,
    configuration: ProxyConfiguration,
    transport: PreparedProxyTransport,
}

/// Backend hook that rebuilds every *live* account client for a proxy-document
/// change. `prepare` must not mutate live state and runs before persistence; if
/// any account fails to prepare the whole transaction aborts. `install` runs
/// after persistence and swaps each account's transport atomically.
pub trait AccountClientBackend: Send + Sync {
    /// Account ids whose live client must be rebuilt (online accounts only).
    fn live_accounts(&self) -> Vec<AccountId>;
    fn prepare(
        &self,
        account_id: &AccountId,
        configuration: &ProxyConfiguration,
    ) -> Result<PreparedProxyTransport, String>;
    fn install(
        &self,
        account_id: &AccountId,
        configuration: &ProxyConfiguration,
        prepared: PreparedProxyTransport,
    ) -> Result<(), String>;
}

// ============================================================================
// Runtime
// ============================================================================

/// Serializes every proxy transaction and performs the (blocking) keyring work.
pub struct ProxyRuntime {
    store: Arc<dyn CredentialStore>,
    settings_path: PathBuf,
    transaction: Mutex<()>,
}

impl ProxyRuntime {
    pub fn new(store: Arc<dyn CredentialStore>, settings_path: PathBuf) -> Self {
        Self {
            store,
            settings_path,
            transaction: Mutex::new(()),
        }
    }

    /// Guard the complete proxy transaction. Public so the command layer can hold
    /// it across persistence plus live-client installation.
    pub fn transaction_lock(&self) -> MutexGuard<'_, ()> {
        self.transaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Public, secret-free snapshot built only from validated state.
    ///
    /// `hasCredentials` is reference-derived: it reflects whether a credential
    /// reference is saved. A live keyring probe is intentionally not performed
    /// here (it would block and could not be represented without changing the
    /// response contract); `resolve_current` fails closed if the referenced
    /// secret is actually missing.
    pub fn read_dto(&self) -> Result<ProxySettingsDto, ProxySettingsError> {
        let _transaction = self.transaction_lock();
        let persisted = load_persisted_from(&self.settings_path)?;
        let validated = validated_global(&persisted)?;
        Ok(dto_from_validated(
            &validated,
            validated.credential_ref.is_some(),
        ))
    }

    /// Load and validate the saved policy at startup. A failure is reported, not
    /// swallowed; network resolution later fails closed until repaired.
    pub fn refresh_from_disk(&self) -> Result<ProxySettingsDto, ProxySettingsError> {
        let _transaction = self.transaction_lock();
        let persisted = load_persisted_from(&self.settings_path)?;
        let validated = validated_global(&persisted)?;
        // Resolve (and therefore fully validate) the policy; the value is dropped
        // because the live transport is owned by the Discord client.
        self.configuration_from_validated(&validated, true)?;
        self.drain_pending_cleanup_best_effort();
        Ok(dto_from_validated(
            &validated,
            validated.credential_ref.is_some(),
        ))
    }

    /// Resolve the persisted policy into an effective configuration, failing
    /// closed on any corrupt/incomplete/unreadable state. Only a missing file
    /// yields the default `System` policy (handled inside `load_persisted_from`).
    pub fn resolve_current(&self) -> Result<ProxyConfiguration, ProxySettingsError> {
        let _transaction = self.transaction_lock();
        let persisted = load_persisted_from(&self.settings_path)?;
        let validated = validated_global(&persisted)?;
        self.configuration_from_validated(&validated, true)
    }

    /// Apply a new proxy policy transactionally:
    ///
    /// 1. validate input (credentials zeroized immediately),
    /// 2. stage/write credentials to the OS store,
    /// 3. validate the candidate document and resolve the effective policy,
    /// 4. **build the replacement transport before persisting**,
    /// 5. persist, install, then drain superseded credentials.
    ///
    /// Any failure before persistence leaves the on-disk state untouched.
    pub fn set(
        &self,
        input: ProxySettingsInput,
        backend: &dyn AccountClientBackend,
    ) -> Result<(ProxySettingsDto, ProxyConfiguration), ProxySettingsError> {
        let _transaction = self.transaction_lock();

        // Wrap credentials in zeroizing storage before any early return so an
        // invalid request cannot leave plaintext behind.
        let ProxySettingsInput {
            mode,
            endpoint,
            no_proxy,
            username,
            password,
        } = input;
        let username = username.map(Zeroizing::new);
        let password = password.map(Zeroizing::new);

        let (persisted, old) = self.load_for_update()?;
        let old_ref = old.global.credential_ref.clone();

        let (validated_endpoint, validated_no_proxy) = match mode {
            ProxyMode::Custom => (
                Some(validate_endpoint(endpoint.as_deref().unwrap_or_default())?),
                validate_no_proxy(no_proxy.as_deref())?,
            ),
            _ => (None, None),
        };

        let supplied = match (username.as_ref(), password.as_ref()) {
            (None, None) => None,
            (Some(username), Some(password)) => Some((
                validate_credential_field(username, "username")?,
                validate_credential_field(password, "password")?,
            )),
            _ => return Err(invalid(
                "Both a proxy username and password are required when credentials are supplied.",
            )),
        };
        if supplied.is_some() && mode != ProxyMode::Custom {
            return Err(invalid(
                "Proxy credentials are only supported with the custom proxy mode.",
            ));
        }

        // Credentials are only carried by custom mode. Other modes drop them.
        let mut new_ref = if mode == ProxyMode::Custom {
            old_ref.clone()
        } else {
            None
        };
        let mut created_ref: Option<String> = None;
        if let Some((username, password)) = supplied {
            let credentials = ProxyCredentials::new(username, password);
            let reference = generate_credential_ref();
            self.store
                .store(&reference, &credentials)
                .map_err(ProxySettingsError::Credential)?;
            created_ref = Some(reference.clone());
            new_ref = Some(reference);
        }

        let mut candidate = persisted.clone();
        candidate.version = PROXY_SETTINGS_VERSION;
        candidate.global = PersistedGlobalProxy {
            mode,
            endpoint: validated_endpoint,
            no_proxy: validated_no_proxy,
            credential_ref: new_ref.clone(),
        };
        candidate.pending_cleanup = old.global.pending_cleanup.clone();
        if let Some(old) = old_ref.as_deref() {
            if Some(old) != new_ref.as_deref() {
                push_unique(&mut candidate.pending_cleanup, old);
            }
        }

        let validated = match validated_settings(&candidate) {
            Ok(validated) => validated,
            Err(error) => {
                self.discard_created_credential(&created_ref);
                return Err(error);
            }
        };
        let configuration = match self.configuration_from_validated(&validated.global, true) {
            Ok(configuration) => configuration,
            Err(error) => {
                self.discard_created_credential(&created_ref);
                return Err(error);
            }
        };

        // Build (and therefore validate) a replacement transport for EVERY live
        // account using its own effective candidate policy (an overridden account
        // keeps its override; an inheriting account reflects the new global).
        let prepared = match self.prepare_account_clients(&validated, backend) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.discard_created_credential(&created_ref);
                return Err(error);
            }
        };

        if let Err(error) = save_persisted_to(&self.settings_path, &candidate) {
            self.discard_created_credential(&created_ref);
            return Err(error);
        }

        if let Err(error) = self.install_account_clients(prepared, backend) {
            // The swap could not be applied: restore the previous document so we
            // never report success while traffic still uses the old policy.
            let _ = save_persisted_to(&self.settings_path, &persisted);
            self.discard_created_credential(&created_ref);
            return Err(error);
        }

        self.drain_pending_cleanup_best_effort();

        Ok((
            dto_from_validated(&validated.global, validated.global.credential_ref.is_some()),
            configuration,
        ))
    }

    /// Remove the saved credential, rollback-safe. The credential-free document
    /// is validated and committed first; the secret is only deleted afterwards,
    /// and a deletion failure is retained as a pending-cleanup reference.
    pub fn clear_credentials(
        &self,
        backend: &dyn AccountClientBackend,
    ) -> Result<(ProxySettingsDto, ProxyConfiguration), ProxySettingsError> {
        let _transaction = self.transaction_lock();

        let (persisted, old) = self.load_for_update()?;
        let mut candidate = persisted.clone();
        candidate.version = PROXY_SETTINGS_VERSION;
        candidate.global.credential_ref = None;
        candidate.pending_cleanup = old.global.pending_cleanup.clone();
        if let Some(reference) = old.global.credential_ref.as_deref() {
            push_unique(&mut candidate.pending_cleanup, reference);
        }

        let validated = validated_settings(&candidate)?;
        let configuration = self.configuration_from_validated(&validated.global, true)?;
        // Batch rebuild every live account with its effective candidate policy.
        let prepared = self.prepare_account_clients(&validated, backend)?;

        // Commit the credential-free document before deleting the secret so a
        // later failure cannot strand the config pointing at a missing entry.
        save_persisted_to(&self.settings_path, &candidate)?;

        if let Err(error) = self.install_account_clients(prepared, backend) {
            let _ = save_persisted_to(&self.settings_path, &persisted);
            return Err(error);
        }

        self.drain_pending_cleanup_best_effort();

        Ok((dto_from_validated(&validated.global, false), configuration))
    }

    /// Load the on-disk document for a mutating transaction, falling back to the
    /// default only when the document is corrupt/inconsistent (so the settings UI
    /// can repair it). I/O failures still propagate.
    fn load_for_update(
        &self,
    ) -> Result<(PersistedProxySettings, ValidatedSettings), ProxySettingsError> {
        let document = match load_persisted_from(&self.settings_path) {
            Ok(document) => document,
            Err(ProxySettingsError::ConfigInvalid(_)) => {
                use crate::logger::{log, LogCategory, LogLevel};
                log(
                    LogLevel::Warn,
                    LogCategory::Api,
                    "Saved proxy settings were invalid and are being replaced",
                    None,
                );
                PersistedProxySettings::default()
            }
            Err(error) => return Err(error),
        };

        match validated_settings(&document) {
            Ok(validated) => Ok((document, validated)),
            Err(_) => {
                // A malformed/unknown-version document (including a malformed
                // accounts slot) is replaced so the settings UI can repair it.
                let document = PersistedProxySettings::default();
                let validated = validated_settings(&document)
                    .expect("the default persisted document is always valid");
                Ok((document, validated))
            }
        }
    }

    /// Best-effort deletion of a credential we created but could not use. If the
    /// store refuses the deletion, retain a durable retry handle instead of
    /// silently losing track of the secret.
    fn discard_created_credential(&self, created_ref: &Option<String>) {
        let Some(reference) = created_ref.as_deref() else {
            return;
        };
        if self.store.delete(reference).is_ok() {
            return;
        }
        if let Ok(mut document) = load_persisted_from(&self.settings_path) {
            push_unique(&mut document.pending_cleanup, reference);
            let _ = save_persisted_to(&self.settings_path, &document);
        }
    }

    /// Retry deletions of superseded credentials. Un-deletable references stay in
    /// the durable document for a later attempt.
    fn drain_pending_cleanup_best_effort(&self) {
        let Ok(mut document) = load_persisted_from(&self.settings_path) else {
            return;
        };
        if document.pending_cleanup.is_empty() {
            return;
        }
        let original = std::mem::take(&mut document.pending_cleanup);
        let mut remaining = Vec::new();
        for reference in original.iter() {
            if self.store.delete(reference).is_err() {
                remaining.push(reference.clone());
            }
        }
        if remaining.len() != original.len() {
            document.pending_cleanup = remaining;
            let _ = save_persisted_to(&self.settings_path, &document);
        }
    }

    fn configuration_from_validated(
        &self,
        validated: &ValidatedGlobal,
        load_credentials: bool,
    ) -> Result<ProxyConfiguration, ProxySettingsError> {
        let effective = EffectiveProxy {
            mode: validated.mode,
            endpoint: validated.endpoint.clone(),
            no_proxy: validated.no_proxy.clone(),
            credential_ref: validated.credential_ref.clone(),
        };
        self.configuration_from_effective(&effective, load_credentials)
    }

    /// Build the effective configuration for an already-validated policy. Fails
    /// closed when a referenced credential is missing from the store.
    fn configuration_from_effective(
        &self,
        effective: &EffectiveProxy,
        load_credentials: bool,
    ) -> Result<ProxyConfiguration, ProxySettingsError> {
        match effective.mode {
            ProxyMode::System => Ok(ProxyConfiguration::System),
            ProxyMode::Direct => Ok(ProxyConfiguration::Direct),
            ProxyMode::Custom => {
                let endpoint = effective.endpoint.clone().ok_or_else(|| {
                    ProxySettingsError::ConfigInvalid(
                        "The saved custom proxy is missing its endpoint.".to_string(),
                    )
                })?;
                let endpoint = validate_endpoint(&endpoint)?;
                let credentials = if load_credentials {
                    match effective.credential_ref.as_deref() {
                        Some(reference) => {
                            match self
                                .store
                                .load(reference)
                                .map_err(ProxySettingsError::Credential)?
                            {
                                Some(credentials) => Some(Arc::new(credentials)),
                                None => {
                                    return Err(ProxySettingsError::ConfigInvalid(
                                    "Saved proxy credentials are missing from the OS credential store. Re-enter them in proxy settings."
                                        .to_string(),
                                ))
                                }
                            }
                        }
                        None => None,
                    }
                } else {
                    None
                };
                Ok(ProxyConfiguration::Custom(CustomProxyConfiguration {
                    endpoint,
                    no_proxy: effective.no_proxy.clone(),
                    credentials,
                }))
            }
        }
    }

    /// Resolve the effective policy for `account_id`: the account override merged
    /// over the global policy over the system default. Fails closed on any
    /// corrupt document or stranded credential reference. Never returns secret
    /// material (credentials are resolved only to build the client).
    #[allow(dead_code)] // Public API for the Phase 6.4 account surface.
    pub fn resolve_for_account(
        &self,
        account_id: &AccountId,
    ) -> Result<ProxyConfiguration, ProxySettingsError> {
        let _transaction = self.transaction_lock();
        let persisted = load_persisted_from(&self.settings_path)?;
        let settings = validated_settings(&persisted)?;
        let effective = effective_for_account_document(&settings, account_id)?;
        self.configuration_from_effective(&effective, true)
    }

    /// Secret-free DTO for one account: its explicit override (if any) plus the
    /// effective inherited policy. Never exposes a credential reference.
    pub fn read_account_dto(
        &self,
        account_id: &AccountId,
    ) -> Result<AccountProxySettingsDto, ProxySettingsError> {
        let _transaction = self.transaction_lock();
        let persisted = load_persisted_from(&self.settings_path)?;
        let settings = validated_settings(&persisted)?;
        self.account_dto_locked(&persisted, &settings, account_id)
    }

    fn account_dto_locked(
        &self,
        persisted: &PersistedProxySettings,
        settings: &ValidatedSettings,
        account_id: &AccountId,
    ) -> Result<AccountProxySettingsDto, ProxySettingsError> {
        let raw = persisted.accounts.get(account_id);
        let override_ = settings.accounts.get(account_id);
        let effective = effective_for_account(&settings.global, override_)?;
        Ok(account_dto(account_id, raw, override_, &effective))
    }

    /// Prepare a replacement transport for every live account under `settings`.
    fn prepare_account_clients(
        &self,
        settings: &ValidatedSettings,
        backend: &dyn AccountClientBackend,
    ) -> Result<Vec<PreparedAccountClient>, ProxySettingsError> {
        let mut prepared = Vec::new();
        for account_id in backend.live_accounts() {
            let effective = effective_for_account_document(settings, &account_id)?;
            let configuration = self.configuration_from_effective(&effective, true)?;
            let transport = backend
                .prepare(&account_id, &configuration)
                .map_err(ProxySettingsError::Invalid)?;
            prepared.push(PreparedAccountClient {
                account_id,
                configuration,
                transport,
            });
        }
        Ok(prepared)
    }

    fn install_account_clients(
        &self,
        prepared: Vec<PreparedAccountClient>,
        backend: &dyn AccountClientBackend,
    ) -> Result<(), ProxySettingsError> {
        for entry in prepared {
            backend
                .install(&entry.account_id, &entry.configuration, entry.transport)
                .map_err(ProxySettingsError::ConfigUnavailable)?;
        }
        Ok(())
    }

    /// Set (or replace) one account's proxy override transactionally. Credentials
    /// are written only to the OS store under an account-scoped reference; the
    /// document is committed atomically and every live account client is rebuilt
    /// under the caller's coordination gate. Any failure leaves the previous
    /// document and clients intact.
    pub fn set_account_override(
        &self,
        account_id: &AccountId,
        input: AccountProxyOverrideInput,
        backend: &dyn AccountClientBackend,
    ) -> Result<AccountProxySettingsDto, ProxySettingsError> {
        let _transaction = self.transaction_lock();

        let AccountProxyOverrideInput {
            mode,
            endpoint,
            no_proxy,
            username,
            password,
        } = input;
        let username = username.map(Zeroizing::new);
        let password = password.map(Zeroizing::new);

        let (persisted, old) = self.load_for_update()?;
        let old_ref = old
            .accounts
            .get(account_id)
            .and_then(|value| value.credential_ref.clone());

        let validated_endpoint = match endpoint.as_deref() {
            Some(value) => Some(validate_endpoint(value)?),
            None => None,
        };
        let validated_no_proxy = validate_no_proxy(no_proxy.as_deref())?;

        let supplied = match (username.as_ref(), password.as_ref()) {
            (None, None) => None,
            (Some(username), Some(password)) => Some((
                validate_credential_field(username, "username")?,
                validate_credential_field(password, "password")?,
            )),
            _ => return Err(invalid(
                "Both a proxy username and password are required when credentials are supplied.",
            )),
        };

        let effective_mode = mode.unwrap_or(old.global.mode);
        if effective_mode != ProxyMode::Custom {
            if supplied.is_some() {
                return Err(invalid(
                    "Proxy credentials are only supported with the custom proxy mode.",
                ));
            }
            if validated_endpoint.is_some() {
                return Err(invalid(
                    "A proxy endpoint is only supported with the custom proxy mode.",
                ));
            }
            if validated_no_proxy.is_some() {
                return Err(invalid(
                    "A no-proxy list is only supported with the custom proxy mode.",
                ));
            }
        }

        // Credentials survive only while the override's effective mode is Custom.
        let mut new_ref = if effective_mode == ProxyMode::Custom {
            old_ref.clone()
        } else {
            None
        };
        let mut created_ref: Option<String> = None;
        if let Some((username, password)) = supplied {
            let credentials = ProxyCredentials::new(username, password);
            let reference = generate_account_credential_ref(account_id);
            self.store
                .store(&reference, &credentials)
                .map_err(ProxySettingsError::Credential)?;
            created_ref = Some(reference.clone());
            new_ref = Some(reference);
        }

        let mut candidate = persisted.clone();
        candidate.version = PROXY_SETTINGS_VERSION;
        let extra = candidate
            .accounts
            .get(account_id)
            .map(|value| value.extra.clone())
            .unwrap_or_default();
        candidate.accounts.insert(
            account_id.clone(),
            AccountProxyOverride {
                mode,
                endpoint: validated_endpoint,
                no_proxy: validated_no_proxy,
                credential_ref: new_ref.clone(),
                extra,
            },
        );
        candidate.pending_cleanup = old.global.pending_cleanup.clone();
        if let Some(reference) = old_ref.as_deref() {
            if Some(reference) != new_ref.as_deref() {
                push_unique(&mut candidate.pending_cleanup, reference);
            }
        }

        let settings = match validated_settings(&candidate) {
            Ok(settings) => settings,
            Err(error) => {
                self.discard_created_credential(&created_ref);
                return Err(error);
            }
        };
        let prepared = match self.prepare_account_clients(&settings, backend) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.discard_created_credential(&created_ref);
                return Err(error);
            }
        };

        if let Err(error) = save_persisted_to(&self.settings_path, &candidate) {
            self.discard_created_credential(&created_ref);
            return Err(error);
        }
        if let Err(error) = self.install_account_clients(prepared, backend) {
            let _ = save_persisted_to(&self.settings_path, &persisted);
            self.discard_created_credential(&created_ref);
            return Err(error);
        }

        self.drain_pending_cleanup_best_effort();
        self.account_dto_locked(&candidate, &settings, account_id)
    }

    /// Remove one account's override (restoring global inheritance) and
    /// pending-delete any account credential it referenced. Transactional and
    /// rollback-safe like [`ProxyRuntime::set_account_override`].
    pub fn clear_account_override(
        &self,
        account_id: &AccountId,
        backend: &dyn AccountClientBackend,
    ) -> Result<AccountProxySettingsDto, ProxySettingsError> {
        let _transaction = self.transaction_lock();

        let (persisted, old) = self.load_for_update()?;
        let mut candidate = persisted.clone();
        candidate.version = PROXY_SETTINGS_VERSION;
        candidate.pending_cleanup = old.global.pending_cleanup.clone();
        if let Some(removed) = candidate.accounts.remove(account_id) {
            if let Some(reference) = removed.credential_ref.as_deref() {
                push_unique(&mut candidate.pending_cleanup, reference);
            }
        }

        let settings = validated_settings(&candidate)?;
        let prepared = self.prepare_account_clients(&settings, backend)?;

        save_persisted_to(&self.settings_path, &candidate)?;
        if let Err(error) = self.install_account_clients(prepared, backend) {
            let _ = save_persisted_to(&self.settings_path, &persisted);
            return Err(error);
        }

        self.drain_pending_cleanup_best_effort();
        self.account_dto_locked(&candidate, &settings, account_id)
    }
}

// ============================================================================
// Atomic persistence
// ============================================================================

/// Load persisted settings. A missing file yields the default (System) policy.
/// Every other failure (I/O, malformed JSON, unsupported version) fails closed.
pub fn load_persisted_from(path: &Path) -> Result<PersistedProxySettings, ProxySettingsError> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let document: PersistedProxySettings =
                serde_json::from_slice(&bytes).map_err(|_| {
                    ProxySettingsError::ConfigInvalid(
                        "The saved proxy settings are invalid. Open proxy settings to repair them."
                            .to_string(),
                    )
                })?;
            if document.version != PROXY_SETTINGS_VERSION {
                return Err(ProxySettingsError::ConfigInvalid(
                    "The saved proxy settings use an unsupported version. Open proxy settings to migrate them."
                        .to_string(),
                ));
            }
            Ok(document)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(PersistedProxySettings::default())
        }
        Err(error) => Err(ProxySettingsError::ConfigUnavailable(format!(
            "Proxy settings could not be read: {error}"
        ))),
    }
}

/// Atomic save using a unique same-directory temp file followed by rename, so
/// concurrent transactions can never share/clobber a fixed `.tmp` path.
pub fn save_persisted_to(
    path: &Path,
    settings: &PersistedProxySettings,
) -> Result<(), ProxySettingsError> {
    let parent = path.parent().ok_or_else(|| {
        ProxySettingsError::ConfigUnavailable(
            "Proxy settings path has no parent directory.".to_string(),
        )
    })?;
    std::fs::create_dir_all(parent).map_err(|error| {
        ProxySettingsError::ConfigUnavailable(format!(
            "Proxy settings directory could not be created: {error}"
        ))
    })?;
    let bytes = serde_json::to_vec_pretty(settings).map_err(|_| {
        ProxySettingsError::ConfigInvalid("Proxy settings could not be serialized.".to_string())
    })?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(PROXY_SETTINGS_FILE);
    let temporary = parent.join(format!(
        ".{file_name}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    if let Err(error) =
        std::fs::write(&temporary, bytes).and_then(|_| std::fs::rename(&temporary, path))
    {
        let _ = std::fs::remove_file(&temporary);
        return Err(ProxySettingsError::ConfigUnavailable(format!(
            "Proxy settings could not be saved: {error}"
        )));
    }
    Ok(())
}

// ============================================================================
// Tests (no real OS keychain)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemoryCredentialStore {
        entries: Mutex<HashMap<String, Zeroizing<String>>>,
        fail_writes: AtomicBool,
        fail_loads: AtomicBool,
        fail_deletes: AtomicBool,
    }

    impl CredentialStore for MemoryCredentialStore {
        fn store(
            &self,
            reference: &str,
            credentials: &ProxyCredentials,
        ) -> Result<(), CredentialError> {
            if self.fail_writes.load(Ordering::SeqCst) {
                return Err(CredentialError::Unavailable);
            }
            self.entries
                .lock()
                .unwrap()
                .insert(reference.to_string(), credentials.to_stored_json());
            Ok(())
        }

        fn load(&self, reference: &str) -> Result<Option<ProxyCredentials>, CredentialError> {
            if self.fail_loads.load(Ordering::SeqCst) {
                return Err(CredentialError::Locked);
            }
            match self.entries.lock().unwrap().get(reference) {
                Some(secret) => Ok(Some(ProxyCredentials::from_stored_json(secret.as_str())?)),
                None => Ok(None),
            }
        }

        fn delete(&self, reference: &str) -> Result<(), CredentialError> {
            if self.fail_deletes.load(Ordering::SeqCst) {
                return Err(CredentialError::Unavailable);
            }
            self.entries.lock().unwrap().remove(reference);
            Ok(())
        }
    }

    struct MockBackend {
        fail_prepare: AtomicBool,
        fail_install: AtomicBool,
        prepares: Mutex<Vec<ProxyMode>>,
        installs: Mutex<Vec<ProxyMode>>,
    }

    impl Default for MockBackend {
        fn default() -> Self {
            Self {
                fail_prepare: AtomicBool::new(false),
                fail_install: AtomicBool::new(false),
                prepares: Mutex::new(Vec::new()),
                installs: Mutex::new(Vec::new()),
            }
        }
    }

    impl AccountClientBackend for MockBackend {
        fn live_accounts(&self) -> Vec<AccountId> {
            // A single synthetic live account is enough for the global
            // set/clear tests: its effective policy is the global policy.
            vec![AccountId::parse("111111111111111111").unwrap()]
        }

        fn prepare(
            &self,
            _account_id: &AccountId,
            configuration: &ProxyConfiguration,
        ) -> Result<PreparedProxyTransport, String> {
            if self.fail_prepare.load(Ordering::SeqCst) {
                return Err("cannot build client".to_string());
            }
            self.prepares.lock().unwrap().push(configuration.mode());
            Ok(PreparedProxyTransport::new(()))
        }

        fn install(
            &self,
            _account_id: &AccountId,
            configuration: &ProxyConfiguration,
            _prepared: PreparedProxyTransport,
        ) -> Result<(), String> {
            if self.fail_install.load(Ordering::SeqCst) {
                return Err("install failed".to_string());
            }
            self.installs.lock().unwrap().push(configuration.mode());
            Ok(())
        }
    }

    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "dqh-proxy-{label}-{}.json",
            uuid::Uuid::new_v4().simple()
        ))
    }

    fn custom_input(
        endpoint: &str,
        username: Option<&str>,
        password: Option<&str>,
    ) -> ProxySettingsInput {
        ProxySettingsInput {
            mode: ProxyMode::Custom,
            endpoint: Some(endpoint.to_string()),
            no_proxy: Some("localhost,127.0.0.1".to_string()),
            username: username.map(str::to_string),
            password: password.map(str::to_string),
        }
    }

    #[test]
    fn accepts_http_and_https_endpoints_and_normalizes_trailing_slash() {
        assert_eq!(
            validate_endpoint(" http://127.0.0.1:8080/ ").unwrap(),
            "http://127.0.0.1:8080"
        );
        // `url` normalizes away the default port for the scheme.
        assert_eq!(
            validate_endpoint("https://proxy.example.com:443").unwrap(),
            "https://proxy.example.com"
        );
        assert_eq!(
            validate_endpoint("http://[::1]:1080").unwrap(),
            "http://[::1]:1080"
        );
    }

    #[test]
    fn rejects_unsupported_or_unsafe_endpoints() {
        for bad in [
            "",
            "ftp://proxy.example.com",
            "socks5://127.0.0.1:1080",
            "http://",
            "http://127.0.0.1:0",
            "http://user:pass@proxy.example.com:8080",
            "http://user@proxy.example.com:8080",
            "http://proxy.example.com:8080/?x=1",
            "http://proxy.example.com:8080/#frag",
            "http://proxy.example.com:8080/path",
            "http://proxy.example.com:8080/ has space",
        ] {
            assert!(
                validate_endpoint(bad).is_err(),
                "endpoint should be rejected: {bad:?}"
            );
        }
        let oversized = format!("http://proxy.example.com:8080/{}", "a".repeat(4096));
        assert!(validate_endpoint(&oversized).is_err());
    }

    #[test]
    fn no_proxy_is_bounded_and_entry_validated() {
        assert_eq!(validate_no_proxy(None).unwrap(), None);
        assert_eq!(validate_no_proxy(Some("   ")).unwrap(), None);
        assert_eq!(
            validate_no_proxy(Some(
                " localhost,127.0.0.1,.example.com,10.0.0.0/8,[::1]:8080,* "
            ))
            .unwrap(),
            Some("localhost,127.0.0.1,.example.com,10.0.0.0/8,[::1]:8080,*".to_string())
        );
        for bad in [
            "local host",
            "bad\nvalue",
            "host:",
            "host:0",
            "host:99999",
            "user@host",
            "host/path",
            "10.0.0.0/33",
            "::1/129",
            ",localhost",
            "localhost,",
        ] {
            assert!(
                validate_no_proxy(Some(bad)).is_err(),
                "no-proxy should be rejected: {bad:?}"
            );
        }
        assert!(validate_no_proxy(Some(&"a".repeat(4096))).is_err());
    }

    #[test]
    fn credential_references_are_dot_free_and_delimiter_safe() {
        let reference = generate_credential_ref();
        assert!(!reference.contains('.'));
        assert!(!reference.contains(':'));
        assert!(reference
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_eq!(normalize_credential_ref(&reference), Some(reference));

        for bad in ["", "a.b", "a:b", "a b", "a/b", "a\\b", &"x".repeat(200)] {
            assert!(
                normalize_credential_ref(bad).is_none(),
                "ref should be rejected: {bad:?}"
            );
        }
    }

    #[test]
    fn credentials_are_redacted_in_debug_and_input_is_redacted() {
        let credentials =
            ProxyCredentials::new("sensitive-user".to_string(), "secret-pass".to_string());
        let debug = format!("{credentials:?}");
        assert!(!debug.contains("sensitive-user"));
        assert!(!debug.contains("secret-pass"));

        let input = custom_input(
            "http://127.0.0.1:8080",
            Some("sensitive-user"),
            Some("secret-pass"),
        );
        let input_debug = format!("{input:?}");
        assert!(!input_debug.contains("sensitive-user"));
        assert!(!input_debug.contains("secret-pass"));
        // The unvalidated endpoint/no-proxy must not be printable either.
        assert!(!input_debug.contains("127.0.0.1:8080"));
        assert!(!input_debug.contains("localhost"));
        assert!(input_debug.contains("<redacted>"));
    }

    #[test]
    fn set_persists_only_a_reference_and_never_secrets() {
        let store = Arc::new(MemoryCredentialStore::default());
        let path = temp_path("custom");
        let runtime = ProxyRuntime::new(store.clone(), path.clone());
        let backend = MockBackend::default();

        let (dto, configuration) = runtime
            .set(
                custom_input(
                    "https://proxy.example.com:8443",
                    Some("sensitive-user"),
                    Some("secret-pass"),
                ),
                &backend,
            )
            .expect("valid custom settings persist");

        assert_eq!(dto.mode, ProxyMode::Custom);
        assert!(dto.has_credentials);
        assert_eq!(
            dto.endpoint.as_deref(),
            Some("https://proxy.example.com:8443")
        );
        assert_eq!(*backend.installs.lock().unwrap(), vec![ProxyMode::Custom]);

        let file = std::fs::read_to_string(&path).unwrap();
        assert!(file.contains("credentialRef"));
        assert!(!file.contains("secret-pass"));
        assert!(!file.contains("sensitive-user"));
        assert!(!file.contains("password"));
        assert_eq!(store.entries.lock().unwrap().len(), 1);

        match configuration {
            ProxyConfiguration::Custom(custom) => {
                let credentials = custom.credentials.expect("credentials resolved");
                assert_eq!(credentials.username(), "sensitive-user");
                assert_eq!(credentials.password(), "secret-pass");
            }
            other => panic!("expected custom configuration, got {other:?}"),
        }

        let reloaded = runtime.resolve_current().unwrap();
        assert!(reloaded.has_credentials());
        assert!(!format!("{reloaded:?}").contains("secret-pass"));
        let dto_again = runtime.read_dto().unwrap();
        assert!(dto_again.has_credentials);
        assert!(!serde_json::to_string(&dto_again)
            .unwrap()
            .contains("secret-pass"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn serialized_public_dto_is_camel_case_and_secret_free() {
        let dto = ProxySettingsDto {
            mode: ProxyMode::Custom,
            endpoint: Some("http://127.0.0.1:8080".to_string()),
            no_proxy: Some("localhost".to_string()),
            has_credentials: true,
        };
        let value = serde_json::to_value(&dto).unwrap();
        assert_eq!(value["mode"], "custom");
        assert_eq!(value["endpoint"], "http://127.0.0.1:8080");
        assert_eq!(value["noProxy"], "localhost");
        assert_eq!(value["hasCredentials"], true);
        assert!(value.get("username").is_none());
        assert!(value.get("password").is_none());
        assert!(value.get("credentialRef").is_none());
    }

    #[test]
    fn failed_keyring_write_never_persists_plaintext() {
        let store = Arc::new(MemoryCredentialStore::default());
        store.fail_writes.store(true, Ordering::SeqCst);
        let path = temp_path("fail");
        let runtime = ProxyRuntime::new(store, path.clone());

        let result = runtime.set(
            custom_input(
                "https://proxy.example.com:8443",
                Some("sensitive-user"),
                Some("secret-pass"),
            ),
            &MockBackend::default(),
        );
        assert!(matches!(
            result,
            Err(ProxySettingsError::Credential(CredentialError::Unavailable))
        ));

        let file = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(!file.contains("secret-pass"));
        assert!(!file.contains("sensitive-user"));
        assert!(!file.contains("credentialRef"));
        assert!(!path.exists());
        let _ = std::fs::remove_file(&path);
    }

    // P1-5: the replacement transport is built before persistence; a build
    // failure must change nothing and must not report success.
    #[test]
    fn prepare_failure_changes_nothing_and_rolls_back_new_credential() {
        let store = Arc::new(MemoryCredentialStore::default());
        let path = temp_path("prepare");
        let runtime = ProxyRuntime::new(store.clone(), path.clone());
        let backend = MockBackend::default();
        backend.fail_prepare.store(true, Ordering::SeqCst);

        let result = runtime.set(
            custom_input(
                "https://proxy.example.com:8443",
                Some("sensitive-user"),
                Some("secret-pass"),
            ),
            &backend,
        );
        assert!(matches!(result, Err(ProxySettingsError::Invalid(_))));
        assert!(store.entries.lock().unwrap().is_empty());
        assert!(backend.installs.lock().unwrap().is_empty());
        assert!(!path.exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn install_failure_restores_the_previous_document() {
        let store = Arc::new(MemoryCredentialStore::default());
        let path = temp_path("install");
        let runtime = ProxyRuntime::new(store.clone(), path.clone());

        runtime
            .set(
                custom_input("https://old.example.com:8443", None, None),
                &MockBackend::default(),
            )
            .unwrap();

        let failing = MockBackend::default();
        failing.fail_install.store(true, Ordering::SeqCst);
        let result = runtime.set(
            custom_input("https://new.example.com:8443", None, None),
            &failing,
        );
        assert!(matches!(
            result,
            Err(ProxySettingsError::ConfigUnavailable(_))
        ));

        let persisted = load_persisted_from(&path).unwrap();
        assert_eq!(
            persisted.global.endpoint.as_deref(),
            Some("https://old.example.com:8443")
        );
        let _ = std::fs::remove_file(&path);
    }

    // P1-4: a superseded credential whose deletion fails is retained durably and
    // retried later rather than being lost.
    #[test]
    fn superseded_credential_delete_failure_is_retained_and_retried() {
        let store = Arc::new(MemoryCredentialStore::default());
        let path = temp_path("pending");
        let runtime = ProxyRuntime::new(store.clone(), path.clone());
        let backend = MockBackend::default();

        runtime
            .set(
                custom_input("https://proxy.example.com:8443", Some("u1"), Some("p1")),
                &backend,
            )
            .unwrap();
        let first_ref = load_persisted_from(&path)
            .unwrap()
            .global
            .credential_ref
            .unwrap();

        store.fail_deletes.store(true, Ordering::SeqCst);
        runtime
            .set(
                custom_input("https://proxy.example.com:8443", Some("u2"), Some("p2")),
                &backend,
            )
            .unwrap();

        let persisted = load_persisted_from(&path).unwrap();
        assert_eq!(persisted.pending_cleanup, vec![first_ref.clone()]);
        // Both credentials still exist because the delete was blocked.
        assert_eq!(store.entries.lock().unwrap().len(), 2);

        store.fail_deletes.store(false, Ordering::SeqCst);
        runtime.refresh_from_disk().unwrap();
        let persisted = load_persisted_from(&path).unwrap();
        assert!(persisted.pending_cleanup.is_empty());
        assert!(!store.entries.lock().unwrap().contains_key(&first_ref));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn failed_created_credential_delete_is_recorded_for_retry() {
        let store = Arc::new(MemoryCredentialStore::default());
        store.fail_deletes.store(true, Ordering::SeqCst);
        let path = temp_path("rollback-pending");
        let runtime = ProxyRuntime::new(store.clone(), path.clone());
        let backend = MockBackend::default();
        backend.fail_prepare.store(true, Ordering::SeqCst);

        let result = runtime.set(
            custom_input("https://proxy.example.com:8443", Some("u"), Some("p")),
            &backend,
        );
        assert!(matches!(result, Err(ProxySettingsError::Invalid(_))));

        let persisted = load_persisted_from(&path).unwrap();
        assert_eq!(persisted.pending_cleanup.len(), 1);
        assert_eq!(store.entries.lock().unwrap().len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    // P1-1: only a missing file defaults to System; corrupt/unsupported state
    // fails closed.
    #[test]
    fn corrupt_or_unsupported_config_fails_closed_but_is_repairable() {
        let store = Arc::new(MemoryCredentialStore::default());
        let path = temp_path("corrupt");
        let runtime = ProxyRuntime::new(store, path.clone());

        std::fs::write(&path, b"{ not json").unwrap();
        assert!(matches!(
            runtime.resolve_current(),
            Err(ProxySettingsError::ConfigInvalid(_))
        ));
        assert!(matches!(
            runtime.read_dto(),
            Err(ProxySettingsError::ConfigInvalid(_))
        ));

        std::fs::write(
            &path,
            br#"{"version":2,"global":{"mode":"custom","endpoint":"http://127.0.0.1:8080"}}"#,
        )
        .unwrap();
        assert!(matches!(
            runtime.resolve_current(),
            Err(ProxySettingsError::ConfigInvalid(_))
        ));

        // A tampered endpoint with userinfo must never reach the DTO, and a
        // system-mode document carrying an endpoint is inconsistent.
        std::fs::write(
            &path,
            br#"{"version":1,"global":{"mode":"custom","endpoint":"http://u:p@127.0.0.1:8080"}}"#,
        )
        .unwrap();
        assert!(runtime.read_dto().is_err());
        std::fs::write(
            &path,
            br#"{"version":1,"global":{"mode":"system","endpoint":"http://127.0.0.1:8080"}}"#,
        )
        .unwrap();
        assert!(runtime.read_dto().is_err());

        // The settings UI can still repair a corrupt document.
        let backend = MockBackend::default();
        let (dto, _) = runtime
            .set(
                ProxySettingsInput {
                    mode: ProxyMode::System,
                    endpoint: None,
                    no_proxy: None,
                    username: None,
                    password: None,
                },
                &backend,
            )
            .unwrap();
        assert_eq!(dto.mode, ProxyMode::System);
        assert_eq!(runtime.read_dto().unwrap().mode, ProxyMode::System);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_config_defaults_to_system() {
        let store = Arc::new(MemoryCredentialStore::default());
        let path = temp_path("missing");
        let runtime = ProxyRuntime::new(store, path.clone());

        assert!(matches!(
            runtime.resolve_current().unwrap(),
            ProxyConfiguration::System
        ));
        let dto = runtime.read_dto().unwrap();
        assert_eq!(dto.mode, ProxyMode::System);
        assert!(!dto.has_credentials);
        assert!(!path.exists());
    }

    #[test]
    fn stranded_credential_reference_fails_closed() {
        let store = Arc::new(MemoryCredentialStore::default());
        let path = temp_path("stranded");
        let runtime = ProxyRuntime::new(store, path.clone());

        std::fs::write(
            &path,
            br#"{"version":1,"global":{"mode":"custom","endpoint":"http://127.0.0.1:8080","credentialRef":"c0123456789abcdef0123456789abcdef"}}"#,
        )
        .unwrap();

        // DTO is reference-derived, but resolution fails closed rather than
        // silently going unauthenticated.
        assert!(runtime.read_dto().unwrap().has_credentials);
        assert!(matches!(
            runtime.resolve_current(),
            Err(ProxySettingsError::ConfigInvalid(_))
        ));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn clear_credentials_is_rollback_safe_when_install_fails() {
        let store = Arc::new(MemoryCredentialStore::default());
        let path = temp_path("clear-rollback");
        let runtime = ProxyRuntime::new(store.clone(), path.clone());

        runtime
            .set(
                custom_input("https://proxy.example.com:8443", Some("u"), Some("p")),
                &MockBackend::default(),
            )
            .unwrap();
        let reference = load_persisted_from(&path)
            .unwrap()
            .global
            .credential_ref
            .unwrap();

        let failing = MockBackend::default();
        failing.fail_install.store(true, Ordering::SeqCst);
        assert!(runtime.clear_credentials(&failing).is_err());

        // The document still references the credential and the secret survives.
        let persisted = load_persisted_from(&path).unwrap();
        assert_eq!(
            persisted.global.credential_ref.as_deref(),
            Some(reference.as_str())
        );
        assert_eq!(store.entries.lock().unwrap().len(), 1);

        // A healthy clear succeeds and removes the secret.
        let (dto, _) = runtime.clear_credentials(&MockBackend::default()).unwrap();
        assert!(!dto.has_credentials);
        assert!(store.entries.lock().unwrap().is_empty());
        assert!(load_persisted_from(&path)
            .unwrap()
            .global
            .credential_ref
            .is_none());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn switching_mode_drops_credentials_and_credential_errors_surface() {
        let store = Arc::new(MemoryCredentialStore::default());
        let path = temp_path("mode");
        let runtime = ProxyRuntime::new(store.clone(), path.clone());
        let backend = MockBackend::default();

        runtime
            .set(
                custom_input("https://proxy.example.com:8443", Some("user"), Some("pass")),
                &backend,
            )
            .unwrap();

        let (dto, configuration) = runtime
            .set(
                ProxySettingsInput {
                    mode: ProxyMode::Direct,
                    endpoint: None,
                    no_proxy: None,
                    username: None,
                    password: None,
                },
                &backend,
            )
            .unwrap();
        assert_eq!(dto.mode, ProxyMode::Direct);
        assert!(!dto.has_credentials);
        assert!(matches!(configuration, ProxyConfiguration::Direct));
        assert!(store.entries.lock().unwrap().is_empty());

        runtime
            .set(
                custom_input("https://proxy.example.com:8443", Some("user"), Some("pass")),
                &backend,
            )
            .unwrap();
        store.fail_loads.store(true, Ordering::SeqCst);
        let error = runtime.resolve_current().unwrap_err();
        assert!(matches!(
            error,
            ProxySettingsError::Credential(CredentialError::Locked)
        ));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn supplies_credentials_only_with_custom_mode_and_both_fields() {
        let store = Arc::new(MemoryCredentialStore::default());
        let path = temp_path("validation");
        let runtime = ProxyRuntime::new(store, path.clone());
        let backend = MockBackend::default();

        let missing_password = ProxySettingsInput {
            mode: ProxyMode::Custom,
            endpoint: Some("http://127.0.0.1:8080".to_string()),
            no_proxy: None,
            username: Some("user".to_string()),
            password: None,
        };
        assert!(matches!(
            runtime.set(missing_password, &backend),
            Err(ProxySettingsError::Invalid(_))
        ));

        let creds_in_system = ProxySettingsInput {
            mode: ProxyMode::System,
            endpoint: None,
            no_proxy: None,
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
        };
        assert!(matches!(
            runtime.set(creds_in_system, &backend),
            Err(ProxySettingsError::Invalid(_))
        ));

        let _ = std::fs::remove_file(&path);
    }

    // P1-2: a fixed `.tmp` sibling must never be used, so concurrent writers
    // cannot clobber each other's staging file.
    #[test]
    fn save_uses_a_unique_temp_and_leaves_a_legacy_tmp_untouched() {
        let path = temp_path("unique-tmp");
        let legacy_tmp = path.with_extension("json.tmp");
        std::fs::write(&legacy_tmp, b"sentinel").unwrap();

        let mut document = PersistedProxySettings::default();
        document.global.mode = ProxyMode::Direct;
        save_persisted_to(&path, &document).unwrap();

        assert_eq!(std::fs::read(&legacy_tmp).unwrap(), b"sentinel");
        assert_eq!(
            load_persisted_from(&path).unwrap().global.mode,
            ProxyMode::Direct
        );

        let _ = std::fs::remove_file(&legacy_tmp);
        let _ = std::fs::remove_file(&path);
    }

    // P1-2: concurrent transactions must serialize and always leave a valid file.
    #[test]
    fn concurrent_transactions_serialize_and_keep_a_valid_document() {
        let store = Arc::new(MemoryCredentialStore::default());
        let path = temp_path("concurrent");
        let runtime = Arc::new(ProxyRuntime::new(store, path.clone()));

        std::thread::scope(|scope| {
            for index in 0..8 {
                let runtime = Arc::clone(&runtime);
                scope.spawn(move || {
                    let endpoint = format!("http://127.0.0.1:{}", 9000 + index);
                    let backend = MockBackend::default();
                    runtime
                        .set(
                            ProxySettingsInput {
                                mode: ProxyMode::Custom,
                                endpoint: Some(endpoint),
                                no_proxy: None,
                                username: None,
                                password: None,
                            },
                            &backend,
                        )
                        .unwrap();
                });
            }
        });

        let persisted = load_persisted_from(&path).unwrap();
        assert_eq!(persisted.global.mode, ProxyMode::Custom);
        assert!(runtime.read_dto().is_ok());
        let _ = std::fs::remove_file(&path);
    }

    // ---- Phase 6.3B: account proxy overrides --------------------------------

    #[derive(Default)]
    struct MockAccountBackend {
        lives: Mutex<Vec<AccountId>>,
        fail_prepare_for: Mutex<Option<AccountId>>,
        fail_install: AtomicBool,
        prepares: Mutex<Vec<(AccountId, ProxyMode)>>,
        installs: Mutex<Vec<(AccountId, ProxyMode)>>,
    }

    impl MockAccountBackend {
        fn with_lives(accounts: &[&AccountId]) -> Self {
            Self {
                lives: Mutex::new(accounts.iter().map(|id| (*id).clone()).collect()),
                ..Default::default()
            }
        }
    }

    impl AccountClientBackend for MockAccountBackend {
        fn live_accounts(&self) -> Vec<AccountId> {
            self.lives.lock().unwrap().clone()
        }

        fn prepare(
            &self,
            account_id: &AccountId,
            configuration: &ProxyConfiguration,
        ) -> Result<PreparedProxyTransport, String> {
            if self.fail_prepare_for.lock().unwrap().as_ref() == Some(account_id) {
                return Err("cannot build account client".to_string());
            }
            self.prepares
                .lock()
                .unwrap()
                .push((account_id.clone(), configuration.mode()));
            Ok(PreparedProxyTransport::new(()))
        }

        fn install(
            &self,
            account_id: &AccountId,
            configuration: &ProxyConfiguration,
            _prepared: PreparedProxyTransport,
        ) -> Result<(), String> {
            if self.fail_install.load(Ordering::SeqCst) {
                return Err("install failed".to_string());
            }
            self.installs
                .lock()
                .unwrap()
                .push((account_id.clone(), configuration.mode()));
            Ok(())
        }
    }

    fn account(id: &str) -> AccountId {
        AccountId::parse(id).expect("valid account id")
    }

    fn global_custom(endpoint: &str) -> ProxySettingsInput {
        ProxySettingsInput {
            mode: ProxyMode::Custom,
            endpoint: Some(endpoint.to_string()),
            no_proxy: None,
            username: None,
            password: None,
        }
    }

    fn account_override(
        mode: Option<ProxyMode>,
        endpoint: Option<&str>,
        no_proxy: Option<&str>,
    ) -> AccountProxyOverrideInput {
        AccountProxyOverrideInput {
            mode,
            endpoint: endpoint.map(str::to_string),
            no_proxy: no_proxy.map(str::to_string),
            username: None,
            password: None,
        }
    }

    fn isolated() -> (Arc<MemoryCredentialStore>, PathBuf, ProxyRuntime) {
        let store = Arc::new(MemoryCredentialStore::default());
        let path = temp_path("account");
        let runtime = ProxyRuntime::new(store.clone(), path.clone());
        (store, path, runtime)
    }

    // Isolation: A's override never leaks to B, which stays on the global policy.
    #[test]
    fn account_override_isolates_a_from_b_on_global() {
        let (_store, path, runtime) = isolated();
        let backend = MockAccountBackend::default();
        runtime
            .set(
                global_custom("http://127.0.0.1:8000"),
                &MockBackend::default(),
            )
            .unwrap();
        let a = account("111111111111111111");
        let b = account("222222222222222222");

        runtime
            .set_account_override(
                &a,
                account_override(Some(ProxyMode::Custom), Some("http://127.0.0.1:9001"), None),
                &backend,
            )
            .unwrap();

        let a_dto = runtime.read_account_dto(&a).unwrap();
        assert!(a_dto.has_override);
        assert_eq!(
            a_dto.effective_endpoint.as_deref(),
            Some("http://127.0.0.1:9001")
        );
        let b_dto = runtime.read_account_dto(&b).unwrap();
        assert!(!b_dto.has_override);
        assert_eq!(
            b_dto.effective_endpoint.as_deref(),
            Some("http://127.0.0.1:8000")
        );

        match runtime.resolve_for_account(&b).unwrap() {
            ProxyConfiguration::Custom(custom) => {
                assert_eq!(custom.endpoint, "http://127.0.0.1:8000")
            }
            other => panic!("expected custom, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    // A partial override (no-proxy only) inherits the global endpoint + mode.
    #[test]
    fn partial_override_inherits_global_mode_and_endpoint() {
        let (_store, path, runtime) = isolated();
        let backend = MockAccountBackend::default();
        let mut global = global_custom("http://127.0.0.1:8000");
        global.no_proxy = Some("global.example".to_string());
        runtime.set(global, &MockBackend::default()).unwrap();
        let a = account("111111111111111111");

        runtime
            .set_account_override(
                &a,
                account_override(None, None, Some("only.example")),
                &backend,
            )
            .unwrap();

        let dto = runtime.read_account_dto(&a).unwrap();
        assert!(dto.has_override);
        assert_eq!(dto.effective_mode, ProxyMode::Custom);
        assert_eq!(
            dto.effective_endpoint.as_deref(),
            Some("http://127.0.0.1:8000")
        );
        assert_eq!(dto.effective_no_proxy.as_deref(), Some("only.example"));
        let _ = std::fs::remove_file(&path);
    }

    // A malformed accounts slot fails closed and can be repaired by a mutation.
    #[test]
    fn malformed_accounts_slot_fails_closed_and_is_repairable() {
        let (_store, path, runtime) = isolated();
        let account_a = account("111111111111111111");
        // `accounts` is an array rather than the typed map: fail closed.
        std::fs::write(
            &path,
            br#"{"version":1,"global":{"mode":"custom","endpoint":"http://127.0.0.1:8000"},"accounts":[]}"#,
        )
        .unwrap();
        assert!(matches!(
            runtime.read_account_dto(&account_a),
            Err(ProxySettingsError::ConfigInvalid(_))
        ));

        // A mutation repairs it (replaces the invalid document) and succeeds.
        let backend = MockAccountBackend::default();
        runtime
            .set_account_override(
                &account_a,
                account_override(Some(ProxyMode::Custom), Some("http://127.0.0.1:9001"), None),
                &backend,
            )
            .unwrap();
        let dto = runtime.read_account_dto(&account_a).unwrap();
        assert_eq!(
            dto.effective_endpoint.as_deref(),
            Some("http://127.0.0.1:9001")
        );
        let _ = std::fs::remove_file(&path);
    }

    // Set then clear restores global inheritance.
    #[test]
    fn set_then_clear_round_trip_restores_global() {
        let (_store, path, runtime) = isolated();
        let backend = MockAccountBackend::default();
        runtime
            .set(
                global_custom("http://127.0.0.1:8000"),
                &MockBackend::default(),
            )
            .unwrap();
        let a = account("111111111111111111");
        runtime
            .set_account_override(
                &a,
                account_override(Some(ProxyMode::Custom), Some("http://127.0.0.1:9001"), None),
                &backend,
            )
            .unwrap();
        runtime.clear_account_override(&a, &backend).unwrap();

        let dto = runtime.read_account_dto(&a).unwrap();
        assert!(!dto.has_override);
        assert_eq!(
            dto.effective_endpoint.as_deref(),
            Some("http://127.0.0.1:8000")
        );
        let _ = std::fs::remove_file(&path);
    }

    // Account credentials are account-scoped references and never persisted in
    // plaintext or exposed in any DTO.
    #[test]
    fn account_credentials_are_scoped_and_absent_from_dtos() {
        let (store, path, runtime) = isolated();
        let backend = MockAccountBackend::default();
        runtime
            .set(
                global_custom("http://127.0.0.1:8000"),
                &MockBackend::default(),
            )
            .unwrap();
        let a = account("111111111111111111");
        let mut input =
            account_override(Some(ProxyMode::Custom), Some("http://127.0.0.1:9001"), None);
        input.username = Some("alice-user".to_string());
        input.password = Some("alice-secret".to_string());
        let dto = runtime.set_account_override(&a, input, &backend).unwrap();
        assert!(dto.override_has_credentials);
        assert!(dto.effective_has_credentials);

        let bytes = std::fs::read_to_string(&path).unwrap();
        assert!(!bytes.contains("alice-secret"));
        assert!(!bytes.contains("alice-user"));
        let persisted = load_persisted_from(&path).unwrap();
        let reference = persisted
            .accounts
            .get(&a)
            .unwrap()
            .credential_ref
            .clone()
            .unwrap();
        assert!(reference.starts_with("a111111111111111111-"));
        assert!(store.entries.lock().unwrap().contains_key(&reference));

        let json = serde_json::to_value(&dto).unwrap();
        assert!(json.get("overrideMode").is_some());
        assert!(json.get("effectiveHasCredentials").is_some());
        assert!(json.get("credentialRef").is_none() && json.get("credential_ref").is_none());
        for forbidden in ["password", "username", "secret"] {
            assert!(!serde_json::to_string(&dto).unwrap().contains(forbidden));
        }
        let _ = std::fs::remove_file(&path);
    }

    // A keyring write failure writes no plaintext and leaves the previous policy.
    #[test]
    fn account_keyring_write_failure_rolls_back() {
        let (store, path, runtime) = isolated();
        let backend = MockAccountBackend::default();
        runtime
            .set(
                global_custom("http://127.0.0.1:8000"),
                &MockBackend::default(),
            )
            .unwrap();
        let a = account("111111111111111111");
        store.fail_writes.store(true, Ordering::SeqCst);
        let mut input =
            account_override(Some(ProxyMode::Custom), Some("http://127.0.0.1:9001"), None);
        input.username = Some("u".to_string());
        input.password = Some("p".to_string());
        assert!(matches!(
            runtime.set_account_override(&a, input, &backend),
            Err(ProxySettingsError::Credential(_))
        ));
        assert!(!runtime.read_account_dto(&a).unwrap().has_override);
        let bytes = std::fs::read_to_string(&path).unwrap();
        assert!(!bytes.contains("\"u\"") && !bytes.contains("\"p\""));
        let _ = std::fs::remove_file(&path);
    }

    // A rebuild failure rolls the document back; every account keeps a
    // consistent previous policy.
    #[test]
    fn account_rebuild_failure_rolls_back_document() {
        let (_store, path, runtime) = isolated();
        runtime
            .set(
                global_custom("http://127.0.0.1:8000"),
                &MockBackend::default(),
            )
            .unwrap();
        let a = account("111111111111111111");
        let b = account("222222222222222222");
        let backend = MockAccountBackend::with_lives(&[&a, &b]);
        *backend.fail_prepare_for.lock().unwrap() = Some(b.clone());

        assert!(runtime
            .set_account_override(
                &a,
                account_override(Some(ProxyMode::Custom), Some("http://127.0.0.1:9001"), None),
                &backend,
            )
            .is_err());
        // Neither account changed.
        assert!(!runtime.read_account_dto(&a).unwrap().has_override);
        let _ = std::fs::remove_file(&path);
    }

    // A batch mutation prepares every live account (not just the active one).
    #[test]
    fn account_batch_rebuild_covers_all_live_accounts() {
        let (_store, path, runtime) = isolated();
        runtime
            .set(
                global_custom("http://127.0.0.1:8000"),
                &MockBackend::default(),
            )
            .unwrap();
        let a = account("111111111111111111");
        let b = account("222222222222222222");
        let backend = MockAccountBackend::with_lives(&[&a, &b]);
        runtime
            .set_account_override(
                &a,
                account_override(Some(ProxyMode::Custom), Some("http://127.0.0.1:9001"), None),
                &backend,
            )
            .unwrap();
        let mut prepared = backend.prepares.lock().unwrap().clone();
        prepared.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(prepared.len(), 2);
        assert_eq!(prepared[0].0, a);
        assert_eq!(prepared[1].0, b);
        let _ = std::fs::remove_file(&path);
    }

    // A superseded account credential whose deletion fails is retained for retry.
    #[test]
    fn superseded_account_credential_deletion_is_retained_for_retry() {
        let (store, path, runtime) = isolated();
        let backend = MockAccountBackend::default();
        runtime
            .set(
                global_custom("http://127.0.0.1:8000"),
                &MockBackend::default(),
            )
            .unwrap();
        let a = account("111111111111111111");
        let mut first =
            account_override(Some(ProxyMode::Custom), Some("http://127.0.0.1:9001"), None);
        first.username = Some("u1".to_string());
        first.password = Some("p1".to_string());
        runtime.set_account_override(&a, first, &backend).unwrap();
        let old_ref = load_persisted_from(&path)
            .unwrap()
            .accounts
            .get(&a)
            .unwrap()
            .credential_ref
            .clone()
            .unwrap();

        store.fail_deletes.store(true, Ordering::SeqCst);
        let mut second =
            account_override(Some(ProxyMode::Custom), Some("http://127.0.0.1:9001"), None);
        second.username = Some("u2".to_string());
        second.password = Some("p2".to_string());
        runtime.set_account_override(&a, second, &backend).unwrap();

        let persisted = load_persisted_from(&path).unwrap();
        assert!(persisted.pending_cleanup.contains(&old_ref));
        assert!(store.entries.lock().unwrap().contains_key(&old_ref));
        let _ = std::fs::remove_file(&path);
    }

    // A stranded account credential reference fails closed (never falls back to
    // another account's policy).
    #[test]
    fn stranded_account_credential_reference_fails_closed() {
        let (store, path, runtime) = isolated();
        let backend = MockAccountBackend::default();
        runtime
            .set(
                global_custom("http://127.0.0.1:8000"),
                &MockBackend::default(),
            )
            .unwrap();
        let a = account("111111111111111111");
        let mut input =
            account_override(Some(ProxyMode::Custom), Some("http://127.0.0.1:9001"), None);
        input.username = Some("u".to_string());
        input.password = Some("p".to_string());
        runtime.set_account_override(&a, input, &backend).unwrap();

        // Simulate the keyring entry disappearing.
        let reference = load_persisted_from(&path)
            .unwrap()
            .accounts
            .get(&a)
            .unwrap()
            .credential_ref
            .clone()
            .unwrap();
        store.entries.lock().unwrap().remove(&reference);

        assert!(matches!(
            runtime.resolve_for_account(&a),
            Err(ProxySettingsError::ConfigInvalid(_))
        ));
        let _ = std::fs::remove_file(&path);
    }

    // A global update batch-rebuilds EVERY live account with its own effective
    // candidate policy: an overridden account keeps its override, an inheriting
    // account reflects the new global.
    #[test]
    fn global_update_batch_rebuilds_override_and_inheriting_accounts() {
        let (_store, path, runtime) = isolated();
        let a = account("111111111111111111");
        let b = account("222222222222222222");
        let backend = MockAccountBackend::with_lives(&[&a, &b]);

        runtime
            .set(global_custom("http://127.0.0.1:8000"), &backend)
            .unwrap();
        runtime
            .set_account_override(
                &a,
                account_override(Some(ProxyMode::Custom), Some("http://127.0.0.1:9001"), None),
                &backend,
            )
            .unwrap();

        backend.prepares.lock().unwrap().clear();
        backend.installs.lock().unwrap().clear();

        // Global changes: both live accounts must be rebuilt.
        runtime
            .set(global_custom("http://127.0.0.1:8002"), &backend)
            .unwrap();

        let prepared = backend.prepares.lock().unwrap().clone();
        assert_eq!(prepared.len(), 2);
        assert!(prepared.iter().any(|(id, _)| id == &a));
        assert!(prepared.iter().any(|(id, _)| id == &b));
        assert_eq!(backend.installs.lock().unwrap().len(), 2);

        // Each account resolves to its own effective policy.
        match runtime.resolve_for_account(&a).unwrap() {
            ProxyConfiguration::Custom(custom) => {
                assert_eq!(custom.endpoint, "http://127.0.0.1:9001")
            }
            other => panic!("alice override lost, got {other:?}"),
        }
        match runtime.resolve_for_account(&b).unwrap() {
            ProxyConfiguration::Custom(custom) => {
                assert_eq!(custom.endpoint, "http://127.0.0.1:8002")
            }
            other => panic!("bob did not inherit the new global, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    // A syntax-valid credential reference that does not carry this account's
    // `a{accountId}-` prefix is a forged/cross-account reference and fails closed.
    #[test]
    fn forged_cross_account_credential_reference_fails_closed() {
        let (_store, path, runtime) = isolated();
        let b = account("222222222222222222");
        std::fs::write(
            &path,
            br#"{"version":1,"global":{"mode":"custom","endpoint":"http://127.0.0.1:8000"},"accounts":{"222222222222222222":{"mode":"custom","endpoint":"http://127.0.0.1:9001","credentialRef":"a111111111111111111-cdeadbeef"}}}"#,
        )
        .unwrap();

        assert!(matches!(
            runtime.read_account_dto(&b),
            Err(ProxySettingsError::ConfigInvalid(_))
        ));
        assert!(matches!(
            runtime.resolve_for_account(&b),
            Err(ProxySettingsError::ConfigInvalid(_))
        ));
        let _ = std::fs::remove_file(&path);
    }
}
