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

use crate::models::{ProxyMode, ProxySettingsDto, ProxySettingsInput};
use serde::{Deserialize, Serialize};
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
const MAX_CREDENTIAL_REF_LEN: usize = 64;
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

/// Versioned persisted document. `accounts` is reserved for Phase 6 per-account
/// overrides and is round-tripped untouched as an opaque value.
/// `pending_cleanup` holds opaque references whose deletion failed and that must
/// be retried; it never contains a secret.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PersistedProxySettings {
    #[serde(default = "default_settings_version")]
    pub version: u32,
    #[serde(default)]
    pub global: PersistedGlobalProxy,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_cleanup: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accounts: Option<serde_json::Value>,
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
            accounts: None,
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

/// Backend hook that builds and installs the real transport for a configuration.
///
/// `prepare` must not mutate any live state and is called *before* the
/// configuration is persisted. `install` runs after persistence and must be as
/// close to infallible as possible; if it returns an error the transaction rolls
/// the persisted document back.
pub trait ProxyTransportBackend: Send + Sync {
    fn prepare(&self, configuration: &ProxyConfiguration)
        -> Result<PreparedProxyTransport, String>;
    fn install(
        &self,
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
        backend: &dyn ProxyTransportBackend,
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
        let old_ref = old.credential_ref.clone();

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
        candidate.pending_cleanup = old.pending_cleanup.clone();
        if let Some(old) = old_ref.as_deref() {
            if Some(old) != new_ref.as_deref() {
                push_unique(&mut candidate.pending_cleanup, old);
            }
        }

        let validated = match validated_global(&candidate) {
            Ok(validated) => validated,
            Err(error) => {
                self.discard_created_credential(&created_ref);
                return Err(error);
            }
        };
        let configuration = match self.configuration_from_validated(&validated, true) {
            Ok(configuration) => configuration,
            Err(error) => {
                self.discard_created_credential(&created_ref);
                return Err(error);
            }
        };

        // Build (and therefore validate) the replacement transport before
        // touching the persisted document.
        let prepared = match backend.prepare(&configuration) {
            Ok(prepared) => prepared,
            Err(message) => {
                self.discard_created_credential(&created_ref);
                return Err(ProxySettingsError::Invalid(message));
            }
        };

        if let Err(error) = save_persisted_to(&self.settings_path, &candidate) {
            self.discard_created_credential(&created_ref);
            return Err(error);
        }

        if let Err(message) = backend.install(&configuration, prepared) {
            // The swap could not be applied: restore the previous document so we
            // never report success while traffic still uses the old policy.
            let _ = save_persisted_to(&self.settings_path, &persisted);
            self.discard_created_credential(&created_ref);
            return Err(ProxySettingsError::ConfigUnavailable(message));
        }

        self.drain_pending_cleanup_best_effort();

        Ok((
            dto_from_validated(&validated, validated.credential_ref.is_some()),
            configuration,
        ))
    }

    /// Remove the saved credential, rollback-safe. The credential-free document
    /// is validated and committed first; the secret is only deleted afterwards,
    /// and a deletion failure is retained as a pending-cleanup reference.
    pub fn clear_credentials(
        &self,
        backend: &dyn ProxyTransportBackend,
    ) -> Result<(ProxySettingsDto, ProxyConfiguration), ProxySettingsError> {
        let _transaction = self.transaction_lock();

        let (persisted, old) = self.load_for_update()?;
        let mut candidate = persisted.clone();
        candidate.version = PROXY_SETTINGS_VERSION;
        candidate.global.credential_ref = None;
        candidate.pending_cleanup = old.pending_cleanup.clone();
        if let Some(reference) = old.credential_ref.as_deref() {
            push_unique(&mut candidate.pending_cleanup, reference);
        }

        let validated = validated_global(&candidate)?;
        let configuration = self.configuration_from_validated(&validated, true)?;
        let prepared = backend
            .prepare(&configuration)
            .map_err(ProxySettingsError::Invalid)?;

        // Commit the credential-free document before deleting the secret so a
        // later failure cannot strand the config pointing at a missing entry.
        save_persisted_to(&self.settings_path, &candidate)?;

        if let Err(message) = backend.install(&configuration, prepared) {
            let _ = save_persisted_to(&self.settings_path, &persisted);
            return Err(ProxySettingsError::ConfigUnavailable(message));
        }

        self.drain_pending_cleanup_best_effort();

        Ok((dto_from_validated(&validated, false), configuration))
    }

    /// Load the on-disk document for a mutating transaction, falling back to the
    /// default only when the document is corrupt/inconsistent (so the settings UI
    /// can repair it). I/O failures still propagate.
    fn load_for_update(
        &self,
    ) -> Result<(PersistedProxySettings, ValidatedGlobal), ProxySettingsError> {
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

        match validated_global(&document) {
            Ok(validated) => Ok((document, validated)),
            Err(_) => {
                let document = PersistedProxySettings::default();
                let validated = validated_global(&document)
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
        match validated.mode {
            ProxyMode::System => Ok(ProxyConfiguration::System),
            ProxyMode::Direct => Ok(ProxyConfiguration::Direct),
            ProxyMode::Custom => {
                let endpoint = validated.endpoint.clone().ok_or_else(|| {
                    ProxySettingsError::ConfigInvalid(
                        "The saved custom proxy is missing its endpoint.".to_string(),
                    )
                })?;
                let endpoint = validate_endpoint(&endpoint)?;
                let credentials = if load_credentials {
                    match validated.credential_ref.as_deref() {
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
                    no_proxy: validated.no_proxy.clone(),
                    credentials,
                }))
            }
        }
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

    impl ProxyTransportBackend for MockBackend {
        fn prepare(
            &self,
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
}
