//! Account runtime container and non-secret profile persistence (Phase 6.1).
//!
//! This phase introduces the account map/container while preserving today's
//! single-active-account command behavior. Per-account super-properties,
//! per-account proxy overrides, and CDP port allocation/binding are later,
//! separately gated phases.
//!
//! Security: only presentation/binding metadata is persisted (`accounts.v1.json`).
//! A profile never contains a token, raw CDP auth material, super-properties,
//! proxy credential, or any other secret; those live in process memory only.
//!
//! Persistence mirrors the proxy settings subsystem: a versioned document saved
//! with a unique same-directory temp file followed by an atomic rename. A missing
//! file yields an empty registry; a corrupt/unsupported file is reported as an
//! actionable error and is never deleted.

use crate::discord_api::DiscordApiClient;
use crate::models::{AccountId, AccountProfile, DiscordUser};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Account profile file name inside the app config directory.
const ACCOUNTS_FILE: &str = "accounts.v1.json";
/// Only this version is accepted; anything else fails closed (migration gate).
pub const ACCOUNTS_VERSION: u32 = 1;

/// Absolute path of the account profile file.
pub fn accounts_path(app_config_dir: &Path) -> PathBuf {
    app_config_dir.join(ACCOUNTS_FILE)
}

// ============================================================================
// Errors (never carry secrets)
// ============================================================================

/// Failures from the account profile store. All variants are safe to surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountProfileError {
    /// The file could not be read/written on disk.
    Storage(String),
    /// The file is malformed, unsupported, or internally inconsistent.
    Invalid(String),
}

impl std::fmt::Display for AccountProfileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AccountProfileError::Storage(message) => formatter.write_str(message),
            AccountProfileError::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for AccountProfileError {}

// ============================================================================
// Persisted shape (secret-free, versioned)
// ============================================================================

/// Versioned, non-secret persisted account document.
///
/// `version` is required (no serde default) so an unversioned document fails
/// closed as unsupported instead of being silently accepted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PersistedAccounts {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<AccountId>,
    #[serde(default)]
    pub accounts: Vec<AccountProfile>,
}

impl Default for PersistedAccounts {
    fn default() -> Self {
        Self {
            version: ACCOUNTS_VERSION,
            active: None,
            accounts: Vec::new(),
        }
    }
}

// ============================================================================
// Account runtime
// ============================================================================

/// One account's mutable runtime state: its authenticated client slot, the
/// authenticated user, presentation metadata, and the client-publication gate.
///
/// All runtimes created by a [`AccountRegistry`] share the same publication gate
/// (a single `Arc<Mutex<()>>`), so a proxy settings transaction and a login
/// publication can never interleave even across an account switch.
pub struct AccountRuntime {
    id: AccountId,
    client: Mutex<Option<DiscordApiClient>>,
    user: Mutex<Option<DiscordUser>>,
    profile: Mutex<AccountProfile>,
    /// Shared with the registry and every other runtime. Read via
    /// `publication_gate()` (reserved for the Phase 6.2 command surface).
    #[allow(dead_code)]
    publication_gate: Arc<Mutex<()>>,
}

impl AccountRuntime {
    fn with_gate(id: AccountId, profile: AccountProfile, publication_gate: Arc<Mutex<()>>) -> Self {
        Self {
            id,
            client: Mutex::new(None),
            user: Mutex::new(None),
            profile: Mutex::new(profile),
            publication_gate,
        }
    }

    pub fn id(&self) -> &AccountId {
        &self.id
    }

    /// The shared client-publication coordination lock.
    ///
    /// Reserved for the Phase 6.2 command surface; exercised by tests now.
    #[allow(dead_code)]
    pub fn publication_gate(&self) -> Arc<Mutex<()>> {
        self.publication_gate.clone()
    }

    pub fn authenticated_user(&self) -> Option<DiscordUser> {
        self.user
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn client(&self) -> Option<DiscordApiClient> {
        self.client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Whether this runtime currently holds an authenticated client. Reserved for
    /// the Phase 6.2 account switcher; exercised by tests now.
    #[allow(dead_code)]
    pub fn has_client(&self) -> bool {
        self.client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }

    /// Replace the client slot. Callers must hold the publication gate.
    pub fn publish_client(&self, client: Option<DiscordApiClient>) {
        *self
            .client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = client;
    }

    pub fn profile(&self) -> AccountProfile {
        self.profile
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Replace the stored profile metadata. Used by activation/upsert so a
    /// caller-provided profile is never silently ignored.
    pub fn set_profile(&self, profile: AccountProfile) {
        *self
            .profile
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = profile;
    }

    /// Record a successful authentication: refresh the presentation fields, the
    /// last-known CDP port, and the last-used timestamp, then store the user.
    /// Callers must hold the publication gate.
    pub fn mark_authenticated(&self, user: &DiscordUser, cdp_port: Option<u16>, used_at_ms: u64) {
        self.profile
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .apply_authentication(user, cdp_port, used_at_ms);
        *self
            .user
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(user.clone());
    }

    /// Drop all online state. The persisted profile metadata is retained.
    /// Reserved for an explicit logout in a later phase.
    ///
    /// Acquires the shared publication gate itself so wiring this up later can
    /// never clear the client slot outside the coordination lock.
    #[allow(dead_code)]
    pub fn clear_authenticated(&self) {
        let _gate = self
            .publication_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.publish_client(None);
        *self
            .user
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

// ============================================================================
// Account registry
// ============================================================================

/// Map of account runtimes plus the active account id and shared publication
/// gate.
pub struct AccountRegistry {
    coordination_gate: Arc<Mutex<()>>,
    runtimes: Mutex<HashMap<AccountId, Arc<AccountRuntime>>>,
    active: Mutex<Option<AccountId>>,
    profiles_path: PathBuf,
    /// `Some(message)` when a load failed: the registry becomes read-only so a
    /// corrupt/unsupported file can never be overwritten until repaired.
    store_error: Mutex<Option<String>>,
}

impl AccountRegistry {
    /// Create an empty registry. Call [`AccountRegistry::load_from_disk`] to
    /// populate known profiles.
    pub fn new(profiles_path: PathBuf) -> Self {
        Self {
            coordination_gate: Arc::new(Mutex::new(())),
            runtimes: Mutex::new(HashMap::new()),
            active: Mutex::new(None),
            profiles_path,
            store_error: Mutex::new(None),
        }
    }

    /// Shared client-publication coordination lock. Every runtime created by this
    /// registry returns this same lock from `publication_gate`.
    pub fn coordination_gate(&self) -> Arc<Mutex<()>> {
        self.coordination_gate.clone()
    }

    /// The actionable reason persistence is disabled, if a load failed.
    pub fn persistence_error(&self) -> Option<String> {
        self.store_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Load persisted profiles. A missing file yields an empty (writable)
    /// registry. A corrupt/unsupported/inconsistent file returns an actionable
    /// error, is left untouched (never deleted), and disables persistence for
    /// this registry until a load succeeds (explicit repair path later).
    pub fn load_from_disk(&self) -> Result<(), AccountProfileError> {
        match self.try_load_from_disk() {
            Ok(()) => {
                *self
                    .store_error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
                Ok(())
            }
            Err(error) => {
                *self
                    .store_error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error.to_string());
                Err(error)
            }
        }
    }

    fn try_load_from_disk(&self) -> Result<(), AccountProfileError> {
        let document = load_accounts(&self.profiles_path)?;
        if document.version != ACCOUNTS_VERSION {
            return Err(AccountProfileError::Invalid(
                "The saved account profiles use an unsupported version. Open account settings to migrate them."
                    .to_string(),
            ));
        }

        let mut seen: HashMap<AccountId, AccountProfile> = HashMap::new();
        for profile in &document.accounts {
            if seen.insert(profile.id.clone(), profile.clone()).is_some() {
                return Err(AccountProfileError::Invalid(
                    "The saved account profiles contain a duplicate account id.".to_string(),
                ));
            }
        }
        if let Some(active) = &document.active {
            if !seen.contains_key(active) {
                return Err(AccountProfileError::Invalid(
                    "The saved account profiles reference an unknown active account.".to_string(),
                ));
            }
        }

        // Commit only after full validation.
        for profile in seen.into_values() {
            self.insert_offline_profile(profile);
        }
        *self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = document.active;
        Ok(())
    }

    fn ensure_writable(&self) -> Result<(), AccountProfileError> {
        if let Some(message) = self.persistence_error() {
            return Err(AccountProfileError::Invalid(format!(
                "Account profile persistence is disabled because the saved profiles could not be loaded ({message}). Repair or remove the file, then reload before making changes."
            )));
        }
        Ok(())
    }

    /// Persist all known profiles and the active id atomically. Rejected while a
    /// failed load has left the registry read-only. Reserved for a future
    /// explicit save/repair path; the login path uses `save_activation`.
    #[allow(dead_code)]
    pub fn persist(&self) -> Result<(), AccountProfileError> {
        self.ensure_writable()?;
        let document = self.snapshot_document(None);
        save_accounts(&self.profiles_path, &document)
    }

    /// Atomically save a candidate document that sets `active` to `id` and stores
    /// `profile` for that account, WITHOUT mutating any in-memory state. On
    /// failure the caller must commit nothing.
    pub fn save_activation(
        &self,
        id: &AccountId,
        profile: &AccountProfile,
    ) -> Result<(), AccountProfileError> {
        self.ensure_writable()?;
        let document = self.snapshot_document(Some((id.clone(), profile.clone())));
        save_accounts(&self.profiles_path, &document)
    }

    /// Build the persisted document from current in-memory profiles, optionally
    /// overriding one account's profile and the active id.
    fn snapshot_document(
        &self,
        activation: Option<(AccountId, AccountProfile)>,
    ) -> PersistedAccounts {
        let mut accounts: Vec<AccountProfile> = self
            .runtimes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|runtime| runtime.profile())
            .collect();

        let active = match activation {
            Some((id, profile)) => {
                accounts.retain(|existing| existing.id != id);
                let mut profile = profile;
                profile.id = id.clone();
                accounts.push(profile);
                Some(id)
            }
            None => self
                .active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
        };
        accounts.sort_by(|left, right| left.id.cmp(&right.id));

        PersistedAccounts {
            version: ACCOUNTS_VERSION,
            active,
            accounts,
        }
    }

    /// Get an existing runtime without creating one.
    pub fn runtime(&self, id: &AccountId) -> Option<Arc<AccountRuntime>> {
        self.runtimes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .cloned()
    }

    /// Get or create the runtime for `id`, seeding a profile from `user`.
    /// Reserved for the Phase 6.2 command surface; exercised by tests now.
    #[allow(dead_code)]
    pub fn ensure_runtime(
        &self,
        user: &DiscordUser,
    ) -> Result<Arc<AccountRuntime>, AccountProfileError> {
        let id = AccountId::from_user(user).map_err(|error| {
            AccountProfileError::Invalid(format!("The account id is invalid: {error}"))
        })?;
        self.ensure_runtime_for_id(
            id,
            AccountProfile::from_user(user).map_err(|error| {
                AccountProfileError::Invalid(format!("The account id is invalid: {error}"))
            })?,
        )
    }

    /// Get or create a runtime for an explicit id/profile. When the runtime
    /// already exists its stored profile is refreshed to `profile` so activation
    /// can update metadata instead of silently ignoring it.
    pub fn ensure_runtime_for_id(
        &self,
        id: AccountId,
        profile: AccountProfile,
    ) -> Result<Arc<AccountRuntime>, AccountProfileError> {
        let mut runtimes = self
            .runtimes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = runtimes.get(&id) {
            existing.set_profile(profile);
            return Ok(existing.clone());
        }
        let runtime = Arc::new(AccountRuntime::with_gate(
            id.clone(),
            profile,
            self.coordination_gate.clone(),
        ));
        runtimes.insert(id, runtime.clone());
        Ok(runtime)
    }

    /// Set the active account, creating its runtime if necessary and refreshing
    /// its stored profile metadata.
    pub fn activate(
        &self,
        id: AccountId,
        profile: AccountProfile,
    ) -> Result<Arc<AccountRuntime>, AccountProfileError> {
        let runtime = self.ensure_runtime_for_id(id.clone(), profile)?;
        *self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(id);
        Ok(runtime)
    }

    pub fn active_id(&self) -> Option<AccountId> {
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn active_runtime(&self) -> Option<Arc<AccountRuntime>> {
        self.active_id().and_then(|id| self.runtime(&id))
    }

    /// Snapshot of all known profiles (sorted by id for determinism). Reserved
    /// for the Phase 6.2 account switcher; exercised by tests now.
    #[allow(dead_code)]
    pub fn profiles(&self) -> Vec<AccountProfile> {
        let mut profiles: Vec<AccountProfile> = self
            .runtimes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|runtime| runtime.profile())
            .collect();
        profiles.sort_by(|left, right| left.id.cmp(&right.id));
        profiles
    }

    fn insert_offline_profile(&self, profile: AccountProfile) {
        let id = profile.id.clone();
        let mut runtimes = self
            .runtimes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        runtimes.entry(id.clone()).or_insert_with(|| {
            Arc::new(AccountRuntime::with_gate(
                id,
                profile,
                self.coordination_gate.clone(),
            ))
        });
    }
}

// ============================================================================
// Atomic persistence
// ============================================================================

fn load_accounts(path: &Path) -> Result<PersistedAccounts, AccountProfileError> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| {
            AccountProfileError::Invalid(
                "The saved account profiles are invalid. Open account settings to repair them."
                    .to_string(),
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(PersistedAccounts::default())
        }
        Err(error) => Err(AccountProfileError::Storage(format!(
            "Account profiles could not be read: {error}"
        ))),
    }
}

fn save_accounts(path: &Path, document: &PersistedAccounts) -> Result<(), AccountProfileError> {
    let parent = path.parent().ok_or_else(|| {
        AccountProfileError::Storage("Account profile path has no parent directory.".to_string())
    })?;
    std::fs::create_dir_all(parent).map_err(|error| {
        AccountProfileError::Storage(format!(
            "Account profile directory could not be created: {error}"
        ))
    })?;
    let bytes = serde_json::to_vec_pretty(document).map_err(|_| {
        AccountProfileError::Invalid("Account profiles could not be serialized.".to_string())
    })?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(ACCOUNTS_FILE);
    let temporary = parent.join(format!(
        ".{file_name}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    if let Err(error) =
        std::fs::write(&temporary, bytes).and_then(|_| std::fs::rename(&temporary, path))
    {
        let _ = std::fs::remove_file(&temporary);
        return Err(AccountProfileError::Storage(format!(
            "Account profiles could not be saved: {error}"
        )));
    }
    Ok(())
}

// ============================================================================
// Tests (pure; no real Discord/CDP/keyring)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "dqh-accounts-{label}-{}.json",
            uuid::Uuid::new_v4().simple()
        ))
    }

    fn user(id: &str, name: &str) -> DiscordUser {
        DiscordUser {
            id: id.to_string(),
            username: name.to_string(),
            discriminator: "0".to_string(),
            avatar: Some(format!("avatar-{name}")),
            global_name: Some(format!("{name} Display")),
            premium_type: None,
        }
    }

    fn test_client() -> DiscordApiClient {
        DiscordApiClient::new_with_proxy(
            "test-token".to_string(),
            crate::proxy_settings::ProxyConfiguration::Direct,
        )
        .unwrap()
    }

    #[test]
    fn account_id_validation_and_serialization() {
        let id = AccountId::parse("123456789012345678").unwrap();
        assert_eq!(id.as_str(), "123456789012345678");
        // Serde-transparent: serializes as a bare string and round-trips.
        assert_eq!(
            serde_json::to_value(&id).unwrap(),
            serde_json::json!("123456789012345678")
        );
        assert_eq!(
            serde_json::from_value::<AccountId>(serde_json::json!("123456789012345678")).unwrap(),
            id
        );
        // Deterministic from the Discord user.
        assert_eq!(
            AccountId::from_user(&user("42", "a")).unwrap().as_str(),
            "42"
        );

        for bad in [
            "",
            "   ",
            "0",
            "0000",
            "12a3",
            "-1",
            "12 34",
            &"9".repeat(64),
        ] {
            assert!(
                AccountId::parse(bad).is_err(),
                "id should be rejected: {bad:?}"
            );
        }
        assert!(serde_json::from_value::<AccountId>(serde_json::json!("not-a-snowflake")).is_err());
    }

    /// Recursively assert no persisted *key* looks like a secret. User-controlled
    /// display *values* are allowed to contain secret-like words.
    fn assert_no_secret_keys(value: &serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    let lower = key.to_ascii_lowercase();
                    for forbidden in [
                        "token",
                        "password",
                        "authorization",
                        "secret",
                        "proxy",
                        "super",
                    ] {
                        assert!(
                            !lower.contains(forbidden),
                            "persisted profile key must not be secret-like: {key}"
                        );
                    }
                    assert_no_secret_keys(child);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    assert_no_secret_keys(item);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn profile_persistence_round_trip_contains_no_secrets() {
        let path = temp_path("roundtrip");
        let registry = AccountRegistry::new(path.clone());

        // A user-controlled display name containing secret-like words is
        // legitimate non-secret metadata and must not trip the scan.
        let alice_user = user("111111111111111111", "token-password-super");
        let first = registry.ensure_runtime(&alice_user).unwrap();
        first.mark_authenticated(&alice_user, Some(9223), 1_700_000_000_000);
        registry
            .activate(AccountId::from_user(&alice_user).unwrap(), first.profile())
            .unwrap();

        let bob_user = user("222222222222222222", "bob");
        let second = registry.ensure_runtime(&bob_user).unwrap();
        second.mark_authenticated(&bob_user, None, 1_700_000_100_000);
        registry.persist().unwrap();

        let bytes = std::fs::read_to_string(&path).unwrap();
        // Structural scan: no secret-like keys anywhere.
        let value: serde_json::Value = serde_json::from_str(&bytes).unwrap();
        assert_no_secret_keys(&value);
        // A real token value must never be persisted.
        assert!(!bytes.contains("test-token"));
        // The user-controlled display name is retained verbatim (no false failure).
        let usernames: Vec<&str> = value["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|profile| profile["username"].as_str())
            .collect();
        assert!(usernames.contains(&"token-password-super"));

        // Reload into a fresh registry and confirm the profiles survive.
        let reloaded = AccountRegistry::new(path.clone());
        reloaded.load_from_disk().unwrap();
        let profiles = reloaded.profiles();
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].id.as_str(), "111111111111111111");
        assert_eq!(profiles[0].last_cdp_port, Some(9223));
        assert_eq!(profiles[0].username, "token-password-super");
        assert_eq!(profiles[1].username, "bob");
        assert_eq!(reloaded.active_id().unwrap().as_str(), "111111111111111111");
        // Loaded runtimes are offline: known, but no token/client in memory.
        for profile in profiles {
            let runtime = reloaded.runtime(&profile.id).unwrap();
            assert!(!runtime.has_client());
            assert!(runtime.authenticated_user().is_none());
        }

        let _ = std::fs::remove_file(&path);
    }

    // P1-1: a failed load disables persistence so the corrupt/unsupported file
    // can never be overwritten by a later upsert/login save.
    #[test]
    fn load_failure_disables_persistence_and_preserves_the_file() {
        let path = temp_path("readonly");
        let original = br#"{"version":2,"accounts":[]}"#;
        std::fs::write(&path, original).unwrap();

        let registry = AccountRegistry::new(path.clone());
        assert!(registry.load_from_disk().is_err());
        assert!(registry.persistence_error().is_some());

        let alice = user("111111111111111111", "alice");
        let runtime = registry.ensure_runtime(&alice).unwrap();
        runtime.mark_authenticated(&alice, Some(9223), 1);

        // Every persistence path is rejected.
        assert!(matches!(
            registry.persist(),
            Err(AccountProfileError::Invalid(_))
        ));
        assert!(matches!(
            registry.save_activation(&AccountId::from_user(&alice).unwrap(), &runtime.profile()),
            Err(AccountProfileError::Invalid(_))
        ));

        // The pre-existing bytes were never touched.
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let _ = std::fs::remove_file(&path);
    }

    // P1-3: a missing `version` field fails closed instead of defaulting to 1.
    #[test]
    fn missing_version_profile_file_fails_closed() {
        let path = temp_path("noversion");
        let original = br#"{"active":null,"accounts":[]}"#;
        std::fs::write(&path, original).unwrap();

        let registry = AccountRegistry::new(path.clone());
        assert!(matches!(
            registry.load_from_disk(),
            Err(AccountProfileError::Invalid(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let _ = std::fs::remove_file(&path);
    }

    // Hazard B: activation must refresh the stored profile metadata rather than
    // silently ignoring the passed profile.
    #[test]
    fn activate_refreshes_existing_profile_metadata() {
        let path = temp_path("refresh");
        let registry = AccountRegistry::new(path.clone());

        let alice = user("111111111111111111", "alice");
        let alice_id = AccountId::from_user(&alice).unwrap();
        let runtime = registry.ensure_runtime(&alice).unwrap();

        let mut updated = runtime.profile();
        updated.username = "alice-renamed".to_string();
        updated.last_cdp_port = Some(9333);
        updated.last_used_at_ms = Some(42);

        registry
            .activate(alice_id.clone(), updated.clone())
            .unwrap();

        assert_eq!(registry.runtime(&alice_id).unwrap().profile(), updated);
        assert_eq!(registry.active_id().unwrap(), alice_id);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_or_unsupported_profile_file_fails_closed_without_deleting() {
        let path = temp_path("corrupt");
        let registry = AccountRegistry::new(path.clone());

        std::fs::write(&path, b"{ not json").unwrap();
        assert!(matches!(
            registry.load_from_disk(),
            Err(AccountProfileError::Invalid(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"{ not json");

        std::fs::write(&path, br#"{"version":2,"accounts":[]}"#).unwrap();
        assert!(matches!(
            registry.load_from_disk(),
            Err(AccountProfileError::Invalid(_))
        ));
        assert!(path.exists());

        std::fs::write(
            &path,
            br#"{"version":1,"active":"999","accounts":[{"id":"111","username":"a","discriminator":"0","avatar":null,"globalName":null}]}"#,
        )
        .unwrap();
        assert!(registry.load_from_disk().is_err());
        assert!(path.exists());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_profile_file_loads_empty() {
        let path = temp_path("missing");
        let registry = AccountRegistry::new(path.clone());
        registry.load_from_disk().unwrap();
        assert!(registry.profiles().is_empty());
        assert!(registry.active_id().is_none());
        assert!(!path.exists());
    }

    #[test]
    fn switching_active_does_not_mutate_the_other_runtime() {
        let path = temp_path("switch");
        let registry = AccountRegistry::new(path.clone());

        let alice = user("111111111111111111", "alice");
        let bob = user("222222222222222222", "bob");

        let alice_runtime = registry.ensure_runtime(&alice).unwrap();
        alice_runtime.publish_client(Some(test_client()));
        alice_runtime.mark_authenticated(&alice, Some(9223), 10);
        registry
            .activate(
                AccountId::from_user(&alice).unwrap(),
                alice_runtime.profile(),
            )
            .unwrap();

        let bob_runtime = registry.ensure_runtime(&bob).unwrap();
        bob_runtime.mark_authenticated(&bob, Some(9333), 20);
        registry
            .activate(AccountId::from_user(&bob).unwrap(), bob_runtime.profile())
            .unwrap();

        // Bob is active; Alice's runtime/profile is untouched and still online.
        assert_eq!(registry.active_id().unwrap().as_str(), "222222222222222222");
        assert!(alice_runtime.has_client());
        assert_eq!(
            alice_runtime.authenticated_user().unwrap().username,
            "alice"
        );
        assert_eq!(alice_runtime.profile().last_cdp_port, Some(9223));
        assert_eq!(alice_runtime.profile().last_used_at_ms, Some(10));
        assert!(!bob_runtime.has_client());
        assert_eq!(bob_runtime.profile().last_used_at_ms, Some(20));

        // The shared publication gate is the same lock for every runtime.
        assert!(Arc::ptr_eq(
            &alice_runtime.publication_gate(),
            &bob_runtime.publication_gate()
        ));
        assert!(Arc::ptr_eq(
            &alice_runtime.publication_gate(),
            &registry.coordination_gate()
        ));

        let _ = std::fs::remove_file(&path);
    }
}
