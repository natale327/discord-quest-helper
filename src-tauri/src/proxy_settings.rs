//! Global HTTP(S) proxy settings (Phase 5A).
//!
//! The persisted file (`proxy.v1.json`) contains only non-secret data: mode,
//! endpoint, no-proxy list, and an opaque credential reference. Optional proxy
//! username/password live exclusively in the OS credential store (Windows
//! Credential Manager, macOS Keychain, Linux Secret Service). Secrets are never
//! serialized, logged, returned through a DTO, or held in a `Debug`/`Display`.
//!
//! All keyring calls are blocking; callers must invoke the `ProxyRuntime`
//! methods on a blocking thread (the Tauri command layer wraps them with
//! `spawn_blocking`), which matters on Linux where the Secret Service backend is
//! synchronous.

use crate::models::{ProxyMode, ProxySettingsDto, ProxySettingsInput};
use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
    /// No saved credential exists for the reference. Treated as "no credentials".
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
/// a host, no embedded userinfo, and no query/fragment/path are accepted.
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

/// Validate an optional no-proxy list. Empty becomes `None`. Bounded and free of
/// control characters/whitespace so it cannot smuggle header content.
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
    Ok(Some(trimmed.to_string()))
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PersistedProxySettings {
    #[serde(default = "default_settings_version")]
    pub version: u32,
    #[serde(default)]
    pub global: PersistedGlobalProxy,
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
            accounts: None,
        }
    }
}

fn dto_from(persisted: &PersistedProxySettings, has_credentials: bool) -> ProxySettingsDto {
    ProxySettingsDto {
        mode: persisted.global.mode,
        endpoint: persisted.global.endpoint.clone(),
        no_proxy: persisted.global.no_proxy.clone(),
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

/// Validator callback used to reject a configuration that cannot build an HTTP
/// client, before it is persisted. Kept as a trait object so this module does
/// not depend on `discord_api`.
pub type ProxyConfigValidator = dyn Fn(&ProxyConfiguration) -> Result<(), String>;

// ============================================================================
// Runtime
// ============================================================================

/// Holds the effective proxy policy and performs the (blocking) keyring work.
pub struct ProxyRuntime {
    store: Arc<dyn CredentialStore>,
    settings_path: PathBuf,
    configuration: ArcSwap<ProxyConfiguration>,
}

impl ProxyRuntime {
    pub fn new(store: Arc<dyn CredentialStore>, settings_path: PathBuf) -> Self {
        Self {
            store,
            settings_path,
            configuration: ArcSwap::from_pointee(ProxyConfiguration::System),
        }
    }

    /// Public, secret-free snapshot. `hasCredentials` reflects whether a
    /// credential reference is saved (it is never resolved or echoed).
    pub fn read_dto(&self) -> Result<ProxySettingsDto, ProxySettingsError> {
        let persisted = load_persisted_from(&self.settings_path)?;
        let has_credentials = persisted.global.credential_ref.is_some();
        Ok(dto_from(&persisted, has_credentials))
    }

    /// Load the saved policy and its credential from the OS store, cache it, and
    /// return the public snapshot. Used at startup and on demand.
    pub fn refresh_from_disk(&self) -> Result<ProxySettingsDto, ProxySettingsError> {
        let persisted = load_persisted_from(&self.settings_path)?;
        let configuration = self.configuration_for(&persisted)?;
        let has_credentials = configuration.has_credentials();
        self.configuration.store(Arc::new(configuration));
        Ok(dto_from(&persisted, has_credentials))
    }

    /// Resolve the currently persisted policy into an effective configuration.
    pub fn resolve_current(&self) -> Result<ProxyConfiguration, ProxySettingsError> {
        let persisted = load_persisted_from(&self.settings_path)?;
        self.configuration_for(&persisted)
    }

    /// Resolve for login/startup. Structural problems fall back to `System`
    /// (never plaintext); credential-store failures still surface so the user can
    /// unlock the keychain.
    pub fn resolve_current_for_login(&self) -> Result<ProxyConfiguration, ProxySettingsError> {
        match self.resolve_current() {
            Ok(configuration) => Ok(configuration),
            Err(ProxySettingsError::Credential(error)) => {
                Err(ProxySettingsError::Credential(error))
            }
            Err(error) => {
                use crate::logger::{log, LogCategory, LogLevel};
                log(
                    LogLevel::Warn,
                    LogCategory::Api,
                    "Saved proxy settings could not be applied; using system detection",
                    Some(&error.to_string()),
                );
                Ok(ProxyConfiguration::System)
            }
        }
    }

    /// Apply a new proxy configuration transactionally:
    /// 1. validate input,
    /// 2. write credentials to the OS store (abort on failure),
    /// 3. build-validate the resulting policy,
    /// 4. atomically persist the secret-free file,
    /// 5. delete any superseded credential entry.
    pub fn set(
        &self,
        input: ProxySettingsInput,
        validator: &ProxyConfigValidator,
    ) -> Result<(ProxySettingsDto, ProxyConfiguration), ProxySettingsError> {
        let ProxySettingsInput {
            mode,
            endpoint,
            no_proxy,
            username,
            password,
        } = input;

        let persisted = load_persisted_from(&self.settings_path)?;
        let old_ref = persisted.global.credential_ref.clone();

        let (validated_endpoint, validated_no_proxy) = match mode {
            ProxyMode::Custom => (
                Some(validate_endpoint(endpoint.as_deref().unwrap_or_default())?),
                validate_no_proxy(no_proxy.as_deref())?,
            ),
            _ => (None, None),
        };

        let supplied = match (username, password) {
            (None, None) => None,
            (Some(username), Some(password)) => Some((
                validate_credential_field(&username, "username")?,
                validate_credential_field(&password, "password")?,
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

        let configuration = match self.configuration_for(&candidate) {
            Ok(configuration) => configuration,
            Err(error) => {
                self.rollback_created(&created_ref);
                return Err(error);
            }
        };
        if let Err(message) = validator(&configuration) {
            self.rollback_created(&created_ref);
            return Err(ProxySettingsError::Invalid(message));
        }

        if let Err(error) = save_persisted_to(&self.settings_path, &candidate) {
            self.rollback_created(&created_ref);
            return Err(error);
        }

        // Persisted successfully; the superseded entry can now be removed.
        if let Some(old) = old_ref.as_deref() {
            if Some(old) != new_ref.as_deref() {
                let _ = self.store.delete(old);
            }
        }

        let has_credentials = configuration.has_credentials();
        self.configuration.store(Arc::new(configuration.clone()));
        Ok((dto_from(&candidate, has_credentials), configuration))
    }

    /// Remove the saved credential. Idempotent. Only persists after the OS store
    /// confirms deletion.
    pub fn clear_credentials(
        &self,
        validator: &ProxyConfigValidator,
    ) -> Result<(ProxySettingsDto, ProxyConfiguration), ProxySettingsError> {
        let mut persisted = load_persisted_from(&self.settings_path)?;
        if let Some(reference) = persisted.global.credential_ref.take() {
            self.store
                .delete(&reference)
                .map_err(ProxySettingsError::Credential)?;
        }

        let configuration = self.configuration_for(&persisted)?;
        if let Err(message) = validator(&configuration) {
            return Err(ProxySettingsError::Invalid(message));
        }
        save_persisted_to(&self.settings_path, &persisted)?;
        self.configuration.store(Arc::new(configuration.clone()));
        Ok((dto_from(&persisted, false), configuration))
    }

    fn rollback_created(&self, created_ref: &Option<String>) {
        if let Some(reference) = created_ref.as_deref() {
            let _ = self.store.delete(reference);
        }
    }

    fn configuration_for(
        &self,
        persisted: &PersistedProxySettings,
    ) -> Result<ProxyConfiguration, ProxySettingsError> {
        match persisted.global.mode {
            ProxyMode::System => Ok(ProxyConfiguration::System),
            ProxyMode::Direct => Ok(ProxyConfiguration::Direct),
            ProxyMode::Custom => {
                let endpoint = persisted.global.endpoint.clone().ok_or_else(|| {
                    ProxySettingsError::ConfigInvalid(
                        "The saved custom proxy is missing its endpoint.".to_string(),
                    )
                })?;
                let endpoint = validate_endpoint(&endpoint)?;
                let no_proxy = validate_no_proxy(persisted.global.no_proxy.as_deref())?;
                let credentials = match persisted.global.credential_ref.as_deref() {
                    Some(reference) => {
                        let reference = normalize_credential_ref(reference).ok_or_else(|| {
                            ProxySettingsError::ConfigInvalid(
                                "The saved proxy credential reference is invalid.".to_string(),
                            )
                        })?;
                        self.store
                            .load(&reference)
                            .map_err(ProxySettingsError::Credential)?
                            .map(Arc::new)
                    }
                    None => None,
                };
                Ok(ProxyConfiguration::Custom(CustomProxyConfiguration {
                    endpoint,
                    no_proxy,
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
pub fn load_persisted_from(path: &Path) -> Result<PersistedProxySettings, ProxySettingsError> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| {
            ProxySettingsError::ConfigInvalid("The saved proxy settings are invalid.".to_string())
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(PersistedProxySettings::default())
        }
        Err(error) => Err(ProxySettingsError::ConfigUnavailable(format!(
            "Proxy settings could not be read: {error}"
        ))),
    }
}

/// Atomic tmp+rename save (mirrors the desktop-client config pattern).
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
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(settings).map_err(|_| {
        ProxySettingsError::ConfigInvalid("Proxy settings could not be serialized.".to_string())
    })?;
    std::fs::write(&temporary, bytes)
        .and_then(|_| std::fs::rename(&temporary, path))
        .map_err(|error| {
            ProxySettingsError::ConfigUnavailable(format!(
                "Proxy settings could not be saved: {error}"
            ))
        })
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
            self.entries.lock().unwrap().remove(reference);
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
    fn no_proxy_is_bounded_and_optional() {
        assert_eq!(validate_no_proxy(None).unwrap(), None);
        assert_eq!(validate_no_proxy(Some("   ")).unwrap(), None);
        assert_eq!(
            validate_no_proxy(Some(" localhost,127.0.0.1 ")).unwrap(),
            Some("localhost,127.0.0.1".to_string())
        );
        assert!(validate_no_proxy(Some("local host")).is_err());
        assert!(validate_no_proxy(Some("bad\nvalue")).is_err());
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
        assert!(input_debug.contains("<redacted>"));
    }

    #[test]
    fn set_persists_only_a_reference_and_never_secrets() {
        let store = Arc::new(MemoryCredentialStore::default());
        let path = temp_path("custom");
        let runtime = ProxyRuntime::new(store.clone(), path.clone());

        let (dto, configuration) = runtime
            .set(
                custom_input(
                    "https://proxy.example.com:8443",
                    Some("sensitive-user"),
                    Some("secret-pass"),
                ),
                &|_| Ok(()),
            )
            .expect("valid custom settings persist");

        assert_eq!(dto.mode, ProxyMode::Custom);
        assert!(dto.has_credentials);
        assert_eq!(
            dto.endpoint.as_deref(),
            Some("https://proxy.example.com:8443")
        );

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

        // Reloading from disk resolves the credential from the store and never
        // exposes it through the DTO.
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
            &|_| Ok(()),
        );
        assert!(matches!(
            result,
            Err(ProxySettingsError::Credential(CredentialError::Unavailable))
        ));

        // No settings file may exist, and no plaintext anywhere on disk.
        let file = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(!file.contains("secret-pass"));
        assert!(!file.contains("sensitive-user"));
        assert!(!file.contains("credentialRef"));
        assert!(!path.exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn validator_rejection_rolls_back_the_new_credential() {
        let store = Arc::new(MemoryCredentialStore::default());
        let path = temp_path("validator");
        let runtime = ProxyRuntime::new(store.clone(), path.clone());

        let result = runtime.set(
            custom_input(
                "https://proxy.example.com:8443",
                Some("sensitive-user"),
                Some("secret-pass"),
            ),
            &|_| Err("cannot build client".to_string()),
        );
        assert!(matches!(result, Err(ProxySettingsError::Invalid(_))));
        assert!(store.entries.lock().unwrap().is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn clear_credentials_deletes_the_secret_and_updates_the_file() {
        let store = Arc::new(MemoryCredentialStore::default());
        let path = temp_path("clear");
        let runtime = ProxyRuntime::new(store.clone(), path.clone());

        runtime
            .set(
                custom_input(
                    "https://proxy.example.com:8443",
                    Some("sensitive-user"),
                    Some("secret-pass"),
                ),
                &|_| Ok(()),
            )
            .unwrap();
        assert_eq!(store.entries.lock().unwrap().len(), 1);

        let (dto, configuration) = runtime.clear_credentials(&|_| Ok(())).unwrap();
        assert!(!dto.has_credentials);
        assert!(matches!(configuration, ProxyConfiguration::Custom(c) if c.credentials.is_none()));
        assert!(store.entries.lock().unwrap().is_empty());

        // Idempotent second call.
        let (dto_again, _) = runtime.clear_credentials(&|_| Ok(())).unwrap();
        assert!(!dto_again.has_credentials);
        let file = std::fs::read_to_string(&path).unwrap();
        assert!(!file.contains("credentialRef"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn switching_mode_drops_credentials_and_credential_errors_surface() {
        let store = Arc::new(MemoryCredentialStore::default());
        let path = temp_path("mode");
        let runtime = ProxyRuntime::new(store.clone(), path.clone());

        runtime
            .set(
                custom_input("https://proxy.example.com:8443", Some("user"), Some("pass")),
                &|_| Ok(()),
            )
            .unwrap();

        // Direct mode drops credentials and removes the stored secret.
        let (dto, configuration) = runtime
            .set(
                ProxySettingsInput {
                    mode: ProxyMode::Direct,
                    endpoint: None,
                    no_proxy: None,
                    username: None,
                    password: None,
                },
                &|_| Ok(()),
            )
            .unwrap();
        assert_eq!(dto.mode, ProxyMode::Direct);
        assert!(!dto.has_credentials);
        assert!(matches!(configuration, ProxyConfiguration::Direct));
        assert!(store.entries.lock().unwrap().is_empty());

        // A locked store surfaces as a credential error, not a fallback. The
        // config is written first while the store is healthy, then the store is
        // locked before resolving.
        runtime
            .set(
                custom_input("https://proxy.example.com:8443", Some("user"), Some("pass")),
                &|_| Ok(()),
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

        let missing_password = ProxySettingsInput {
            mode: ProxyMode::Custom,
            endpoint: Some("http://127.0.0.1:8080".to_string()),
            no_proxy: None,
            username: Some("user".to_string()),
            password: None,
        };
        assert!(matches!(
            runtime.set(missing_password, &|_| Ok(())),
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
            runtime.set(creds_in_system, &|_| Ok(())),
            Err(ProxySettingsError::Invalid(_))
        ));

        let _ = std::fs::remove_file(&path);
    }
}
