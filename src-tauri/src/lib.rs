// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod account_runtime;
mod cdp_client;
mod cdp_game_spoof;
mod cdp_port_lease;
mod cdp_quest;
mod discord_api;
mod discord_cdp_commands;
mod discord_gateway;
mod game_simulator;
mod logger;
mod models;
mod platform_capabilities;
mod proxy_settings;
mod quest_completer;
pub mod quest_runtime;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod runtime_bridge;
mod runtime_identity;
mod super_properties;

use account_runtime::{AccountRegistry, AccountRuntime, OnlineAccountSession};
use cdp_port_lease::{
    AccountAcquireError, CdpLeaseHolder, CdpPortLease, CdpPortLeases, LeaseError,
};
use discord_api::DiscordApiClient;
use models::*;
use proxy_settings::{
    AccountClientBackend, KeyringCredentialStore, PreparedProxyTransport, ProxyConfiguration,
    ProxyRuntime,
};
use quest_runtime::{
    AdmittedRun, DoneWait, QuestEventSink, QuestKind, QuestOutcome, QuestRegistry, QuestResource,
    QuestTransport, ResourceCoordinator, ResourceGuard, StopClass, StopSignal,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use super_properties::SuperPropertiesHandle;
use tauri::ipc::Channel;
use tauri::{Emitter, Listener, Manager, State, WebviewWindowBuilder};

const APP_EXIT_RPC_DISCONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
/// Bound for waiting on a cancelled quest task. CDP cancel cleanup uses one
/// 15s evaluation; keep headroom for a poll-loop select to notice cancel.
const QUEST_STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(45);
/// Last-chance wait inside `exit_app_now` after the frontend's short prepare
/// deadline. Covers verified manual CDP cleanup (five 15s evaluations) plus
/// a cancelled quest task so `process::exit` does not abort in-flight rollback.
const APP_EXIT_FINAL_CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// Bound for a scoped stop waiting on a cancelled in-flight manual start to reach
/// a terminal state (never committed, or installed-then-removed). A hung start is
/// reported as an error rather than hanging the stop or faking success.
const MANUAL_START_STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(60);
/// Bound for a scoped stop waiting on a cleanup another caller already claimed.
const MANUAL_CLEANUP_WAIT: std::time::Duration = std::time::Duration::from_secs(90);

/// Tracks whether process-local exit cleanup and verified active-work cleanup
/// have completed. Local cleanup is one-shot; active-work cleanup stays
/// retryable until it succeeds so a failed CDP rollback cannot permanently
/// skip later `prepare_app_exit` calls.
struct AppExitCleanupState {
    prepared: AtomicBool,
    local_done: AtomicBool,
}

impl AppExitCleanupState {
    const fn new() -> Self {
        Self {
            prepared: AtomicBool::new(false),
            local_done: AtomicBool::new(false),
        }
    }

    fn is_prepared(&self) -> bool {
        self.prepared.load(Ordering::SeqCst)
    }

    fn mark_prepared(&self) {
        self.prepared.store(true, Ordering::SeqCst);
    }

    fn claim_local_cleanup(&self) -> bool {
        !self.local_done.swap(true, Ordering::SeqCst)
    }
}

static APP_EXIT_CLEANUP: AppExitCleanupState = AppExitCleanupState::new();

/// Global state: the account registry plus the quest registry, resource
/// coordinator, manual CDP slot, and the unified CDP port lease authority.
pub(crate) struct AppState {
    /// Account container: maps account ids to runtimes, tracks the active account,
    /// and owns the shared client-publication coordination gate.
    accounts: Arc<AccountRegistry>,
    quests: Arc<QuestRegistry>,
    resources: Arc<ResourceCoordinator>,
    manual_cdp_game: Arc<tokio::sync::Mutex<ManualCdpGameSessionState>>,
    /// The single authority for every migrated CDP port: account ownership,
    /// process-global exclusion, direct/scratch access, and operation lifetime all
    /// live here. There is no idle persistent `port -> account` binding.
    pub(crate) leases: Arc<CdpPortLeases>,
    /// Effective global proxy policy plus its (blocking) OS credential store.
    proxy: Arc<ProxyRuntime>,
}

/// Direct/scratch access: acquire an active direct lease for the whole
/// `operation` future and hold it until it resolves.
async fn with_direct_cdp_access<T, Op, Fut>(
    leases: &CdpPortLeases,
    cdp_port: u16,
    operation: Op,
) -> Result<T, String>
where
    Op: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let _lease = leases
        .acquire_direct(cdp_port)
        .map_err(|error| error.to_string())?;
    operation().await
}

/// Run `work` on a blocking thread with `lease` owned by that thread, and return
/// it with the result so later async/journal work can keep holding it.
///
/// This is the cancellation-safety primitive for process-mutating commands:
/// dropping or aborting the async waiter cannot release the lease while the
/// blocking operation (restore/launch/helper) continues. The lease is released
/// when the returned binding drops (typically at the end of the command).
pub(crate) async fn spawn_blocking_with_lease<L, T, F>(
    lease: L,
    work: F,
) -> Result<(T, L), tokio::task::JoinError>
where
    L: Send + 'static,
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(move || (work(), lease)).await
}

/// Acquire one direct CDP lease before beginning capture and return it with the
/// validated result. If capture fails or its waiter is cancelled, the lease is
/// dropped without reaching any commit operation.
async fn capture_with_direct_cdp_lease<T, Op, Fut>(
    leases: &CdpPortLeases,
    cdp_port: u16,
    capture: Op,
) -> Result<(T, CdpPortLease), String>
where
    Op: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let lease = leases
        .acquire_direct(cdp_port)
        .map_err(|error| error.to_string())?;
    let validated = capture().await?;
    Ok((validated, lease))
}

/// The active account's coherent online session (user AND client), or the legacy
/// not-authenticated error. This is the only authority path for an
/// account-targeted CDP start.
fn cdp_online_session(state: &State<'_, AppState>) -> Result<OnlineAccountSession, String> {
    state
        .accounts
        .active_online_session()
        .ok_or_else(|| "Not logged in".to_string())
}

/// One account-lease acquisition attempt that runs live identity verification.
async fn acquire_account_lease_once(
    state: &State<'_, AppState>,
    session: &OnlineAccountSession,
    cdp_port: u16,
) -> Result<CdpPortLease, AccountAcquireError<String>> {
    state
        .leases
        .acquire_account(cdp_port, session, || {
            verify_cdp_account_consistency(state, session, cdp_port)
        })
        .await
}

/// Acquire an account lease, preempting same-account work to terminal state and
/// retrying exactly once when `preempt` is set. Cross-account or direct
/// contention never preempts.
async fn acquire_account_lease(
    state: &State<'_, AppState>,
    session: &OnlineAccountSession,
    cdp_port: u16,
    preempt: bool,
) -> Result<CdpPortLease, String> {
    match acquire_account_lease_once(state, session, cdp_port).await {
        Ok(lease) => Ok(lease),
        Err(AccountAcquireError::Lease(LeaseError::Busy {
            holder: CdpLeaseHolder::Account(owner),
            ..
        })) if preempt && &owner == session.account_id() => {
            stop_account_work_internal(state, session.account_id()).await?;
            acquire_account_lease_once(state, session, cdp_port)
                .await
                .map_err(|error| error.to_string())
        }
        Err(error) => Err(error.to_string()),
    }
}

/// The active account's runtime, if any account is active.
fn active_account_runtime(state: &State<'_, AppState>) -> Option<Arc<AccountRuntime>> {
    state.accounts.active_runtime()
}

/// The active account's client, or the legacy "not logged in" error.
fn active_client(state: &State<'_, AppState>) -> Result<DiscordApiClient, String> {
    require_active_client(active_account_runtime(state).and_then(|runtime| runtime.client()))
}

/// Pure helper keeping the legacy single-account error semantics testable.
fn require_active_client(client: Option<DiscordApiClient>) -> Result<DiscordApiClient, String> {
    client.ok_or_else(|| "Not logged in".to_string())
}

/// The active account's client if present, for callers with an optional path.
fn optional_active_client(state: &State<'_, AppState>) -> Option<DiscordApiClient> {
    active_account_runtime(state).and_then(|runtime| runtime.client())
}

/// The active account id as a typed [`AccountId`], or the unauthenticated error
/// class when no account is active.
fn active_account_id(state: &State<'_, AppState>) -> Result<AccountId, String> {
    active_account_runtime(state)
        .map(|runtime| runtime.id().clone())
        .ok_or_else(|| "Not logged in".to_string())
}

/// The active account's identity handle, or a fresh non-authoritative scratch
/// handle when no account is active. A scratch handle is never attached to an
/// account runtime, so it can never serve an authenticated account request.
fn active_super_properties(state: &State<'_, AppState>) -> SuperPropertiesHandle {
    active_account_runtime(state)
        .map(|runtime| runtime.super_properties())
        .unwrap_or_default()
}

/// An immutable start-time snapshot of the account that owns one quest start.
///
/// Every quest-start path takes exactly one of these at entry and then uses ONLY
/// this snapshot for worker creation, CDP account consistency, legacy preemption,
/// and admission. Because the account id, authenticated user, and client are all
/// captured together, a concurrent account switch can never register account A's
/// client/token as a run owned by account B.
struct QuestStartContext {
    /// The snapshotted runtime. Retained so the context owns the exact account it
    /// resolved; the identity fields below are captured from it up front and are
    /// never re-read from the active account.
    #[allow(dead_code)]
    runtime: Arc<AccountRuntime>,
    account_id: AccountId,
    /// Retained for REST-start callers/tests; CDP starts use `OnlineAccountSession`.
    #[allow(dead_code)]
    authenticated_user: Option<DiscordUser>,
    client: Option<DiscordApiClient>,
}

impl QuestStartContext {
    /// The snapshotted authenticated user, or the legacy unauthenticated error.
    #[allow(dead_code)]
    fn require_authenticated_user(&self) -> Result<DiscordUser, String> {
        self.authenticated_user
            .clone()
            .ok_or_else(|| "Not logged in".to_string())
    }
}

/// Snapshot ONE account for a quest start. `require_client` selects the stricter
/// legacy contract used by the REST starts; the optional path still requires an
/// active runtime (so CDP consistency has an expected account) but tolerates a
/// missing client.
fn snapshot_quest_start(
    runtime: Option<Arc<AccountRuntime>>,
    require_client: bool,
) -> Result<QuestStartContext, String> {
    let runtime = require_active_account(runtime)?;
    let client = runtime.client();
    if require_client && client.is_none() {
        return Err("Not logged in".to_string());
    }
    Ok(QuestStartContext {
        account_id: runtime.id().clone(),
        authenticated_user: runtime.authenticated_user(),
        client,
        runtime,
    })
}

/// A start context for starts that must hold a REST client.
fn start_context_requiring_client(
    state: &State<'_, AppState>,
) -> Result<QuestStartContext, String> {
    snapshot_quest_start(active_account_runtime(state), true)
}

/// Build a fresh identity handle from ONLY a captured CDP session's
/// super-properties. No global manager is consulted, so the result can never
/// carry another account's identity.
fn identity_from_cdp_session(
    session: &cdp_client::CapturedDiscordSession,
) -> SuperPropertiesHandle {
    use base64::Engine as _;
    let identity = SuperPropertiesHandle::new();
    if let Some(base64) = session.super_properties.as_ref() {
        if let Ok(decoded_bytes) = base64::engine::general_purpose::STANDARD.decode(base64) {
            if let Ok(decoded) = serde_json::from_slice::<serde_json::Value>(&decoded_bytes) {
                identity.set_from_cdp(base64, &decoded);
            }
        }
    }
    identity
}

/// A CDP access target snapshotted BEFORE any await.
///
/// `Account` is produced only from a coherent `OnlineAccountSession` (user AND
/// client on the exact runtime), so a concurrent account switch cannot redirect
/// the operation and live CDP identity can be verified against the snapshot. A
/// restored/offline/user-only runtime is the non-authoritative `Direct` path: its
/// in-memory identity is never updated and `last_cdp_port` is metadata only.
enum CdpAccessTarget {
    Account(Box<OnlineAccountSession>),
    Direct(SuperPropertiesHandle),
}

impl CdpAccessTarget {
    fn handle(&self) -> SuperPropertiesHandle {
        match self {
            CdpAccessTarget::Account(session) => session.runtime().super_properties(),
            CdpAccessTarget::Direct(handle) => handle.clone(),
        }
    }
}

/// Snapshot the access target from a registry. The ONLY way to obtain an account
/// target is `AccountRegistry::active_online_session`, which itself coordinates
/// with the publication gate; outside `account_runtime` no code can mint an
/// `OnlineAccountSession` directly. Anything else is a non-authoritative Direct
/// target.
fn cdp_access_target_for_registry(registry: &AccountRegistry) -> CdpAccessTarget {
    match registry.active_online_session() {
        Some(session) => CdpAccessTarget::Account(Box::new(session)),
        None => CdpAccessTarget::Direct(SuperPropertiesHandle::new()),
    }
}

/// Snapshot the CDP access target BEFORE any CDP await. The publication gate is
/// held only for the coherent user+client read inside `active_online_session`
/// and released before any await.
fn cdp_access_target(state: &State<'_, AppState>) -> CdpAccessTarget {
    cdp_access_target_for_registry(state.accounts.as_ref())
}

/// Runs `operation` under unified CDP access. An account target acquires a
/// verified account lease; a direct target acquires a direct lease. Either lease
/// lives for the whole `operation` future, and a port held at entry runs no
/// operation at all.
async fn with_cdp_access<T, Op, Fut>(
    state: &State<'_, AppState>,
    target: &CdpAccessTarget,
    cdp_port: u16,
    operation: Op,
) -> Result<T, String>
where
    Op: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    match target {
        CdpAccessTarget::Account(session) => {
            let _lease = acquire_account_lease_once(state, session.as_ref(), cdp_port)
                .await
                .map_err(|error| error.to_string())?;
            operation().await
        }
        CdpAccessTarget::Direct(_) => {
            with_direct_cdp_access(state.leases.as_ref(), cdp_port, operation).await
        }
    }
}

/// Current wall-clock time as epoch milliseconds (for `last_used_at_ms`).
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Resolve the saved proxy policy off the main executor. Keyring access can
/// block (notably on Linux), so it runs on a blocking thread. A missing file
/// defaults to System; any corrupt/inconsistent/unreadable state fails closed so
/// proxy-dependent network activity is blocked until repaired.
async fn resolve_proxy_configuration(
    state: &State<'_, AppState>,
) -> Result<ProxyConfiguration, String> {
    let runtime = state.proxy.clone();
    tokio::task::spawn_blocking(move || runtime.resolve_current())
        .await
        .map_err(|error| format!("Proxy settings task failed: {error}"))?
        .map_err(|error| error.to_string())
}

/// Batch backend that rebuilds every *live* account client for an
/// account-proxy change. All of its calls happen while the caller holds the
/// registry coordination gate, so publication cannot interleave. Each account
/// client keeps its own shared request gate across the swap.
struct RegistryAccountClientBackend {
    registry: Arc<AccountRegistry>,
}

impl AccountClientBackend for RegistryAccountClientBackend {
    fn live_accounts(&self) -> Vec<AccountId> {
        self.registry
            .profiles()
            .into_iter()
            .map(|profile| profile.id)
            .filter(|id| {
                self.registry
                    .runtime(id)
                    .is_some_and(|runtime| runtime.has_client())
            })
            .collect()
    }

    fn prepare(
        &self,
        account_id: &AccountId,
        configuration: &ProxyConfiguration,
    ) -> Result<PreparedProxyTransport, String> {
        match self
            .registry
            .runtime(account_id)
            .and_then(|runtime| runtime.client())
        {
            Some(client) => client
                .prepare_proxy_configuration(configuration)
                .map(PreparedProxyTransport::new)
                .map_err(|error| {
                    format!(
                        "The proxy configuration could not be applied to account {}: {error}",
                        account_id.as_str()
                    )
                }),
            // If the client vanished mid-transaction, still build-validate the
            // candidate policy standalone so a bad policy is never persisted.
            None => discord_api::prepare_standalone_client(configuration)
                .map(PreparedProxyTransport::new)
                .map_err(|error| {
                    format!(
                        "The proxy configuration could not be applied to account {}: {error}",
                        account_id.as_str()
                    )
                }),
        }
    }

    fn install(
        &self,
        account_id: &AccountId,
        configuration: &ProxyConfiguration,
        prepared: PreparedProxyTransport,
    ) -> Result<(), String> {
        let Some(client) = self
            .registry
            .runtime(account_id)
            .and_then(|runtime| runtime.client())
        else {
            return Err(format!(
                "Account {} no longer has a live client.",
                account_id.as_str()
            ));
        };
        match prepared.into_inner::<discord_api::PreparedProxyClient>() {
            Some(prepared) => {
                client.install_proxy_configuration(configuration, prepared);
                Ok(())
            }
            None => Err(format!(
                "The proxy transport for account {} could not be swapped.",
                account_id.as_str()
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Coordinated active-account mutation
//
// All account runtimes created by the registry share ONE publication gate
// (`AccountRegistry::coordination_gate`, also returned by
// `AccountRuntime::publication_gate`). Every mutation that must stay consistent
// with the global proxy policy holds that gate for the whole critical section:
//
// * `coordinated_proxy_set` / `coordinated_clear_proxy_credentials` snapshot the
//   active account's client and run the whole persist+install transaction under
//   the gate.
// * `coordinated_publish_account` re-resolves the current on-disk policy, builds
//   the client, activates/updates the account runtime, and persists the profile
//   all under the same gate. Re-resolving at publish time is what guarantees a
//   settings change during login's CDP/network work is honored rather than
//   overwritten by a stale build.
// * `coordinated_add_account` keeps the known-ID check and optional publish in
//   one critical section; activation and final removal use this gate as well.
// * expected-ID CDP confirmation/reconnect compare the live capture before the
//   gate and keep the final known-account check plus publication inside it.
// * `coordinated_build_client` builds a throwaway validation client under the
//   gate; it is never published, so its network call runs outside the gate.
//
// Lock ordering is always `publication_gate` -> (`ProxyRuntime` transaction) and
// `publication_gate` -> registry/slot locks, so no path can deadlock.
// ---------------------------------------------------------------------------

fn coordinated_proxy_set(
    registry: &Arc<AccountRegistry>,
    runtime: &ProxyRuntime,
    input: ProxySettingsInput,
) -> Result<ProxySettingsDto, String> {
    let gate = registry.coordination_gate();
    let _gate = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // Batch rebuild every live account client with its own effective candidate
    // policy (overridden accounts keep their override; inheriting accounts
    // reflect the new global).
    let backend = RegistryAccountClientBackend {
        registry: Arc::clone(registry),
    };
    runtime
        .set(input, &backend)
        .map(|(dto, _)| dto)
        .map_err(|error| error.to_string())
}

/// Set one account's proxy override, rebuilding every live account client under
/// the shared coordination gate (batch rebuild). A failed rebuild rolls the
/// document back, so all clients keep a consistent previous policy.
fn coordinated_set_account_override(
    registry: &Arc<AccountRegistry>,
    runtime: &ProxyRuntime,
    account_id: AccountId,
    input: AccountProxyOverrideInput,
) -> Result<AccountProxySettingsDto, String> {
    let gate = registry.coordination_gate();
    let _gate = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let backend = RegistryAccountClientBackend {
        registry: Arc::clone(registry),
    };
    runtime
        .set_account_override(&account_id, input, &backend)
        .map_err(|error| error.to_string())
}

/// Remove one account's proxy override (restoring global inheritance) and
/// pending-delete its credential, rebuilding every live account client under the
/// coordination gate.
fn coordinated_clear_account_override(
    registry: &Arc<AccountRegistry>,
    runtime: &ProxyRuntime,
    account_id: AccountId,
) -> Result<AccountProxySettingsDto, String> {
    let gate = registry.coordination_gate();
    let _gate = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let backend = RegistryAccountClientBackend {
        registry: Arc::clone(registry),
    };
    runtime
        .clear_account_override(&account_id, &backend)
        .map_err(|error| error.to_string())
}

fn coordinated_clear_proxy_credentials(
    registry: &Arc<AccountRegistry>,
    runtime: &ProxyRuntime,
) -> Result<ProxySettingsDto, String> {
    let gate = registry.coordination_gate();
    let _gate = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let backend = RegistryAccountClientBackend {
        registry: Arc::clone(registry),
    };
    runtime
        .clear_credentials(&backend)
        .map(|(dto, _)| dto)
        .map_err(|error| error.to_string())
}

/// Everything needed to publish a freshly authenticated account.
struct PublishAccountRequest {
    id: AccountId,
    user: DiscordUser,
    cdp_port: Option<u16>,
    used_at_ms: u64,
    token: String,
    /// This account's captured identity; attached to the runtime and injected
    /// into the published client.
    identity: SuperPropertiesHandle,
}

/// Re-resolve the policy, build the client, atomically save the complete
/// candidate document (updated profile + new active id), then commit the
/// in-memory runtime/user/active state and publish the client — all under the
/// shared gate. This is the only place a login writes an account's client slot.
///
/// Ordering matters: the file is written FIRST and in-memory state is mutated
/// only after the save succeeds, so a save failure publishes nothing and leaves
/// the previous active/runtime state completely intact. The saved document also
/// carries the new active id, so a first login never records `active: null`.
fn coordinated_publish_account(
    registry: &AccountRegistry,
    runtime: &ProxyRuntime,
    resources: &ResourceCoordinator,
    request: PublishAccountRequest,
) -> Result<(), String> {
    let gate = registry.coordination_gate();
    let _gate = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    // There is no idle persistent port->account binding in the unified lease
    // design. Auto-login holds a direct lease for the whole capture->publish
    // transaction; `last_cdp_port` is display/relaunch metadata only.
    publish_account_under_gate(registry, runtime, resources, request)
}

/// The publication body, run while the caller holds the coordination gate.
///
/// Ordering matters: the file is written FIRST and in-memory state is mutated
/// only after the save succeeds, so a save failure publishes nothing and leaves
/// the previous active/runtime state completely intact. The saved document also
/// carries the new active id, so a first login never records `active: null`.
fn publish_account_under_gate(
    registry: &AccountRegistry,
    runtime: &ProxyRuntime,
    resources: &ResourceCoordinator,
    request: PublishAccountRequest,
) -> Result<(), String> {
    // Use the account's own effective policy (its override merged over global),
    // never the raw global policy, so publishing A cannot install B/global policy.
    let configuration = runtime
        .resolve_for_account(&request.id)
        .map_err(|error| error.to_string())?;
    // Build the client bound to this account's captured identity. The handle is
    // also attached to the runtime below, so both observe the same manager.
    let client = DiscordApiClient::new_account_bound(
        request.token,
        configuration,
        request.identity.clone(),
        resources.request_gate(&request.id),
    )
    .map_err(|error| format!("Failed to create API client: {error}"))?;

    // Build the complete candidate profile without mutating live state.
    let mut profile = match registry.runtime(&request.id) {
        Some(existing) => existing.profile(),
        None => AccountProfile::from_user(&request.user).map_err(|error| error.to_string())?,
    };
    profile.apply_authentication(&request.user, request.cdp_port, request.used_at_ms);

    // Persist the candidate (profile + active id) before committing anything.
    registry
        .save_activation(&request.id, &profile)
        .map_err(|error| error.to_string())?;

    // Commit in-memory state only after the atomic save succeeded.
    let account = registry
        .activate(request.id, profile)
        .map_err(|error| error.to_string())?;
    account.set_super_properties(request.identity.clone());
    account.mark_authenticated(&request.user, request.cdp_port, request.used_at_ms);
    account.publish_client(Some(client));
    Ok(())
}

/// Add a newly captured identity only when it is unknown. The known-ID check and
/// publication share the same coordination gate, so concurrent Adds have exactly
/// one publisher and a known account is never refreshed or activated as a side
/// effect of Add.
fn coordinated_add_account(
    registry: &AccountRegistry,
    runtime: &ProxyRuntime,
    resources: &ResourceCoordinator,
    request: PublishAccountRequest,
) -> Result<AddCdpResultDto, String> {
    let gate = registry.coordination_gate();
    let _gate = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    if registry.runtime(&request.id).is_some() {
        return Ok(AddCdpResultDto {
            user: request.user,
            already_known: true,
        });
    }

    let user = request.user.clone();
    publish_account_under_gate(registry, runtime, resources, request)?;
    Ok(AddCdpResultDto {
        user,
        already_known: false,
    })
}

/// Activate the current registry runtime while holding the same gate used by
/// account publication/removal. Resolve the runtime after taking the gate so a
/// concurrent removal cannot be followed by activation of a stale `Arc`.
fn coordinated_activate_account(
    registry: &AccountRegistry,
    id: &AccountId,
) -> Result<Arc<AccountRuntime>, String> {
    let gate = registry.coordination_gate();
    let _gate = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let runtime = registry
        .runtime(id)
        .ok_or_else(|| "Unknown account.".to_string())?;
    activate_runtime_under_gate(registry, id, runtime, false)
}

/// Online-only activation. The coherent user/client check and the persisted
/// active-id save plus in-memory activation are serialized under the registry
/// gate; no asynchronous work is performed while it is held.
fn coordinated_activate_online_account(
    registry: &AccountRegistry,
    id: &AccountId,
) -> Result<Arc<AccountRuntime>, String> {
    let gate = registry.coordination_gate();
    let _gate = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let runtime = registry
        .runtime(id)
        .ok_or_else(|| "Unknown account.".to_string())?;
    let Some((user, _client)) = runtime.online_session() else {
        return Err("Account is offline. Reconnect it before switching.".to_string());
    };
    if AccountId::from_user(&user).ok().as_ref() != Some(id) {
        return Err("The authenticated session does not match the selected account.".to_string());
    }
    activate_runtime_under_gate(registry, id, runtime, true)
}

/// Shared activation mutation body. Callers must hold `coordination_gate` and
/// must perform any path-specific eligibility checks before entering here.
fn activate_runtime_under_gate(
    registry: &AccountRegistry,
    id: &AccountId,
    runtime: Arc<AccountRuntime>,
    persist_active: bool,
) -> Result<Arc<AccountRuntime>, String> {
    let profile = runtime.profile();
    if persist_active {
        registry
            .save_activation(id, &profile)
            .map_err(|error| error.to_string())?;
    }
    registry
        .activate(id.clone(), profile)
        .map_err(|error| error.to_string())
}

/// Serialize only the final registry removal. Callers finish all stop, manual
/// cleanup, and proxy work before acquiring the gate here.
fn coordinated_remove_account(registry: &AccountRegistry, id: &AccountId) -> Result<bool, String> {
    let gate = registry.coordination_gate();
    let _gate = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.remove(id).map_err(|error| error.to_string())
}

/// Build a throwaway validation client from a policy snapshot taken under the
/// gate. Never published, so the following network call runs outside the gate.
fn coordinated_build_client(
    registry: &AccountRegistry,
    runtime: &ProxyRuntime,
    account_id: &AccountId,
    token: String,
    identity: SuperPropertiesHandle,
) -> Result<DiscordApiClient, String> {
    let gate = registry.coordination_gate();
    let _gate = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let configuration = runtime
        .resolve_for_account(account_id)
        .map_err(|error| error.to_string())?;
    DiscordApiClient::new_with_super_properties(token, configuration, identity)
        .map_err(|error| format!("Could not validate the desktop client account: {error}"))
}

/// Which manual CDP spoof sessions a stop request is allowed to terminate.
///
/// Legacy preemption and `stop_quest` use [`ManualStopScope::Account`] so an
/// account switch can never make account B stop a spoof owned by account A. Only
/// the process-exit cleanup uses [`ManualStopScope::All`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum ManualStopScope {
    All,
    Account(AccountId),
}

impl ManualStopScope {
    fn matches(&self, owner: &AccountId) -> bool {
        match self {
            ManualStopScope::All => true,
            ManualStopScope::Account(id) => id == owner,
        }
    }
}

/// The account-scoped claim on one manual spoof's verified cleanup. Produced by
/// [`ManualCdpGameSessionState::claim_cleanup`] under the session lock; the
/// actual (network) cleanup runs after the lock is released. Deliberately NOT
/// `Clone`: a cleanup claim is a unique token carrying the session generation, so
/// a stale claim cannot be duplicated onto a newer session.
#[derive(Debug, PartialEq, Eq)]
struct ClaimedManualSession {
    owner: AccountId,
    /// The generation of the session this claim was taken from. Every finalizer
    /// validates it, so a late claim from session A can never touch session B.
    generation: u64,
    session: ManualCdpGameSimulation,
}

/// The lifecycle of the single process-global manual CDP spoof slot.
///
/// Unlike a bare `Option`, this distinguishes "nothing is recorded" from a
/// pending startup or an in-flight cleanup. A stop request must be able to see
/// and act on a `Starting`/`CleaningUp` session; treating those as absent (as a
/// lossy `Option` did) let process exit report success while a spoof was still
/// about to be committed or a cleanup was still running.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ManualSessionStatus {
    /// No manual spoof is recorded and no startup/cleanup is in flight.
    Absent,
    /// A startup owns the slot and is installing its spoof. The session lock is
    /// NOT held across the CDP awaits, so a read-only status query never blocks.
    Starting { owner: AccountId, generation: u64 },
    /// A committed spoof is active for `owner`.
    Active {
        owner: AccountId,
        generation: u64,
        session: ManualCdpGameSimulation,
    },
    /// A verified cleanup for `owner` is in flight. The session lock is NOT held
    /// across the cleanup await, so a status query stays live.
    CleaningUp { owner: AccountId, generation: u64 },
}

/// Single process-global manual CDP spoof slot plus a change notifier so a stop
/// can await a terminal transition without holding the session lock.
#[derive(Debug)]
struct ManualCdpGameSessionState {
    status: ManualSessionStatus,
    /// Set when a stop asks the in-flight `Starting` session to cancel. Only
    /// meaningful while `status` is `Starting`; `commit_start` refuses while set.
    start_cancelled: bool,
    /// Held for the whole manual spoof lifetime so account activity and the CDP
    /// port stay reserved until verified cleanup finishes.
    guards: Vec<ResourceGuard>,
    /// The unified CDP port lease for this spoof. Held from `Starting` through
    /// `Active`/`CleaningUp` and dropped only on verified cleanup (or a
    /// failed/cancelled start). While held, a direct or different-account
    /// competitor stays blocked.
    lease: Option<CdpPortLease>,
    /// Monotonic revision bumped on every state mutation and broadcast so waiters
    /// can await a specific terminal transition without busy-polling.
    revision: u64,
    /// Checked, never-reused session generation counter. Each `begin_start`
    /// allocates the next value; every finalizer validates owner AND generation so
    /// a stale task from a previous session can never touch a newer one.
    next_generation: u64,
    changes: tokio::sync::watch::Sender<u64>,
}

impl Default for ManualCdpGameSessionState {
    fn default() -> Self {
        Self::new()
    }
}

impl ManualCdpGameSessionState {
    fn new() -> Self {
        let (changes, _receiver) = tokio::sync::watch::channel(0);
        Self {
            status: ManualSessionStatus::Absent,
            start_cancelled: false,
            guards: Vec::new(),
            lease: None,
            revision: 0,
            next_generation: 0,
            changes,
        }
    }

    /// Publish a state change so a waiter blocked on the previous revision wakes.
    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        let _ = self.changes.send(self.revision);
    }

    /// Subscribe to state changes. Clone the receiver BEFORE reading state so a
    /// mutation between the read and `changed()` can never be lost.
    fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn status(&self) -> &ManualSessionStatus {
        &self.status
    }

    /// Whether the exact `(owner, generation)` startup is in flight.
    fn is_starting_for(&self, owner: &AccountId, generation: u64) -> bool {
        matches!(
            &self.status,
            ManualSessionStatus::Starting { owner: current, generation: current_generation }
                if current == owner && *current_generation == generation
        )
    }

    /// Whether the exact `(owner, generation)` cleanup is in flight.
    fn is_cleaning_up_for(&self, owner: &AccountId, generation: u64) -> bool {
        matches!(
            &self.status,
            ManualSessionStatus::CleaningUp { owner: current, generation: current_generation }
                if current == owner && *current_generation == generation
        )
    }

    /// Whether a stop has asked the exact `(owner, generation)` startup to cancel.
    fn start_cancelled_for(&self, owner: &AccountId, generation: u64) -> bool {
        self.start_cancelled && self.is_starting_for(owner, generation)
    }

    fn ensure_idle(&self) -> Result<(), String> {
        match &self.status {
            ManualSessionStatus::Absent => Ok(()),
            ManualSessionStatus::Starting { .. } => {
                Err("A manual CDP game simulation is already starting".to_string())
            }
            ManualSessionStatus::Active { session, .. } => Err(format!(
                "A manual CDP game simulation is already active for {}",
                session.app_name
            )),
            ManualSessionStatus::CleaningUp { .. } => {
                Err("A manual CDP game simulation is still stopping".to_string())
            }
        }
    }

    /// Reserve the single process-global manual slot for `owner` before the CDP
    /// startup awaits, taking ownership of the lifetime guards. Returns with no
    /// lock held so status queries stay live. On the `Err` path the guards are
    /// dropped, releasing the reserved resources.
    /// Reserve the slot for `owner`, allocating the next checked session
    /// generation. Returns that generation; every later finalizer must present it.
    fn begin_start(
        &mut self,
        owner: AccountId,
        guards: Vec<ResourceGuard>,
        lease: Option<CdpPortLease>,
    ) -> Result<u64, String> {
        self.ensure_idle()?;
        let generation = self.next_generation.checked_add(1).ok_or_else(|| {
            "The manual CDP game simulation generation space is exhausted.".to_string()
        })?;
        self.next_generation = generation;
        self.status = ManualSessionStatus::Starting { owner, generation };
        self.start_cancelled = false;
        self.guards = guards;
        self.lease = lease;
        self.touch();
        Ok(generation)
    }

    /// Commit a successful start. Only the still-owning, matching-generation,
    /// un-cancelled start may commit, so a cancelled or superseded start (or a
    /// stale finalizer from a previous session) can never install its spoof into
    /// the recorded session.
    fn commit_start(
        &mut self,
        owner: &AccountId,
        generation: u64,
        session: ManualCdpGameSimulation,
    ) -> Result<(), String> {
        match &self.status {
            ManualSessionStatus::Starting {
                owner: current,
                generation: current_generation,
            } if current == owner && *current_generation == generation && !self.start_cancelled => {
                self.status = ManualSessionStatus::Active {
                    owner: owner.clone(),
                    generation,
                    session,
                };
                self.touch();
                Ok(())
            }
            ManualSessionStatus::Starting {
                owner: current,
                generation: current_generation,
            } if current == owner && *current_generation == generation && self.start_cancelled => {
                Err("The manual CDP game simulation start was cancelled".to_string())
            }
            _ => Err("The manual CDP game simulation start was superseded".to_string()),
        }
    }

    /// Ask the exact in-flight `(owner, generation)` startup to cancel.
    /// `commit_start` then refuses, and the startup removes any spoof it already
    /// installed before clearing the slot. No-op for any other generation.
    fn request_cancel(&mut self, owner: &AccountId, generation: u64) -> bool {
        if self.is_starting_for(owner, generation) && !self.start_cancelled {
            self.start_cancelled = true;
            self.touch();
            return true;
        }
        false
    }

    /// Abandon a failed or cancelled start of the exact `(owner, generation)`,
    /// releasing the reserved (never activated) slot and its guards. A stale
    /// generation is a no-op.
    fn cancel_start(&mut self, owner: &AccountId, generation: u64) {
        if self.is_starting_for(owner, generation) {
            self.status = ManualSessionStatus::Absent;
            self.start_cancelled = false;
            self.guards.clear();
            // A failed/cancelled startup releases its port lease.
            self.lease = None;
            self.touch();
        }
    }

    /// Record a spoof that was installed but could not be removed after the exact
    /// `(owner, generation)` start was cancelled or superseded. Keeping it
    /// `Active` (rather than silently clearing) means a later stop or process exit
    /// retries the verified cleanup instead of leaking an untracked injection. A
    /// stale generation is a no-op.
    fn park_installed_but_uncommitted(
        &mut self,
        owner: &AccountId,
        generation: u64,
        session: ManualCdpGameSimulation,
    ) {
        if self.is_starting_for(owner, generation) {
            self.status = ManualSessionStatus::Active {
                owner: owner.clone(),
                generation,
                session,
            };
            self.start_cancelled = false;
            self.touch();
        }
    }

    fn active(&self) -> Option<ManualCdpGameSimulation> {
        match &self.status {
            ManualSessionStatus::Active { session, .. } => Some(session.clone()),
            _ => None,
        }
    }

    /// The active session only when `owner` owns it.
    #[cfg(test)]
    fn active_for(&self, owner: &AccountId) -> Option<ManualCdpGameSimulation> {
        match &self.status {
            ManualSessionStatus::Active {
                owner: current,
                session,
                ..
            } if current == owner => Some(session.clone()),
            _ => None,
        }
    }

    /// Claim this session's verified cleanup when `scope` permits it. Acquires no
    /// await and returns immediately so the caller can release the session lock
    /// before the network cleanup. Transitions `Active -> CleaningUp`.
    fn claim_cleanup(&mut self, scope: ManualStopScope) -> Option<ClaimedManualSession> {
        let ManualSessionStatus::Active {
            owner,
            generation,
            session,
        } = &self.status
        else {
            return None;
        };
        if !scope.matches(owner) {
            return None;
        }
        let claimed = ClaimedManualSession {
            owner: owner.clone(),
            generation: *generation,
            session: session.clone(),
        };
        self.status = ManualSessionStatus::CleaningUp {
            owner: owner.clone(),
            generation: *generation,
        };
        self.touch();
        Some(claimed)
    }

    /// Finish a claimed cleanup after re-validating ownership. `Ok(())` clears
    /// the session (dropping guards releases resources); `Err` restores it to
    /// `Active` for retry. A claim whose owner changed while awaiting never
    /// clears another account's session.
    fn finish_cleanup(
        &mut self,
        claimed: &ClaimedManualSession,
        result: Result<(), String>,
    ) -> Result<(), String> {
        // Validate owner AND generation: a stale finalizer from an earlier
        // same-owner session must never clear or mutate a newer one.
        let still_claimed = matches!(
            &self.status,
            ManualSessionStatus::CleaningUp { owner, generation }
                if owner == &claimed.owner && *generation == claimed.generation
        );
        if !still_claimed {
            return result;
        }
        match result {
            Ok(()) => {
                self.clear();
                Ok(())
            }
            Err(error) => {
                self.status = ManualSessionStatus::Active {
                    owner: claimed.owner.clone(),
                    generation: claimed.generation,
                    session: claimed.session.clone(),
                };
                self.touch();
                Err(error)
            }
        }
    }

    fn clear(&mut self) {
        self.status = ManualSessionStatus::Absent;
        self.start_cancelled = false;
        // Dropping the guards releases the reserved resources; dropping the
        // lease releases the CDP port.
        self.guards.clear();
        self.lease = None;
        self.touch();
    }
}

#[cfg(test)]
mod manual_cdp_game_session_tests {
    use super::*;

    fn session(name: &str) -> ManualCdpGameSimulation {
        ManualCdpGameSimulation {
            app_id: "123456".to_string(),
            app_name: name.to_string(),
            cdp_port: 9223,
        }
    }

    fn account(id: &str) -> AccountId {
        AccountId::parse(id).unwrap()
    }

    fn alice() -> AccountId {
        account("111111111111111111")
    }

    fn bob() -> AccountId {
        account("222222222222222222")
    }

    /// Record an active spoof owned by `owner`, mirroring a successful start.
    fn activate_for(state: &mut ManualCdpGameSessionState, owner: &AccountId, name: &str) {
        let generation = state.begin_start(owner.clone(), Vec::new(), None).unwrap();
        state
            .commit_start(owner, generation, session(name))
            .unwrap();
    }

    #[test]
    fn only_one_manual_cdp_game_can_be_active() {
        let mut state = ManualCdpGameSessionState::default();
        state.ensure_idle().unwrap();
        activate_for(&mut state, &alice(), "First");

        assert!(state.ensure_idle().is_err());
        assert_eq!(state.active().unwrap().app_name, "First");
    }

    #[test]
    fn failed_start_does_not_record_a_session() {
        let state = ManualCdpGameSessionState::default();
        state.ensure_idle().unwrap();

        // CDP startup failed before commit_start() was called.
        assert!(state.active().is_none());
    }

    #[test]
    fn cleanup_failure_keeps_the_session_for_retry() {
        let mut state = ManualCdpGameSessionState::default();
        activate_for(&mut state, &alice(), "Retry Me");

        let claimed = state
            .claim_cleanup(ManualStopScope::Account(alice()))
            .unwrap();
        assert!(state
            .finish_cleanup(&claimed, Err("Discord target disconnected".to_string()))
            .is_err());
        assert_eq!(state.active().unwrap().app_name, "Retry Me");

        let claimed = state
            .claim_cleanup(ManualStopScope::Account(alice()))
            .unwrap();
        state.finish_cleanup(&claimed, Ok(())).unwrap();
        assert!(state.active().is_none());
    }

    // The manual spoof's unified CDP port lease lives through Starting, Active,
    // and CleaningUp, survives a cleanup failure, and releases only after a
    // verified cleanup.
    #[tokio::test]
    async fn manual_spoof_holds_the_port_lease_until_verified_cleanup() {
        let alice = alice();
        let leases = CdpPortLeases::new();
        let lease = leases.acquire_direct(9223).unwrap();

        let sessions = tokio::sync::Mutex::new(ManualCdpGameSessionState::default());
        let generation = sessions
            .lock()
            .await
            .begin_start(alice.clone(), Vec::new(), Some(lease))
            .unwrap();
        assert!(leases.snapshot(9223).is_some());

        // Competitors stay blocked while the spoof start is in flight.
        assert!(leases.acquire_direct(9223).is_err());

        sessions
            .lock()
            .await
            .commit_start(&alice, generation, session("Held"))
            .unwrap();
        assert!(leases.snapshot(9223).is_some());

        // A failed verified cleanup keeps the session and its lease.
        let failed = run_scoped_manual_stop(
            &sessions,
            ManualStopScope::Account(alice.clone()),
            |_| async { Err("cleanup failed".to_string()) },
        )
        .await;
        assert!(failed.is_err());
        assert!(sessions.lock().await.active().is_some());
        assert!(leases.snapshot(9223).is_some());

        // A verified cleanup releases the lease.
        let cleaned = run_scoped_manual_stop(
            &sessions,
            ManualStopScope::Account(alice.clone()),
            |_| async { Ok(()) },
        )
        .await;
        assert!(cleaned.is_ok());
        assert!(sessions.lock().await.active().is_none());
        assert!(leases.snapshot(9223).is_none());
        assert!(leases.acquire_direct(9223).is_ok());
    }

    // A failed/cancelled manual startup drops its lease.
    #[tokio::test]
    async fn failed_manual_startup_drops_the_port_lease() {
        let alice = alice();
        let leases = CdpPortLeases::new();
        let lease = leases.acquire_direct(9223).unwrap();

        let mut state = ManualCdpGameSessionState::default();
        let generation = state
            .begin_start(alice.clone(), Vec::new(), Some(lease))
            .unwrap();
        assert!(leases.snapshot(9223).is_some());

        state.cancel_start(&alice, generation);
        assert!(leases.snapshot(9223).is_none());
        assert!(leases.acquire_direct(9223).is_ok());
    }

    #[test]
    fn session_uses_the_frontend_camel_case_contract() {
        let value = serde_json::to_value(session("Contract")).unwrap();
        assert_eq!(value["appId"], "123456");
        assert_eq!(value["appName"], "Contract");
        assert_eq!(value["cdpPort"], 9223);
        assert!(value.get("app_id").is_none());
    }

    // P1-3: account B's legacy stop/preemption must never claim or stop a spoof
    // owned by account A; only A's own flow may.
    #[test]
    fn account_b_flow_never_stops_account_as_spoof() {
        let mut state = ManualCdpGameSessionState::default();
        activate_for(&mut state, &alice(), "Alice Game");

        assert!(
            state
                .claim_cleanup(ManualStopScope::Account(bob()))
                .is_none(),
            "account B must not claim account A's spoof"
        );
        assert!(state.active_for(&alice()).is_some());
        assert!(state.active_for(&bob()).is_none());

        // A's own account-scoped flow can claim it.
        assert!(state
            .claim_cleanup(ManualStopScope::Account(alice()))
            .is_some());
    }

    // P1-3: the process-exit cleanup is the explicit all-account path and stops a
    // spoof regardless of which account owns it.
    #[test]
    fn all_account_exit_cleanup_stops_any_owners_spoof() {
        let mut state = ManualCdpGameSessionState::default();
        activate_for(&mut state, &alice(), "Alice Game");

        let claimed = state
            .claim_cleanup(ManualStopScope::All)
            .expect("exit cleanup claims any owner");
        assert_eq!(claimed.owner, alice());
        state.finish_cleanup(&claimed, Ok(())).unwrap();
        assert!(state.active().is_none());
    }

    // P1-3: a read-only status query must not block for the duration of another
    // account's spoof startup. The startup awaits run without the session lock.
    #[tokio::test]
    async fn status_does_not_block_while_another_accounts_startup_is_in_flight() {
        let sessions = Arc::new(tokio::sync::Mutex::new(ManualCdpGameSessionState::default()));
        let generation = {
            let mut guard = sessions.lock().await;
            guard.begin_start(alice(), Vec::new(), None).unwrap()
        };

        // Model the in-flight CDP startup awaits; they hold no session lock.
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let startup_sessions = Arc::clone(&sessions);
        let owner = alice();
        let startup = tokio::spawn(async move {
            let _ = release_rx.await;
            let mut guard = startup_sessions.lock().await;
            guard
                .commit_start(&owner, generation, session("Alice Game"))
                .unwrap();
        });

        let status = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            sessions.lock().await.active()
        })
        .await
        .expect("a read-only status query must not block on an in-flight start");
        assert!(status.is_none());

        release_tx.send(()).unwrap();
        startup.await.unwrap();
        assert!(sessions.lock().await.active().is_some());
    }

    // P1-A: a stop that lands during a start must cancel the in-flight startup,
    // wait for its terminal state, and never report success while the spoof could
    // still be committed.
    #[tokio::test]
    async fn stop_during_starting_cancels_and_waits_for_terminal_state() {
        let sessions = Arc::new(tokio::sync::Mutex::new(ManualCdpGameSessionState::default()));
        let owner = alice();
        let generation = sessions
            .lock()
            .await
            .begin_start(owner.clone(), Vec::new(), None)
            .unwrap();

        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let startup_sessions = Arc::clone(&sessions);
        let startup_owner = owner.clone();
        let startup = tokio::spawn(async move {
            // Wait until the stop has requested cancellation, then hold the
            // terminal transition open so the test can prove the stop waits.
            while !startup_sessions
                .lock()
                .await
                .start_cancelled_for(&startup_owner, generation)
            {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
            let _ = release_rx.await;
            // The cancelled start installed nothing and clears its reservation.
            startup_sessions
                .lock()
                .await
                .cancel_start(&startup_owner, generation);
        });

        let stop_sessions = Arc::clone(&sessions);
        let stop_owner = owner.clone();
        let stopper = tokio::spawn(async move {
            run_scoped_manual_stop(
                &stop_sessions,
                ManualStopScope::Account(stop_owner.clone()),
                |_port| async { Ok::<(), String>(()) },
            )
            .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !stopper.is_finished(),
            "stop must await the cancelled start's terminal state"
        );

        release_tx.send(()).unwrap();
        assert!(stopper.await.unwrap().is_ok());
        startup.await.unwrap();

        assert!(sessions.lock().await.active().is_none());
        // A cancelled start can never commit its spoof afterwards.
        assert!(sessions
            .lock()
            .await
            .commit_start(&owner, generation, session("Must Not Survive"))
            .is_err());
    }

    // P1-A: a stop that lands while a cleanup is already claimed must await the
    // cleanup's completion instead of reporting success early.
    #[tokio::test]
    async fn stop_during_cleanup_awaits_completion() {
        let sessions = Arc::new(tokio::sync::Mutex::new(ManualCdpGameSessionState::default()));
        let owner = alice();
        {
            let mut guard = sessions.lock().await;
            let generation = guard.begin_start(owner.clone(), Vec::new(), None).unwrap();
            guard
                .commit_start(&owner, generation, session("Alice Game"))
                .unwrap();
        }
        let claimed = sessions
            .lock()
            .await
            .claim_cleanup(ManualStopScope::Account(owner.clone()))
            .unwrap();

        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let cleanup_sessions = Arc::clone(&sessions);
        let cleaner = tokio::spawn(async move {
            let _ = release_rx.await;
            let mut guard = cleanup_sessions.lock().await;
            guard.finish_cleanup(&claimed, Ok(())).unwrap();
        });

        let stop_sessions = Arc::clone(&sessions);
        let stop_owner = owner.clone();
        let stopper = tokio::spawn(async move {
            run_scoped_manual_stop(
                &stop_sessions,
                ManualStopScope::Account(stop_owner.clone()),
                |_port| async { Err("the stop must not run its own cleanup".to_string()) },
            )
            .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !stopper.is_finished(),
            "stop must await an in-flight cleanup"
        );

        release_tx.send(()).unwrap();
        cleaner.await.unwrap();
        assert!(stopper.await.unwrap().is_ok());
        assert!(sessions.lock().await.active().is_none());
    }

    // P1-A: a failed verified cleanup is reported accurately and retained for a
    // later retry rather than being reported as a successful stop.
    #[tokio::test]
    async fn stop_reports_cleanup_failure_and_keeps_session_for_retry() {
        let sessions = Arc::new(tokio::sync::Mutex::new(ManualCdpGameSessionState::default()));
        let owner = alice();
        {
            let mut guard = sessions.lock().await;
            let generation = guard.begin_start(owner.clone(), Vec::new(), None).unwrap();
            guard
                .commit_start(&owner, generation, session("Retry Me"))
                .unwrap();
        }

        let result = run_scoped_manual_stop(
            &sessions,
            ManualStopScope::Account(owner.clone()),
            |_port| async { Err("Discord target disconnected".to_string()) },
        )
        .await;

        assert!(result.is_err());
        assert_eq!(sessions.lock().await.active().unwrap().app_name, "Retry Me");
    }

    // P1-A: the cleanup-wait bound surfaces a clear, actionable message if it is
    // ever reached. (`test-util`/paused time is unavailable here, so the actual
    // timeout branch is covered by the message contract rather than a 90s wait.)
    #[test]
    fn manual_stop_timeout_errors_name_the_account_and_are_retryable() {
        let owner = alice();
        let start_error = manual_start_stop_timeout_error(&owner);
        assert!(start_error.contains("Timed out"));
        assert!(start_error.contains(owner.as_str()));
        assert!(start_error.contains("Try again"));

        let cleanup_error = manual_cleanup_timeout_error(&owner);
        assert!(cleanup_error.contains("Timed out"));
        assert!(cleanup_error.contains(owner.as_str()));
        assert!(cleanup_error.contains("Try again"));
    }
}

#[cfg(test)]
mod app_exit_cleanup_state_tests {
    use super::AppExitCleanupState;

    #[test]
    fn failed_active_work_cleanup_leaves_exit_retryable() {
        let state = AppExitCleanupState::new();
        assert!(state.claim_local_cleanup());
        assert!(!state.claim_local_cleanup());
        assert!(!state.is_prepared());
    }

    #[test]
    fn successful_exit_preparation_skips_later_attempts() {
        let state = AppExitCleanupState::new();
        assert!(state.claim_local_cleanup());
        state.mark_prepared();
        assert!(state.is_prepared());
    }
}

async fn capture_cdp_session_with_progress<T, E, Fut, P>(
    capture: Fut,
    mut report: P,
) -> Result<T, E>
where
    Fut: std::future::Future<Output = Result<T, E>>,
    P: FnMut(AuthProgress),
{
    report(AuthProgress::phase(AuthProgressPhase::CapturingCdpSession));
    capture.await
}

#[cfg(test)]
mod auth_progress_tests {
    use super::{capture_cdp_session_with_progress, captured_cdp_user_validation_error};
    use crate::models::{AuthProgress, AuthProgressPhase};
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn failed_cdp_capture_stops_after_the_capture_phase() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let result: Result<(), &str> =
            capture_cdp_session_with_progress(async { Err("capture failed") }, move |progress| {
                captured.lock().unwrap().push(progress)
            })
            .await;

        assert_eq!(result, Err("capture failed"));
        assert_eq!(
            *events.lock().unwrap(),
            vec![AuthProgress::phase(AuthProgressPhase::CapturingCdpSession)]
        );
    }

    #[test]
    fn cdp_validation_error_mapping_preserves_status_without_upstream_body() {
        let response_body =
            "upstream error echoed Authorization: mfa.token-shaped-secret-abcdef012345";
        let status_only =
            crate::discord_api::current_user_http_error(reqwest::StatusCode::BAD_GATEWAY);
        let surfaced_error = captured_cdp_user_validation_error(status_only);

        assert!(surfaced_error.contains("HTTP 502"));
        assert!(!surfaced_error.contains("token-shaped-secret"));
        assert!(!surfaced_error.contains(response_body));
    }
}

/// A validated capture that is still entirely Rust-owned. `token` is consumed
/// only by a backend publication request and is never serialized or returned.
struct ValidatedCdpLogin {
    id: AccountId,
    user: DiscordUser,
    token: String,
    identity: SuperPropertiesHandle,
}

/// Shared CDP capture, identity extraction, and `/users/@me` validation for
/// legacy sign-in, preview, Add confirmation, and reconnect. The authorization
/// value remains Rust-only and neither preview nor commit returns it over IPC.
fn captured_cdp_user_validation_error(error: impl std::fmt::Display) -> String {
    format!("Captured Discord session is not valid: {error}")
}

async fn capture_and_validate_cdp_login<P>(
    state: &State<'_, AppState>,
    cdp_port: u16,
    mut report_progress: P,
) -> Result<ValidatedCdpLogin, String>
where
    P: FnMut(AuthProgress),
{
    let session = capture_cdp_session_with_progress(
        cdp_client::capture_discord_auth_via_cdp(cdp_port, std::time::Duration::from_secs(20)),
        &mut report_progress,
    )
    .await
    .map_err(|error| error.to_string())?;

    report_progress(AuthProgress::phase(AuthProgressPhase::ValidatingCdpSession));

    // Derive this identity solely from the captured session. Fall back to a
    // fresh CDP read only when its request did not carry x-super-properties.
    let identity = identity_from_cdp_session(&session);
    if identity.get_mode() != super_properties::SourceMode::Cdp {
        if let Ok(cdp_result) = cdp_client::fetch_super_properties_via_cdp(cdp_port).await {
            identity.set_from_cdp(&cdp_result.base64, &cdp_result.decoded);
        }
    }

    // Validate the captured authorization against Discord before either command
    // makes a publication decision. Proxy/keyring resolution remains off the
    // async executor.
    let proxy = resolve_proxy_configuration(state).await?;
    let client = DiscordApiClient::new_with_super_properties(
        session.authorization.to_string(),
        proxy,
        identity.clone(),
    )
    .map_err(|error| format!("Failed to create API client: {error}"))?;
    let user = client
        .get_current_user()
        .await
        .map_err(captured_cdp_user_validation_error)?;

    report_progress(AuthProgress::phase(AuthProgressPhase::PreparingSession));
    let id = AccountId::from_user(&user).map_err(|error| error.to_string())?;

    Ok(ValidatedCdpLogin {
        id,
        user,
        token: session.authorization.to_string(),
        identity,
    })
}

impl ValidatedCdpLogin {
    fn into_publish_request(self, cdp_port: u16) -> PublishAccountRequest {
        PublishAccountRequest {
            id: self.id,
            user: self.user,
            cdp_port: Some(cdp_port),
            used_at_ms: now_unix_ms(),
            token: self.token,
            identity: self.identity,
        }
    }
}

/// The read-only preview projection used by the live command and offline tests.
fn cdp_identity_preview_from_validated(
    validated: ValidatedCdpLogin,
    cdp_port: u16,
) -> CdpIdentityPreviewDto {
    CdpIdentityPreviewDto {
        port: cdp_port,
        user: validated.user,
    }
}

fn identity_changed_result(validated: ValidatedCdpLogin, cdp_port: u16) -> CdpConfirmResultDto {
    CdpConfirmResultDto {
        status: CdpConfirmStatus::IdentityChanged,
        user: validated.user,
        port: cdp_port,
    }
}

/// Expected-ID verification plus Add's known-ID/publish decision. The known
/// check and optional publication are one coordination-gate transaction.
fn coordinated_confirm_add_cdp_account(
    registry: &AccountRegistry,
    runtime: &ProxyRuntime,
    resources: &ResourceCoordinator,
    expected_id: &AccountId,
    cdp_port: u16,
    validated: ValidatedCdpLogin,
) -> Result<CdpConfirmResultDto, String> {
    if &validated.id != expected_id {
        return Ok(identity_changed_result(validated, cdp_port));
    }

    let gate = registry.coordination_gate();
    let _gate = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if registry.runtime(expected_id).is_some() {
        return Ok(CdpConfirmResultDto {
            status: CdpConfirmStatus::AlreadySaved,
            user: validated.user,
            port: cdp_port,
        });
    }

    let user = validated.user.clone();
    publish_account_under_gate(
        registry,
        runtime,
        resources,
        validated.into_publish_request(cdp_port),
    )?;
    Ok(CdpConfirmResultDto {
        status: CdpConfirmStatus::Added,
        user,
        port: cdp_port,
    })
}

/// Check that reconnect targets a saved account without holding the gate across
/// any subsequent CDP capture await.
fn require_known_reconnect_account(
    registry: &AccountRegistry,
    account_id: &AccountId,
) -> Result<(), String> {
    let gate = registry.coordination_gate();
    let _gate = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if registry.runtime(account_id).is_some() {
        Ok(())
    } else {
        Err("Unknown account.".to_string())
    }
}

/// Reconnect's identity verification and publication transaction. A live
/// mismatch returns before the gate or any profile/client/active mutation. A
/// matching account is checked again under the gate in case it was removed
/// while capture was in flight.
fn coordinated_reconnect_cdp_account(
    registry: &AccountRegistry,
    runtime: &ProxyRuntime,
    resources: &ResourceCoordinator,
    expected_id: &AccountId,
    cdp_port: u16,
    validated: ValidatedCdpLogin,
) -> Result<CdpConfirmResultDto, String> {
    if &validated.id != expected_id {
        return Ok(identity_changed_result(validated, cdp_port));
    }

    let gate = registry.coordination_gate();
    let _gate = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if registry.runtime(expected_id).is_none() {
        return Err("Unknown account.".to_string());
    }

    let user = validated.user.clone();
    publish_account_under_gate(
        registry,
        runtime,
        resources,
        validated.into_publish_request(cdp_port),
    )?;
    Ok(CdpConfirmResultDto {
        status: CdpConfirmStatus::Reconnected,
        user,
        port: cdp_port,
    })
}

/// Identity changes are terminal but not a completed commit; do not emit the
/// frontend's Complete progress phase for that outcome.
fn report_cdp_confirm_completion<P>(status: CdpConfirmStatus, mut report: P)
where
    P: FnMut(AuthProgress),
{
    if status != CdpConfirmStatus::IdentityChanged {
        report(AuthProgress::phase(AuthProgressPhase::Complete));
    }
}

/// CDP auto-login: capture the currently logged-in Discord session over CDP and
/// establish a DQH login from it. This is the primary login path on Linux.
///
/// The raw token is captured, validated, and stored **entirely on the Rust
/// side** — only the resolved `DiscordUser` is returned to the frontend. A
/// running client has exactly one current account, so the raw token is never
/// handed to the WebView. Requires Discord to be running with CDP enabled.
/// Works on every platform; on Linux it is the primary login path.
#[tauri::command]
async fn auto_login_via_cdp(
    port: Option<u16>,
    state: State<'_, AppState>,
    on_progress: Channel<AuthProgress>,
) -> Result<DiscordUser, String> {
    use crate::logger::{log, LogCategory, LogLevel};

    let cdp_port = port.unwrap_or(cdp_client::DEFAULT_CDP_PORT);

    log(
        LogLevel::Info,
        LogCategory::TokenExtraction,
        &format!("Starting CDP auto-login on port {}", cdp_port),
        None,
    );

    // Keep a direct lease for capture -> identity -> user lookup -> publication.
    // Once publication is dispatched, the blocking worker owns the lease so an
    // aborted command waiter cannot release the port while it is still writing.
    let (validated, login_lease) =
        capture_with_direct_cdp_lease(state.leases.as_ref(), cdp_port, || {
            capture_and_validate_cdp_login(&state, cdp_port, |progress| {
                let _ = on_progress.send(progress);
            })
        })
        .await?;

    // Re-resolve policy in coordinated publication so a proxy setting changed
    // during CDP/network work is honored rather than overwritten by a stale build.
    let registry = state.accounts.clone();
    let runtime = state.proxy.clone();
    let resources = state.resources.clone();
    let request = PublishAccountRequest {
        id: validated.id,
        user: validated.user.clone(),
        cdp_port: Some(cdp_port),
        used_at_ms: now_unix_ms(),
        token: validated.token,
        identity: validated.identity,
    };
    let (publish_result, _login_lease) = spawn_blocking_with_lease(login_lease, move || {
        coordinated_publish_account(&registry, &runtime, resources.as_ref(), request)
    })
    .await
    .map_err(|error| format!("Login publish task failed: {error}"))?;
    publish_result?;

    log(
        LogLevel::Info,
        LogCategory::TokenExtraction,
        "CDP auto-login succeeded",
        None,
    );

    let _ = on_progress.send(AuthProgress::phase(AuthProgressPhase::Complete));

    Ok(validated.user)
}

/// Add a captured Discord account without disturbing any account already known
/// by the registry. Duplicate detection and publication are atomic with respect
/// to activation/removal and other Add requests.
#[tauri::command]
async fn auto_add_account_via_cdp(
    port: Option<u16>,
    state: State<'_, AppState>,
    on_progress: Channel<AuthProgress>,
) -> Result<AddCdpResultDto, String> {
    let cdp_port = port.unwrap_or(cdp_client::DEFAULT_CDP_PORT);
    let (validated, login_lease) =
        capture_with_direct_cdp_lease(state.leases.as_ref(), cdp_port, || {
            capture_and_validate_cdp_login(&state, cdp_port, |progress| {
                let _ = on_progress.send(progress);
            })
        })
        .await?;
    let registry = state.accounts.clone();
    let runtime = state.proxy.clone();
    let resources = state.resources.clone();
    let request = PublishAccountRequest {
        id: validated.id,
        user: validated.user,
        cdp_port: Some(cdp_port),
        used_at_ms: now_unix_ms(),
        token: validated.token,
        identity: validated.identity,
    };

    let (add_result, _login_lease) = spawn_blocking_with_lease(login_lease, move || {
        coordinated_add_account(&registry, &runtime, resources.as_ref(), request)
    })
    .await
    .map_err(|error| format!("Add account publish task failed: {error}"))?;
    let result = add_result?;

    let _ = on_progress.send(AuthProgress::phase(AuthProgressPhase::Complete));
    Ok(result)
}

/// Preview the identity currently open in one selected CDP client. This command
/// is read-only and returns only the verified Discord user presentation.
#[tauri::command]
async fn preview_cdp_identity(
    port: u16,
    state: State<'_, AppState>,
) -> Result<CdpIdentityPreviewDto, String> {
    let (validated, _lease) = capture_with_direct_cdp_lease(state.leases.as_ref(), port, || {
        capture_and_validate_cdp_login(&state, port, |_| {})
    })
    .await?;
    Ok(cdp_identity_preview_from_validated(validated, port))
}

/// Confirm a previously previewed Add identity by capturing it again. The
/// expected ID, known-account check, and optional publication are enforced by
/// the backend; a stale preview cannot publish a different user.
#[tauri::command]
async fn confirm_add_cdp_account(
    port: u16,
    expected_user_id: String,
    state: State<'_, AppState>,
    on_progress: Channel<AuthProgress>,
) -> Result<CdpConfirmResultDto, String> {
    let expected_id = AccountId::parse(&expected_user_id).map_err(|error| error.to_string())?;
    let (validated, lease) = capture_with_direct_cdp_lease(state.leases.as_ref(), port, || {
        capture_and_validate_cdp_login(&state, port, |progress| {
            let _ = on_progress.send(progress);
        })
    })
    .await?;

    let registry = state.accounts.clone();
    let runtime = state.proxy.clone();
    let resources = state.resources.clone();
    let (result, _lease) = spawn_blocking_with_lease(lease, move || {
        coordinated_confirm_add_cdp_account(
            &registry,
            &runtime,
            resources.as_ref(),
            &expected_id,
            port,
            validated,
        )
    })
    .await
    .map_err(|error| format!("Add confirmation task failed: {error}"))?;
    let result = result?;
    report_cdp_confirm_completion(result.status, |progress| {
        let _ = on_progress.send(progress);
    });
    Ok(result)
}

/// Reauthenticate only the requested, already-saved account. No activation or
/// client publication occurs unless the selected live CDP identity matches it.
#[tauri::command]
async fn reconnect_cdp_account(
    account_id: String,
    port: u16,
    state: State<'_, AppState>,
    on_progress: Channel<AuthProgress>,
) -> Result<CdpConfirmResultDto, String> {
    let expected_id = AccountId::parse(&account_id).map_err(|error| error.to_string())?;
    require_known_reconnect_account(state.accounts.as_ref(), &expected_id)?;

    let (validated, lease) = capture_with_direct_cdp_lease(state.leases.as_ref(), port, || {
        capture_and_validate_cdp_login(&state, port, |progress| {
            let _ = on_progress.send(progress);
        })
    })
    .await?;

    let registry = state.accounts.clone();
    let runtime = state.proxy.clone();
    let resources = state.resources.clone();
    let (result, _lease) = spawn_blocking_with_lease(lease, move || {
        coordinated_reconnect_cdp_account(
            &registry,
            &runtime,
            resources.as_ref(),
            &expected_id,
            port,
            validated,
        )
    })
    .await
    .map_err(|error| format!("Reconnect task failed: {error}"))?;
    let result = result?;
    report_cdp_confirm_completion(result.status, |progress| {
        let _ = on_progress.send(progress);
    });
    Ok(result)
}

/// Refuse CDP mutations when Helper's authenticated account differs from the
/// account currently open in the selected desktop client. Without this guard,
/// injection can affect account B while progress polling still targets A.
///
/// `session` is the coherent online account snapshot taken before any await. It
/// must be passed in rather than re-read from the active account afterward,
/// otherwise an account switch during CDP capture would compare the desktop
/// client against the wrong account. This function verifies only; the lease
/// transaction is owned by `CdpPortLeases::acquire_account`.
async fn verify_cdp_account_consistency(
    state: &State<'_, AppState>,
    session: &OnlineAccountSession,
    cdp_port: u16,
) -> Result<(), String> {
    let captured = cdp_client::capture_discord_auth_via_cdp(
        cdp_port,
        std::time::Duration::from_secs(8),
    )
    .await
    .map_err(|error| {
        format!(
            "Could not verify the account open in the desktop client on CDP port {cdp_port}: {error}"
        )
    })?;
    // Build the validation client from this captured session's identity and a
    // policy snapshot taken under the coordination gate. It is not published, so
    // the account-read network call runs outside the gate.
    let registry = state.accounts.clone();
    let runtime = state.proxy.clone();
    let account_id = session.account_id().clone();
    let token = captured.authorization.to_string();
    let identity = identity_from_cdp_session(&captured);
    let client = tokio::task::spawn_blocking(move || {
        coordinated_build_client(&registry, &runtime, &account_id, token, identity)
    })
    .await
    .map_err(|error| format!("Account consistency task failed: {error}"))??;
    let actual = client
        .get_current_user()
        .await
        .map_err(|error| format!("Could not read the desktop client account: {error}"))?;
    if actual.id == session.user().id {
        // Verification only: the lease transaction (Verifying -> Active) is owned
        // by `CdpPortLeases::acquire_account`, so a mismatch can never be silently
        // discarded and this function never claims after an await.
        return Ok(());
    }

    let owner = match discord_cdp_launch_core::inspect_cdp_port_owner(cdp_port) {
        discord_cdp_launch_core::CdpPortOwner::Official => "Discord",
        discord_cdp_launch_core::CdpPortOwner::Vesktop => "Vesktop",
        discord_cdp_launch_core::CdpPortOwner::None => "the selected desktop client",
        discord_cdp_launch_core::CdpPortOwner::Other => "an unrecognized desktop client",
    };
    let expected = session.user();
    let expected_name = expected
        .global_name
        .as_deref()
        .unwrap_or(&expected.username);
    let actual_name = actual.global_name.as_deref().unwrap_or(&actual.username);
    Err(format!(
        "account_mismatch: Helper is signed in as {expected_name} ({}), but {owner} is signed in as {actual_name} ({}). Sign both into the same account before starting a CDP task.",
        session.account_id().as_str(),
        actual.id
    ))
}

/// Get quest list (via HTTP API /quests/@me endpoint)
#[tauri::command]
async fn get_quests(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let client = active_client(&state)?;

    let quests = client
        .get_quests_raw()
        .await
        .map_err(|e| format!("Failed to get quest list: {}", e))?;

    // Return the "quests" array directly
    Ok(quests
        .get("quests")
        .cloned()
        .unwrap_or(serde_json::Value::Array(vec![])))
}

/// Get full quest list response, preserving excluded quests and enrollment block status.
#[tauri::command]
async fn get_quests_full(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let client = active_client(&state)?;

    client
        .get_quests_raw()
        .await
        .map_err(|e| format!("Failed to get quest list: {}", e))
}

/// Shared setup for video quest starts. `preempt` selects the legacy
/// stop-before-start behavior versus the non-preemptive run API.
#[allow(clippy::too_many_arguments)]
async fn start_video_quest_impl(
    quest_id: String,
    seconds_needed: u32,
    initial_progress: f64,
    speed_multiplier: f64,
    heartbeat_interval: u64,
    preempt: bool,
    state: &State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<QuestRunDto, String> {
    let context = start_context_requiring_client(state)?;

    // Preserve today's replacement UX only for the legacy wrapper.
    if preempt {
        stop_account_work_internal(state, &context.account_id).await?;
    }

    let account_id = context.account_id.clone();
    let client = context.client.expect("REST start validated a client");
    let kind = QuestKind::Video;
    let transport = QuestTransport::Rest;
    let worker_handle = app_handle.clone();
    let worker_quest_id = quest_id.clone();
    let worker_account = account_id.clone();
    admit_quest_run(
        state,
        account_id,
        app_handle,
        quest_id,
        kind,
        transport,
        Box::new(move |guards, cancel_watch, progress, run_id| {
            let app_handle = worker_handle;
            let quest_id = worker_quest_id;
            let account_id = worker_account;
            Box::pin(async move {
                let _guards = guards;
                let cancelled = cancel_watch.clone();
                let cancel_rx = bridge_cancel(cancel_watch);
                let emitter =
                    QuestEventSink::new(app_handle, progress, account_id, quest_id.clone(), run_id);
                let result = quest_completer::complete_video_quest(
                    &client,
                    quest_id,
                    seconds_needed,
                    initial_progress,
                    speed_multiplier,
                    heartbeat_interval,
                    emitter,
                    cancel_rx,
                )
                .await;
                worker_outcome(cancelled, result)
            })
        }),
    )
    .await
}

/// Start video quest
#[tauri::command]
async fn start_video_quest(
    quest_id: String,
    seconds_needed: u32,
    initial_progress: f64,
    speed_multiplier: f64,
    heartbeat_interval: u64,
    state: State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    start_video_quest_impl(
        quest_id,
        seconds_needed,
        initial_progress,
        speed_multiplier,
        heartbeat_interval,
        true,
        &state,
        app_handle,
    )
    .await
    .map(|_| ())
}

/// Start a video quest run without stopping existing runs.
#[tauri::command]
async fn start_video_quest_run(
    quest_id: String,
    seconds_needed: u32,
    initial_progress: f64,
    speed_multiplier: f64,
    heartbeat_interval: u64,
    state: State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<QuestRunDto, String> {
    start_video_quest_impl(
        quest_id,
        seconds_needed,
        initial_progress,
        speed_multiplier,
        heartbeat_interval,
        false,
        &state,
        app_handle,
    )
    .await
}

/// Shared setup for stream quest starts.
async fn start_stream_quest_impl(
    quest_id: String,
    stream_key: String,
    seconds_needed: u32,
    initial_progress: f64,
    preempt: bool,
    state: &State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<QuestRunDto, String> {
    let context = start_context_requiring_client(state)?;

    if preempt {
        stop_account_work_internal(state, &context.account_id).await?;
    }

    let account_id = context.account_id.clone();
    let client = context.client.expect("REST start validated a client");
    let kind = QuestKind::Stream;
    let transport = QuestTransport::Rest;
    let worker_handle = app_handle.clone();
    let worker_quest_id = quest_id.clone();
    let worker_account = account_id.clone();
    admit_quest_run(
        state,
        account_id,
        app_handle,
        quest_id,
        kind,
        transport,
        Box::new(move |guards, cancel_watch, progress, run_id| {
            let app_handle = worker_handle;
            let quest_id = worker_quest_id;
            let account_id = worker_account;
            Box::pin(async move {
                let _guards = guards;
                let cancelled = cancel_watch.clone();
                let cancel_rx = bridge_cancel(cancel_watch);
                let emitter =
                    QuestEventSink::new(app_handle, progress, account_id, quest_id.clone(), run_id);
                let result = quest_completer::complete_stream_quest(
                    &client,
                    quest_id,
                    stream_key,
                    seconds_needed,
                    initial_progress,
                    emitter,
                    cancel_rx,
                )
                .await;
                worker_outcome(cancelled, result)
            })
        }),
    )
    .await
}

/// Start stream quest
#[tauri::command]
async fn start_stream_quest(
    quest_id: String,
    stream_key: String,
    seconds_needed: u32,
    initial_progress: f64,
    state: State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    start_stream_quest_impl(
        quest_id,
        stream_key,
        seconds_needed,
        initial_progress,
        true,
        &state,
        app_handle,
    )
    .await
    .map(|_| ())
}

/// Start a stream quest run without stopping existing runs.
#[tauri::command]
async fn start_stream_quest_run(
    quest_id: String,
    stream_key: String,
    seconds_needed: u32,
    initial_progress: f64,
    state: State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<QuestRunDto, String> {
    start_stream_quest_impl(
        quest_id,
        stream_key,
        seconds_needed,
        initial_progress,
        false,
        &state,
        app_handle,
    )
    .await
}

/// Shared setup for game-heartbeat quest starts.
async fn start_game_heartbeat_quest_impl(
    quest_id: String,
    application_id: String,
    seconds_needed: u32,
    initial_progress: f64,
    preempt: bool,
    state: &State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<QuestRunDto, String> {
    let context = start_context_requiring_client(state)?;

    if preempt {
        stop_account_work_internal(state, &context.account_id).await?;
    }

    let account_id = context.account_id.clone();
    let client = context.client.expect("REST start validated a client");
    let kind = QuestKind::Game;
    let transport = QuestTransport::Rest;
    let worker_handle = app_handle.clone();
    let worker_quest_id = quest_id.clone();
    let worker_account = account_id.clone();
    admit_quest_run(
        state,
        account_id,
        app_handle,
        quest_id,
        kind,
        transport,
        Box::new(move |guards, cancel_watch, progress, run_id| {
            let app_handle = worker_handle;
            let quest_id = worker_quest_id;
            let account_id = worker_account;
            Box::pin(async move {
                let _guards = guards;
                let cancelled = cancel_watch.clone();
                let cancel_rx = bridge_cancel(cancel_watch);
                let emitter =
                    QuestEventSink::new(app_handle, progress, account_id, quest_id.clone(), run_id);
                let result = quest_completer::complete_game_quest_via_heartbeat(
                    &client,
                    quest_id,
                    application_id,
                    seconds_needed,
                    initial_progress,
                    emitter,
                    cancel_rx,
                )
                .await;
                worker_outcome(cancelled, result)
            })
        }),
    )
    .await
}

/// Start game quest via direct heartbeat (without running simulated game)
#[tauri::command]
async fn start_game_heartbeat_quest(
    quest_id: String,
    application_id: String,
    seconds_needed: u32,
    initial_progress: f64,
    state: State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    start_game_heartbeat_quest_impl(
        quest_id,
        application_id,
        seconds_needed,
        initial_progress,
        true,
        &state,
        app_handle,
    )
    .await
    .map(|_| ())
}

/// Start a game-heartbeat quest run without stopping existing runs.
#[tauri::command]
async fn start_game_heartbeat_quest_run(
    quest_id: String,
    application_id: String,
    seconds_needed: u32,
    initial_progress: f64,
    state: State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<QuestRunDto, String> {
    start_game_heartbeat_quest_impl(
        quest_id,
        application_id,
        seconds_needed,
        initial_progress,
        false,
        &state,
        app_handle,
    )
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlayActivityTransport {
    DirectApi,
    Cdp,
}

impl TryFrom<&str> for PlayActivityTransport {
    type Error = String;

    fn try_from(mode: &str) -> Result<Self, Self::Error> {
        match mode {
            "simulate" | "heartbeat" => Ok(Self::DirectApi),
            "cdp" => Ok(Self::Cdp),
            _ => Err(format!("Unsupported PLAY_ACTIVITY mode: {}", mode)),
        }
    }
}

#[cfg(test)]
mod play_activity_transport_tests {
    use super::PlayActivityTransport;

    #[test]
    fn maps_supported_frontend_modes_to_a_transport() {
        assert_eq!(
            PlayActivityTransport::try_from("simulate"),
            Ok(PlayActivityTransport::DirectApi)
        );
        assert_eq!(
            PlayActivityTransport::try_from("heartbeat"),
            Ok(PlayActivityTransport::DirectApi)
        );
        assert_eq!(
            PlayActivityTransport::try_from("cdp"),
            Ok(PlayActivityTransport::Cdp)
        );
        assert!(PlayActivityTransport::try_from("unknown").is_err());
    }
}

#[cfg(test)]
mod quest_start_outcome_tests {
    use super::{run_outcome, worker_outcome};
    use crate::quest_runtime::QuestOutcome;

    #[test]
    fn cancelled_result_is_stopped_even_when_the_loop_reported_success() {
        assert_eq!(run_outcome(true, Ok(())), QuestOutcome::Stopped);
        assert_eq!(
            run_outcome(true, Err(anyhow::anyhow!("rollback failed"))),
            QuestOutcome::Stopped
        );
    }

    #[test]
    fn uncancelled_result_maps_success_and_failure() {
        assert_eq!(run_outcome(false, Ok(())), QuestOutcome::Completed);
        match run_outcome(false, Err(anyhow::anyhow!("boom"))) {
            QuestOutcome::Failed(message) => assert_eq!(message, "boom"),
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn worker_outcome_reads_the_final_cancel_state() {
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        assert_eq!(
            worker_outcome(cancel_rx.clone(), Ok(())),
            QuestOutcome::Completed
        );
        let _ = cancel_tx.send(true);
        assert_eq!(worker_outcome(cancel_rx, Ok(())), QuestOutcome::Stopped);
    }
}

/// Shared setup for PLAY_ACTIVITY quest starts.
#[allow(clippy::too_many_arguments)]
async fn start_play_activity_quest_impl(
    quest_id: String,
    application_id: String,
    seconds_needed: u32,
    initial_progress: f64,
    mode: String,
    cdp_port: u16,
    heartbeat_interval: u64,
    progress_polling_interval: u64,
    preempt: bool,
    state: &State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<QuestRunDto, String> {
    let transport = PlayActivityTransport::try_from(mode.as_str())?;
    if heartbeat_interval == 0 {
        return Err("PLAY_ACTIVITY heartbeat interval must be greater than zero".to_string());
    }
    if progress_polling_interval == 0 {
        return Err(
            "PLAY_ACTIVITY progress polling interval must be greater than zero".to_string(),
        );
    }

    // CDP mode requires a coherent online session (user AND client) and acquires
    // a verified account lease before legacy preemption or admission; DirectApi
    // only needs its REST client. Both are snapshotted once, before any await.
    let (account_id, client, cdp_lease) = if transport == PlayActivityTransport::Cdp {
        let session = cdp_online_session(state)?;
        let lease = acquire_account_lease(state, &session, cdp_port, preempt).await?;
        (
            session.account_id().clone(),
            Some(session.client().clone()),
            Some(lease),
        )
    } else {
        let context = start_context_requiring_client(state)?;
        if preempt {
            stop_account_work_internal(state, &context.account_id).await?;
        }
        (context.account_id.clone(), context.client, None)
    };

    let kind = QuestKind::PlayActivity;
    let quest_transport = match transport {
        PlayActivityTransport::Cdp => QuestTransport::Cdp { port: cdp_port },
        PlayActivityTransport::DirectApi => QuestTransport::Rest,
    };
    let worker_handle = app_handle.clone();
    let worker_quest_id = quest_id.clone();
    let worker_account = account_id.clone();
    // CDP mode holds the account lease for the whole worker via the shared
    // wrapper; DirectApi holds no lease.
    let worker: QuestWorkerFactory = match cdp_lease {
        Some(lease) => cdp_worker_factory(lease, move |cancel_watch, progress, run_id| {
            run_play_activity_worker(
                transport,
                cdp_port,
                worker_account,
                run_id,
                worker_quest_id,
                application_id,
                seconds_needed,
                initial_progress,
                heartbeat_interval,
                progress_polling_interval,
                client,
                worker_handle,
                cancel_watch,
                progress,
            )
        }),
        None => Box::new(move |guards, cancel_watch, progress, run_id| {
            Box::pin(async move {
                let _guards = guards;
                run_play_activity_worker(
                    transport,
                    cdp_port,
                    worker_account,
                    run_id,
                    worker_quest_id,
                    application_id,
                    seconds_needed,
                    initial_progress,
                    heartbeat_interval,
                    progress_polling_interval,
                    client,
                    worker_handle,
                    cancel_watch,
                    progress,
                )
                .await
            })
        }),
    };
    admit_quest_run(
        state,
        account_id,
        app_handle,
        quest_id,
        kind,
        quest_transport,
        worker,
    )
    .await
}

/// Start a PLAY_ACTIVITY cloud-game quest using the current game quest mode.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn start_play_activity_quest(
    quest_id: String,
    application_id: String,
    seconds_needed: u32,
    initial_progress: f64,
    mode: String,
    cdp_port: u16,
    heartbeat_interval: u64,
    progress_polling_interval: u64,
    state: State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    start_play_activity_quest_impl(
        quest_id,
        application_id,
        seconds_needed,
        initial_progress,
        mode,
        cdp_port,
        heartbeat_interval,
        progress_polling_interval,
        true,
        &state,
        app_handle,
    )
    .await
    .map(|_| ())
}

/// Start a PLAY_ACTIVITY run without stopping existing runs.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn start_play_activity_quest_run(
    quest_id: String,
    application_id: String,
    seconds_needed: u32,
    initial_progress: f64,
    mode: String,
    cdp_port: u16,
    heartbeat_interval: u64,
    progress_polling_interval: u64,
    state: State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<QuestRunDto, String> {
    start_play_activity_quest_impl(
        quest_id,
        application_id,
        seconds_needed,
        initial_progress,
        mode,
        cdp_port,
        heartbeat_interval,
        progress_polling_interval,
        false,
        &state,
        app_handle,
    )
    .await
}

/// Shared setup for CDP quest starts.
///
/// Dispatches to the appropriate CDP completion function based on quest_type.
#[allow(clippy::too_many_arguments)]
async fn start_cdp_quest_impl(
    quest_id: String,
    quest_type: String,
    application_id: String,
    application_name: String,
    seconds_needed: u32,
    initial_progress: f64,
    cdp_port: u16,
    checkpoint_times: Option<Vec<u32>>,
    preempt: bool,
    state: &State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<QuestRunDto, String> {
    let kind = match quest_type.as_str() {
        "play" => QuestKind::Game,
        "stream" => QuestKind::Stream,
        "video" => QuestKind::Video,
        "activity" => QuestKind::EmbeddedActivity,
        other => return Err(format!("Unknown CDP quest type: {other}")),
    };

    // Fail-closed: a coherent online session (user AND client) is required
    // before any side effect. A user-only/tokenless or restored profile returns
    // the normal not-authenticated error here. The verified account lease is
    // acquired before legacy preemption or admission and moved into the worker.
    let session = cdp_online_session(state)?;
    let cdp_lease = acquire_account_lease(state, &session, cdp_port, preempt).await?;

    let quest_transport = QuestTransport::Cdp { port: cdp_port };
    // Clone the API client for progress polling (play/stream quests)
    let account_id = session.account_id().clone();
    let client = Some(session.client().clone());
    let worker_quest_id = quest_id.clone();
    let worker_quest_type = quest_type.clone();
    let worker_handle = app_handle.clone();
    let worker_account = account_id.clone();

    admit_quest_run(
        state,
        account_id,
        app_handle,
        quest_id,
        kind,
        quest_transport,
        cdp_worker_factory(
            cdp_lease,
            move |cancel_watch, progress, run_id| async move {
                let cancelled = cancel_watch.clone();
                let cancel_rx = bridge_cancel(cancel_watch);
                let emitter = QuestEventSink::new(
                    worker_handle,
                    progress,
                    worker_account,
                    worker_quest_id.clone(),
                    run_id,
                );
                let result = match worker_quest_type.as_str() {
                    "play" => {
                        cdp_quest::complete_play_quest_via_cdp(
                            cdp_port,
                            worker_quest_id,
                            application_id,
                            application_name,
                            seconds_needed,
                            initial_progress,
                            client,
                            emitter,
                            cancel_rx,
                        )
                        .await
                    }
                    "stream" => {
                        cdp_quest::complete_stream_quest_via_cdp(
                            cdp_port,
                            worker_quest_id,
                            application_id,
                            seconds_needed,
                            initial_progress,
                            client,
                            emitter,
                            cancel_rx,
                        )
                        .await
                    }
                    "video" => {
                        cdp_quest::complete_video_quest_via_cdp(
                            cdp_port,
                            worker_quest_id,
                            seconds_needed,
                            initial_progress,
                            emitter,
                            cancel_rx,
                        )
                        .await
                    }
                    "activity" => {
                        let times = checkpoint_times
                            .filter(|v| !v.is_empty())
                            .unwrap_or_else(|| vec![180, 180, 180]);
                        cdp_quest::complete_activity_quest_via_cdp(
                            cdp_port,
                            worker_quest_id,
                            application_id,
                            initial_progress,
                            times,
                            client,
                            emitter,
                            cancel_rx,
                        )
                        .await
                    }
                    other => Err(anyhow::anyhow!("Unknown CDP quest type: {other}")),
                };
                worker_outcome(cancelled, result)
            },
        ),
    )
    .await
}

/// Start a quest via CDP injection
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn start_cdp_quest(
    quest_id: String,
    quest_type: String,
    application_id: String,
    application_name: String,
    seconds_needed: u32,
    initial_progress: f64,
    cdp_port: u16,
    checkpoint_times: Option<Vec<u32>>,
    state: State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    start_cdp_quest_impl(
        quest_id,
        quest_type,
        application_id,
        application_name,
        seconds_needed,
        initial_progress,
        cdp_port,
        checkpoint_times,
        true,
        &state,
        app_handle,
    )
    .await
    .map(|_| ())
}

/// Start a CDP quest run without stopping existing runs.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn start_cdp_quest_run(
    quest_id: String,
    quest_type: String,
    application_id: String,
    application_name: String,
    seconds_needed: u32,
    initial_progress: f64,
    cdp_port: u16,
    checkpoint_times: Option<Vec<u32>>,
    state: State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<QuestRunDto, String> {
    start_cdp_quest_impl(
        quest_id,
        quest_type,
        application_id,
        application_name,
        seconds_needed,
        initial_progress,
        cdp_port,
        checkpoint_times,
        false,
        &state,
        app_handle,
    )
    .await
}

/// Stop the account's active quest run(s) and wait. Preserves the legacy
/// no-argument contract used by the sequential frontend.
#[tauri::command]
async fn stop_quest(state: State<'_, AppState>) -> Result<(), String> {
    stop_active_work_internal(&state).await
}

/// List the active account's live quest runs for the frontend.
///
/// KNOWN LIMIT (Phase 6.2): account-scoped by design. After an account switch a
/// previous account's still-live run is not returned here, because the frontend
/// and DTO contract key runs by quest id and cross-account listing would collide
/// identical quest ids. That run stays reachable through the process-global
/// `stop_all_quests` command and the app-exit cleanup; per-run targeting of a
/// previous account's run needs the Phase 6.4 account-scoped run API.
#[tauri::command]
async fn list_quest_runs(state: State<'_, AppState>) -> Result<Vec<QuestRunDto>, String> {
    let account_id = active_account_id(&state)?;
    Ok(state
        .quests
        .snapshot_for_account(&account_id)
        .iter()
        .map(|control| quest_run_dto(control))
        .collect())
}

/// Stop one run by quest id. A stale `run_id` is rejected rather than allowed to
/// stop a newer run; unknown ids report `alreadyFinished` idempotently; a wait
/// timeout leaves the run in `Stopping` and reports `stopTimeout`.
#[tauri::command]
async fn stop_quest_run(
    quest_id: String,
    run_id: Option<String>,
    state: State<'_, AppState>,
) -> Result<StopQuestResult, String> {
    let account_id = active_account_id(&state)?;
    let result = match state
        .quests
        .signal_stop(&account_id, &quest_id, run_id.as_deref())
    {
        StopSignal::NotFound => StopQuestResult {
            quest_id,
            run_id,
            status: "alreadyFinished".to_string(),
        },
        StopSignal::RunIdMismatch { .. } => StopQuestResult {
            quest_id,
            run_id,
            status: "runIdMismatch".to_string(),
        },
        StopSignal::Signalled(control) => {
            let status = match quest_runtime::wait_for_done(&control, QUEST_STOP_WAIT).await {
                DoneWait::Finished(_) => "stopped",
                DoneWait::TimedOut => "stopTimeout",
            };
            StopQuestResult {
                quest_id,
                run_id,
                status: status.to_string(),
            }
        }
    };
    Ok(result)
}

/// Stop every account's live quest runs (process-global).
///
/// KNOWN LIMIT (Phase 6.2): the per-run surface is account-scoped, so after an
/// account switch a previous account's still-live run is not listed by
/// `list_quest_runs` nor individually stoppable through `stop_quest_run`. This
/// process-global command (the frontend's "stop every run through the registry"
/// compatibility path) and the app-exit cleanup are the deliberate escapes that
/// still reach it. Kept process-global rather than account-scoped so that escape
/// remains until Phase 6.4 adds an explicit account-scoped run API.
#[tauri::command]
async fn stop_all_quests(state: State<'_, AppState>) -> Result<StopAllResult, String> {
    Ok(stop_all_quests_internal(&state).await)
}

/// List every live quest run across all accounts. Each DTO already carries its
/// `accountId`, so the frontend can key runs account-safely.
#[tauri::command]
async fn list_all_quest_runs(state: State<'_, AppState>) -> Result<Vec<QuestRunDto>, String> {
    Ok(state
        .quests
        .snapshot()
        .iter()
        .map(|control| quest_run_dto(control))
        .collect())
}

/// Stop one run by explicit `(account, quest)` (optionally pinned by `run_id`),
/// never routing through the current active account.
#[tauri::command]
async fn stop_account_quest_run(
    account_id: String,
    quest_id: String,
    run_id: Option<String>,
    state: State<'_, AppState>,
) -> Result<StopQuestResult, String> {
    let account_id = AccountId::parse(&account_id).map_err(|error| error.to_string())?;
    let result = match state
        .quests
        .signal_stop(&account_id, &quest_id, run_id.as_deref())
    {
        StopSignal::NotFound => StopQuestResult {
            quest_id,
            run_id,
            status: "alreadyFinished".to_string(),
        },
        StopSignal::RunIdMismatch { .. } => StopQuestResult {
            quest_id,
            run_id,
            status: "runIdMismatch".to_string(),
        },
        StopSignal::Signalled(control) => {
            let status = match quest_runtime::wait_for_done(&control, QUEST_STOP_WAIT).await {
                DoneWait::Finished(_) => "stopped",
                DoneWait::TimedOut => "stopTimeout",
            };
            StopQuestResult {
                quest_id,
                run_id,
                status: status.to_string(),
            }
        }
    };
    Ok(result)
}

/// Stop every run of one explicit account. Other accounts' runs are untouched.
#[tauri::command]
async fn stop_account_quests(
    account_id: String,
    state: State<'_, AppState>,
) -> Result<StopAllResult, String> {
    let account_id = AccountId::parse(&account_id).map_err(|error| error.to_string())?;
    Ok(stop_account_quests_internal(&state, &account_id).await)
}

/// List all known accounts plus the active account id. Secret-free.
#[tauri::command]
async fn list_accounts(state: State<'_, AppState>) -> Result<AccountsSnapshotDto, String> {
    Ok(accounts_snapshot(state.accounts.as_ref()))
}

/// Activate a saved account. An offline (persisted-only) profile can be
/// activated; it never rehydrates a token/client, so the returned DTO has
/// `isAuthenticated = false` and authenticated operations keep returning
/// `Not logged in`.
#[tauri::command]
async fn activate_account(
    account_id: String,
    state: State<'_, AppState>,
) -> Result<AccountSummaryDto, String> {
    let id = AccountId::parse(&account_id).map_err(|error| error.to_string())?;
    let registry = state.accounts.as_ref();
    let account = coordinated_activate_account(registry, &id)?;
    Ok(account_summary_dto(registry, account.profile()))
}

/// Activate an account only when its runtime has a coherent authenticated
/// user/client pair. Unlike `activate_account`, this command also persists the
/// active id before mutating in-memory registry state.
#[tauri::command]
async fn activate_online_account(
    account_id: String,
    state: State<'_, AppState>,
) -> Result<AccountSummaryDto, String> {
    let id = AccountId::parse(&account_id).map_err(|error| error.to_string())?;
    let registry = state.accounts.as_ref();
    let account = coordinated_activate_online_account(registry, &id)?;
    Ok(account_summary_dto(registry, account.profile()))
}

/// Remove one account and its profile. Stops ONLY that account's quests and
/// manual spoof to terminal, clears its account proxy override/credential, then
/// deletes the profile. If the removed account was active, no account is left
/// active. Never affects another account.
#[tauri::command]
async fn remove_account(
    account_id: String,
    state: State<'_, AppState>,
) -> Result<AccountsSnapshotDto, String> {
    let id = AccountId::parse(&account_id).map_err(|error| error.to_string())?;

    // Stop this account's runs to terminal; a timed-out run retains the account.
    let stopped = stop_account_quests_internal(&state, &id).await;
    if !stopped.timed_out.is_empty() || !stopped.cleanup_failed.is_empty() {
        return Err(
            "The account's quest runs did not stop cleanly; try again before removing it."
                .to_string(),
        );
    }
    stop_manual_cdp_game_simulation_scoped(&state, ManualStopScope::Account(id.clone())).await?;

    // Clear its account proxy override/credential with the 6.3B API.
    let registry = state.accounts.clone();
    let runtime = state.proxy.clone();
    let proxy_account = id.clone();
    tokio::task::spawn_blocking(move || {
        coordinated_clear_account_override(&registry, &runtime, proxy_account)
    })
    .await
    .map_err(|error| format!("Proxy settings task failed: {error}"))??;

    coordinated_remove_account(state.accounts.as_ref(), &id)?;
    Ok(accounts_snapshot(state.accounts.as_ref()))
}

async fn stop_all_quests_internal(state: &State<'_, AppState>) -> StopAllResult {
    let results = quest_runtime::stop_all_runs(state.quests.as_ref(), QUEST_STOP_WAIT).await;
    classify_stop_results(results)
}

/// Stop ONE explicit account's runs. A snapshotted start uses this so legacy
/// preemption can never stop whichever account became active after the snapshot.
async fn stop_account_quests_internal(
    state: &State<'_, AppState>,
    account_id: &AccountId,
) -> StopAllResult {
    stop_account_quests_core(state.quests.as_ref(), account_id, QUEST_STOP_WAIT).await
}

/// Pure account-scoped stop core, testable without a `tauri::State`.
async fn stop_account_quests_core(
    quests: &QuestRegistry,
    account_id: &AccountId,
    timeout: std::time::Duration,
) -> StopAllResult {
    let results = quest_runtime::stop_account_runs(quests, account_id, timeout).await;
    classify_stop_results(results)
}

fn classify_stop_results(results: Vec<(String, StopClass)>) -> StopAllResult {
    let mut result = StopAllResult::default();
    for (quest_id, class) in results {
        match class {
            StopClass::Completed => result.completed.push(quest_id),
            StopClass::TimedOut => result.timed_out.push(quest_id),
            StopClass::CleanupFailed => result.cleanup_failed.push(quest_id),
        }
    }
    result
}

/// Bridge a `watch`-based cancellation signal into the `mpsc` receiver the quest
/// loops already consume. Sends at most once.
fn bridge_cancel(
    mut watch_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::sync::mpsc::Receiver<()> {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        loop {
            if *watch_rx.borrow_and_update() {
                let _ = tx.send(()).await;
                return;
            }
            if watch_rx.changed().await.is_err() {
                return;
            }
        }
    });
    rx
}

/// Convert a quest-loop result plus the final cancel state into a terminal
/// outcome. A user-requested stop always wins.
fn run_outcome(cancelled: bool, result: anyhow::Result<()>) -> QuestOutcome {
    if cancelled {
        return QuestOutcome::Stopped;
    }
    match result {
        Ok(()) => QuestOutcome::Completed,
        Err(error) => QuestOutcome::Failed(error.to_string()),
    }
}

/// Read the final cancel state after a worker finishes and map it to a terminal
/// outcome. Shared so every quest kind applies the same cancellation rule.
fn worker_outcome(
    cancelled: tokio::sync::watch::Receiver<bool>,
    result: anyhow::Result<()>,
) -> QuestOutcome {
    run_outcome(*cancelled.borrow(), result)
}

/// Shared PLAY_ACTIVITY worker body, used by both the CDP (lease-holding) and
/// DirectApi (no-lease) worker factories so the two paths cannot drift.
#[allow(clippy::too_many_arguments)]
async fn run_play_activity_worker(
    transport: PlayActivityTransport,
    cdp_port: u16,
    account_id: AccountId,
    run_id: uuid::Uuid,
    quest_id: String,
    application_id: String,
    seconds_needed: u32,
    initial_progress: f64,
    heartbeat_interval: u64,
    progress_polling_interval: u64,
    client: Option<DiscordApiClient>,
    app_handle: tauri::AppHandle,
    cancel_watch: tokio::sync::watch::Receiver<bool>,
    progress: Arc<std::sync::atomic::AtomicU64>,
) -> QuestOutcome {
    let cancelled = cancel_watch.clone();
    let cancel_rx = bridge_cancel(cancel_watch);
    let emitter = QuestEventSink::new(app_handle, progress, account_id, quest_id.clone(), run_id);
    let result = if transport == PlayActivityTransport::Cdp {
        cdp_quest::complete_play_activity_via_cdp(
            cdp_port,
            quest_id,
            application_id,
            seconds_needed,
            initial_progress,
            heartbeat_interval,
            progress_polling_interval,
            emitter,
            cancel_rx,
        )
        .await
    } else {
        quest_completer::complete_play_activity_via_heartbeat(
            client
                .as_ref()
                .expect("direct PLAY_ACTIVITY mode validated an API client"),
            quest_id,
            application_id,
            seconds_needed,
            initial_progress,
            heartbeat_interval,
            progress_polling_interval,
            emitter,
            cancel_rx,
        )
        .await
    };
    worker_outcome(cancelled, result)
}

/// Boxed worker future produced by a [`QuestWorkerFactory`].
type QuestWorkerFuture = std::pin::Pin<Box<dyn std::future::Future<Output = QuestOutcome> + Send>>;

/// Boxed worker factory. The shared admit path is generic over this so legacy
/// and non-preemptive start commands cannot drift in how they register a run.
type QuestWorkerFactory = Box<
    dyn FnOnce(
            Vec<ResourceGuard>,
            tokio::sync::watch::Receiver<bool>,
            Arc<std::sync::atomic::AtomicU64>,
            uuid::Uuid,
        ) -> QuestWorkerFuture
        + Send,
>;

/// The single production wrapper for a CDP worker. The verified account lease is
/// moved into the spawned future and held until the worker's terminal
/// return/cancel, so no release/competitor can free the port while the run is
/// parked or driving CDP. Both CDP quest and CDP PLAY_ACTIVITY use this one
/// wrapper. On admission failure the factory is dropped before it runs, releasing
/// the lease.
fn cdp_worker_factory<F, Fut>(lease: CdpPortLease, run: F) -> QuestWorkerFactory
where
    F: FnOnce(
            tokio::sync::watch::Receiver<bool>,
            Arc<std::sync::atomic::AtomicU64>,
            uuid::Uuid,
        ) -> Fut
        + Send
        + 'static,
    Fut: std::future::Future<Output = QuestOutcome> + Send + 'static,
{
    Box::new(move |guards, cancel_watch, progress, run_id| {
        Box::pin(async move {
            let _guards = guards;
            let _lease = lease;
            run(cancel_watch, progress, run_id).await
        })
    })
}

/// Build the same DTO `list_quest_runs` returns, so a newly admitted run is
/// immediately observable through that command. `accountId` comes from the
/// control record, never from a synthesized global active account.
fn quest_run_dto(control: &quest_runtime::QuestControl) -> QuestRunDto {
    QuestRunDto {
        account_id: control.account_id.as_str().to_string(),
        quest_id: control.quest_id.clone(),
        run_id: control.run_id.to_string(),
        kind: control.kind.as_str().to_string(),
        transport: control.transport.as_str(),
        phase: control.phase().as_str().to_string(),
        progress: control.progress(),
    }
}

/// Secret-free account summary derived from profile metadata plus whether the
/// runtime currently holds an authenticated client.
fn account_summary_dto(registry: &AccountRegistry, profile: AccountProfile) -> AccountSummaryDto {
    let is_authenticated = registry
        .runtime(&profile.id)
        .is_some_and(|runtime| runtime.has_client());
    AccountSummaryDto {
        id: profile.id.as_str().to_string(),
        username: profile.username,
        discriminator: if profile.discriminator.is_empty() {
            None
        } else {
            Some(profile.discriminator)
        },
        avatar: profile.avatar,
        global_name: profile.global_name,
        last_cdp_port: profile.last_cdp_port,
        last_used_at_ms: profile.last_used_at_ms,
        is_authenticated,
    }
}

/// The full account list plus the active account id.
fn accounts_snapshot(registry: &AccountRegistry) -> AccountsSnapshotDto {
    AccountsSnapshotDto {
        accounts: registry
            .profiles()
            .into_iter()
            .map(|profile| account_summary_dto(registry, profile))
            .collect(),
        active_account_id: registry.active_id().map(|id| id.as_str().to_string()),
    }
}

/// The active account runtime, or the shared unauthenticated error class when no
/// account is active.
fn require_active_account(
    runtime: Option<Arc<AccountRuntime>>,
) -> Result<Arc<AccountRuntime>, String> {
    runtime.ok_or_else(|| "Not logged in".to_string())
}

/// The active account id as a string, or the unauthenticated error class.
/// Kept for callers/tests that need the string form.
#[allow(dead_code)]
fn require_active_account_id(runtime: Option<Arc<AccountRuntime>>) -> Result<String, String> {
    require_active_account(runtime).map(|runtime| runtime.id().as_str().to_string())
}

/// Shared admit + monitor setup for every quest start. It never preempts; the
/// caller decides whether to stop existing work first, so the legacy and
/// non-preemptive APIs share exactly one admission path.
///
/// `account_id` is the OWNER of the run, supplied by the caller's
/// [`QuestStartContext`]. It is never re-read from the active account here, so a
/// concurrent account switch cannot register this run under another account.
async fn admit_quest_run(
    state: &State<'_, AppState>,
    account_id: AccountId,
    app_handle: tauri::AppHandle,
    quest_id: String,
    kind: QuestKind,
    transport: QuestTransport,
    make_worker: QuestWorkerFactory,
) -> Result<QuestRunDto, String> {
    let admitted = admit_quest_run_core(
        state.quests.as_ref(),
        state.resources.as_ref(),
        account_id,
        quest_id,
        kind,
        transport,
        make_worker,
    )
    .await?;

    let dto = quest_run_dto(&admitted.control);
    spawn_quest_monitor(state.quests.clone(), admitted, app_handle);
    Ok(dto)
}

/// Pure admission seam: the run owner is passed in explicitly and the active
/// account is never consulted. Testable without a `tauri::State`.
async fn admit_quest_run_core(
    quests: &QuestRegistry,
    resources: &ResourceCoordinator,
    account_id: AccountId,
    quest_id: String,
    kind: QuestKind,
    transport: QuestTransport,
    make_worker: QuestWorkerFactory,
) -> Result<AdmittedRun, String> {
    quest_runtime::admit_run(
        quests,
        resources,
        quest_runtime::QuestAdmission {
            account_id,
            quest_id,
            kind,
            transport,
            required: kind.required_resources(transport),
        },
        make_worker,
    )
    .await
    .map_err(|error| error.to_string())
}

/// Spawn the single monitor that awaits the worker and emits exactly one
/// terminal event.
fn spawn_quest_monitor(
    registry: Arc<QuestRegistry>,
    admitted: AdmittedRun,
    app_handle: tauri::AppHandle,
) {
    tokio::spawn(async move {
        quest_runtime::monitor_run(registry.as_ref(), admitted, move |control, outcome| {
            emit_terminal_event(&app_handle, control, outcome);
        })
        .await;
    });
}

/// The one and only terminal-event emission point for a quest run. Every event
/// carries the exact account/quest/run identity from the run's control record.
fn emit_terminal_event(
    app_handle: &tauri::AppHandle,
    control: &quest_runtime::QuestControl,
    outcome: QuestOutcome,
) {
    let envelope = |kind: &str, message: Option<String>| QuestEventEnvelope {
        account_id: control.account_id.as_str().to_string(),
        quest_id: control.quest_id.clone(),
        run_id: control.run_id.to_string(),
        progress: None,
        message,
        kind: Some(kind.to_string()),
    };
    match outcome {
        QuestOutcome::Completed => {
            let _ = app_handle.emit("quest-complete", envelope("complete", None));
        }
        QuestOutcome::Stopped => {
            let _ = app_handle.emit("quest-stopped", envelope("stopped", None));
        }
        QuestOutcome::Failed(message) => {
            let label = match control.kind {
                QuestKind::Video => "Video quest: ",
                QuestKind::Stream => "Stream quest: ",
                QuestKind::Game => "Game heartbeat quest: ",
                QuestKind::PlayActivity => "PLAY_ACTIVITY quest: ",
                QuestKind::EmbeddedActivity => "CDP quest: ",
            };
            let _ = app_handle.emit(
                "quest-error",
                envelope("error", Some(format!("{label}{message}"))),
            );
        }
    }
}

/// Refuse a manual spoof while the addressed account already has a live quest
/// run. Account-scoped so another account's run never blocks this one.
fn ensure_no_active_quest(state: &AppState, account_id: &AccountId) -> Result<(), String> {
    if state.quests.has_live_runs_for_account(account_id) {
        return Err(
            "Stop the active quest before starting a manual CDP game simulation".to_string(),
        );
    }
    Ok(())
}

/// A step chosen from one [`ManualCdpGameSessionState`] snapshot by
/// [`run_scoped_manual_stop`].
enum ManualStopStep {
    /// Nothing more the scope may stop.
    Done,
    /// A matching start is in flight and has been asked to cancel.
    Cancel { owner: AccountId, generation: u64 },
    /// A matching active spoof was claimed for cleanup.
    Cleanup { claimed: ClaimedManualSession },
    /// A matching cleanup is already in flight for `owner`.
    AwaitCleanup { owner: AccountId, generation: u64 },
}

/// Stop one manual CDP spoof permitted by `scope`, WITHOUT holding the session
/// mutex across the verified-cleanup network await. A read-only status query is
/// therefore never blocked for the duration of a cleanup, and ownership is
/// re-validated before any shared state is mutated.
///
/// A `Starting` or `CleaningUp` session is NOT treated as "nothing to stop": a
/// matching stop cancels the in-flight start (so `commit_start` can no longer
/// commit) or awaits the in-progress cleanup, and only returns success once the
/// slot is terminal. It can therefore never report success while a spoof may
/// still be committed or a cleanup may still be running.
/// Run `operation` in an independently-owned task, returning its result through a
/// oneshot so aborting the caller's waiter cannot strand the operation. The task
/// owns the state transitions; the caller only observes the result. Shared by the
/// manual spoof start and stop wrappers.
async fn run_owned_operation<T, F, Fut>(operation: F) -> Result<T, String>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T, String>> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _ = tx.send(operation().await);
    });
    rx.await
        .map_err(|_| "The manual CDP game simulation task ended unexpectedly".to_string())?
}

async fn stop_manual_cdp_game_simulation_scoped(
    state: &State<'_, AppState>,
    scope: ManualStopScope,
) -> Result<(), String> {
    // The stop runs in an independently-owned task so cancelling the command
    // waiter cannot strand the session in `CleaningUp`; the task drives the
    // verified cleanup to a terminal/retryable state and reports through a
    // oneshot.
    let sessions = state.manual_cdp_game.clone();
    run_owned_operation(move || {
        let sessions = sessions;
        async move {
            run_scoped_manual_stop(sessions.as_ref(), scope, |cdp_port| async move {
                cdp_quest::stop_manual_game_spoof(cdp_port)
                    .await
                    .map_err(|error| {
                        format!(
                            "Failed to stop manual CDP game simulation: {error}. Restart Discord if the simulated game remains visible."
                        )
                    })
            })
            .await
        }
    })
    .await
}

/// Shared, testable core of [`stop_manual_cdp_game_simulation_scoped`]. `cleanup`
/// performs the verified network cleanup for a claimed session's CDP port.
async fn run_scoped_manual_stop<F, Fut>(
    sessions: &tokio::sync::Mutex<ManualCdpGameSessionState>,
    scope: ManualStopScope,
    cleanup: F,
) -> Result<(), String>
where
    F: Fn(u16) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), String>> + Send,
{
    // Subscribe BEFORE the first status read so a mutation between the read and
    // the wait below can never be missed.
    let mut changes = sessions.lock().await.subscribe();

    loop {
        let step = {
            let mut guard = sessions.lock().await;
            let status = guard.status().clone();
            match status {
                ManualSessionStatus::Absent => ManualStopStep::Done,
                ManualSessionStatus::Starting { owner, generation } if scope.matches(&owner) => {
                    guard.request_cancel(&owner, generation);
                    ManualStopStep::Cancel { owner, generation }
                }
                ManualSessionStatus::Starting { .. } => ManualStopStep::Done,
                ManualSessionStatus::Active { .. } => match guard.claim_cleanup(scope.clone()) {
                    Some(claimed) => ManualStopStep::Cleanup { claimed },
                    None => ManualStopStep::Done,
                },
                ManualSessionStatus::CleaningUp { owner, generation } if scope.matches(&owner) => {
                    ManualStopStep::AwaitCleanup { owner, generation }
                }
                ManualSessionStatus::CleaningUp { .. } => ManualStopStep::Done,
            }
        };

        match step {
            ManualStopStep::Done => return Ok(()),
            ManualStopStep::Cancel { owner, generation } => {
                wait_for_start_to_stop(
                    sessions,
                    &mut changes,
                    &owner,
                    generation,
                    MANUAL_START_STOP_WAIT,
                )
                .await?;
            }
            ManualStopStep::Cleanup { claimed } => {
                let result = cleanup(claimed.session.cdp_port).await;
                let mut guard = sessions.lock().await;
                return guard.finish_cleanup(&claimed, result);
            }
            ManualStopStep::AwaitCleanup { owner, generation } => {
                wait_for_cleanup_to_finish(
                    sessions,
                    &mut changes,
                    &owner,
                    generation,
                    MANUAL_CLEANUP_WAIT,
                )
                .await?;
            }
        }
    }
}

fn manual_start_stop_timeout_error(owner: &AccountId) -> String {
    format!(
        "Timed out waiting for the cancelled manual CDP game simulation start for {} to stop; it may still be starting. Try again.",
        owner.as_str()
    )
}

fn manual_cleanup_timeout_error(owner: &AccountId) -> String {
    format!(
        "Timed out waiting for the manual CDP game simulation cleanup for {} to finish; it may still be running. Try again.",
        owner.as_str()
    )
}

/// Await the cancelled `owner` startup leaving the `Starting` state (to `Absent`
/// after a clean cancel, or `Active` if it won the commit race). Bounded so a hung
/// start can never make a stop wait forever.
async fn wait_for_start_to_stop(
    sessions: &tokio::sync::Mutex<ManualCdpGameSessionState>,
    changes: &mut tokio::sync::watch::Receiver<u64>,
    owner: &AccountId,
    generation: u64,
    timeout: std::time::Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !sessions.lock().await.is_starting_for(owner, generation) {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(manual_start_stop_timeout_error(owner));
        }
        match tokio::time::timeout(remaining, changes.changed()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                return Err(
                    "The manual CDP game simulation state is no longer observable".to_string(),
                )
            }
            Err(_) => return Err(manual_start_stop_timeout_error(owner)),
        }
    }
}

/// Await an already-claimed cleanup for `owner` leaving `CleaningUp`. Bounded so a
/// stuck cleanup is reported instead of hanging the stop.
async fn wait_for_cleanup_to_finish(
    sessions: &tokio::sync::Mutex<ManualCdpGameSessionState>,
    changes: &mut tokio::sync::watch::Receiver<u64>,
    owner: &AccountId,
    generation: u64,
    timeout: std::time::Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !sessions.lock().await.is_cleaning_up_for(owner, generation) {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(manual_cleanup_timeout_error(owner));
        }
        match tokio::time::timeout(remaining, changes.changed()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                return Err(
                    "The manual CDP game simulation state is no longer observable".to_string(),
                )
            }
            Err(_) => return Err(manual_cleanup_timeout_error(owner)),
        }
    }
}

/// Stop the active account's runs plus its own manual CDP spoof. Legacy
/// `start_*` preemption and `stop_quest` use this so account B can never stop a
/// spoof owned by account A.
async fn stop_active_work_internal(state: &State<'_, AppState>) -> Result<(), String> {
    // No active account means there is nothing account-scoped to stop.
    let Ok(account_id) = active_account_id(state) else {
        return Ok(());
    };
    let _ = stop_account_quests_internal(state, &account_id).await;
    stop_manual_cdp_game_simulation_scoped(state, ManualStopScope::Account(account_id.clone()))
        .await
}

/// Stop ONE snapshotted account's runs plus its own manual CDP spoof. A
/// snapshotted quest start uses this for legacy preemption so it can only ever
/// affect the account that start resolved, even if the active account changed
/// before preemption ran.
async fn stop_account_work_internal(
    state: &State<'_, AppState>,
    account_id: &AccountId,
) -> Result<(), String> {
    let _ = stop_account_quests_internal(state, account_id).await;
    stop_manual_cdp_game_simulation_scoped(state, ManualStopScope::Account(account_id.clone()))
        .await
}

/// Stop every account's runs plus the manual CDP spoof regardless of owner. Used
/// only for process exit, where no account should be left running.
async fn stop_all_work_internal(state: &State<'_, AppState>) -> Result<(), String> {
    let _ = stop_all_quests_internal(state).await;
    stop_manual_cdp_game_simulation_scoped(state, ManualStopScope::All).await
}

/// Navigate Discord client SPA to a specific path (no reload)
///
/// Phase 6.3A: an authenticated account runs the verified preflight transaction
/// (reserve/verify/commit) before navigating. With no active account, or an
/// active but offline profile, only a genuinely unbound port is allowed; a bound
/// port is rejected with `cdp_port_conflict`.
#[tauri::command]
async fn navigate_discord_spa(
    target_path: String,
    cdp_port: u16,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let target = cdp_access_target(&state);
    with_cdp_access(&state, &target, cdp_port, || async {
        cdp_quest::navigate_discord_spa(cdp_port, &target_path)
            .await
            .map_err(|e| format!("Failed to navigate Discord SPA: {}", e))
    })
    .await
}

/// Create simulated game
#[tauri::command]
async fn create_simulated_game(
    path: String,
    executable_name: String,
    app_id: String,
) -> Result<(), String> {
    game_simulator::create_simulated_game(&path, &executable_name, &app_id)
        .map_err(|e| format!("Failed to create simulated game: {}", e))
}

/// Run simulated game
#[tauri::command]
async fn run_simulated_game(
    name: String,
    path: String,
    executable_name: String,
    app_id: String,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        game_simulator::run_simulated_game(&name, &path, &executable_name, &app_id)
    })
    .await
    .map_err(|e| format!("Game simulator task failed: {}", e))?
    .map_err(|e| format!("Failed to run simulated game: {}", e))
}

/// Stop simulated game
#[tauri::command]
async fn stop_simulated_game(exec_name: String) -> Result<(), String> {
    game_simulator::stop_simulated_game(&exec_name)
        .map_err(|e| format!("Failed to stop simulated game: {}", e))
}

/// Start a persistent manual game simulation inside Discord via CDP.
#[tauri::command]
async fn start_manual_cdp_game_simulation(
    app_id: String,
    app_name: String,
    cdp_port: u16,
    state: State<'_, AppState>,
) -> Result<ManualCdpGameSimulation, String> {
    let app_id = app_id.trim().to_string();
    let app_name = app_name.trim().to_string();
    if app_id.is_empty() {
        return Err("Application ID is required for CDP game simulation".to_string());
    }
    if app_name.is_empty() {
        return Err("Application name is required for CDP game simulation".to_string());
    }
    if cdp_port == 0 {
        return Err("CDP port must be between 1 and 65535".to_string());
    }

    // Refuse while this account already has a live quest run so a manual spoof
    // cannot inject a second Discord activity for it. A coherent online session
    // (user AND client) is required before any side effect.
    let session = cdp_online_session(&state)?;
    let account_id = session.account_id().clone();
    ensure_no_active_quest(&state, &account_id)?;

    // Unified lease: a verified account lease for the port, acquired before any
    // CDP mutation. The manual spoof is not preemptive, so contention is an
    // immediate error. The lease is owned by the session state below for the whole
    // Starting/Active/CleaningUp lifecycle.
    let lease = acquire_account_lease_once(&state, &session, cdp_port)
        .await
        .map_err(|error| error.to_string())?;
    let guards = state
        .resources
        .try_acquire_all(&account_id, &[QuestResource::AccountActivity])
        .map_err(|error| error.to_string())?;
    let generation = {
        let mut sessions = state.manual_cdp_game.lock().await;
        sessions.begin_start(account_id.clone(), guards, Some(lease))?
    };

    // Run the CDP startup in an independently-owned task so a cancelled command
    // waiter cannot strand the session. The task finalizes Starting -> Active or
    // Absent (validating this exact generation) and reports through a oneshot.
    let sessions = state.manual_cdp_game.clone();
    let task_owner = account_id.clone();
    run_owned_operation(move || {
        let sessions = sessions;
        async move {
            run_manual_start_task(
                sessions.as_ref(),
                &task_owner,
                generation,
                cdp_port,
                app_id,
                app_name,
            )
            .await
        }
    })
    .await
}

/// The detached manual-spoof startup. Owns the `Starting` -> `Active` (or
/// `Absent`) transitions so cancelling the command waiter never strands state.
async fn run_manual_start_task(
    sessions: &tokio::sync::Mutex<ManualCdpGameSessionState>,
    owner: &AccountId,
    generation: u64,
    cdp_port: u16,
    app_id: String,
    app_name: String,
) -> Result<ManualCdpGameSimulation, String> {
    let startup = async {
        let status = cdp_client::check_cdp_available(cdp_port).await;
        if !status.connected {
            return Err(status
                .error
                .unwrap_or_else(|| format!("Discord CDP is not connected on port {cdp_port}")));
        }
        // A stop that landed before the install aborts here so no spoof is ever
        // installed for a cancelled start.
        if sessions.lock().await.start_cancelled_for(owner, generation) {
            return Err("The manual CDP game simulation start was cancelled".to_string());
        }
        cdp_quest::start_manual_game_spoof(cdp_port, &app_id, &app_name)
            .await
            .map_err(|error| format!("Failed to start manual CDP game simulation: {error}"))
    }
    .await;

    if let Err(error) = startup {
        sessions.lock().await.cancel_start(owner, generation);
        return Err(error);
    }

    let session = ManualCdpGameSimulation {
        app_id,
        app_name,
        cdp_port,
    };
    // A cancellation that raced the install makes `commit_start` refuse; the
    // just-installed spoof is removed so a cancelled start never leaves an
    // injection behind. If that removal also fails, the spoof (and its lease) is
    // parked as `Active` so a later stop/exit retries. Only this exact generation
    // may commit or be cleared.
    let commit_result = sessions
        .lock()
        .await
        .commit_start(owner, generation, session.clone());
    if let Err(error) = commit_result {
        let cleanup = cdp_quest::stop_manual_game_spoof(cdp_port).await;
        let mut guard = sessions.lock().await;
        return match cleanup {
            Ok(()) => {
                guard.cancel_start(owner, generation);
                Err(error)
            }
            Err(cleanup_error) => {
                guard.park_installed_but_uncommitted(owner, generation, session);
                Err(format!(
                    "{error}; removing the installed game simulation also failed: {cleanup_error}. Stop the simulation again to retry cleanup."
                ))
            }
        };
    }
    Ok(session)
}

/// Stop and fully verify cleanup of the active account's manual CDP game
/// simulation. Owner-scoped: a different active account cannot stop a spoof it
/// does not own. The all-account exit path uses `stop_all_work_internal`.
#[tauri::command]
async fn stop_manual_cdp_game_simulation(state: State<'_, AppState>) -> Result<(), String> {
    let Ok(account_id) = active_account_id(&state) else {
        return Ok(());
    };
    stop_manual_cdp_game_simulation_scoped(&state, ManualStopScope::Account(account_id.clone()))
        .await
}

/// Return the backend-owned manual CDP game simulation, if one is active. This
/// is a read-only status query and never blocks on another account's in-flight
/// startup or cleanup.
#[tauri::command]
async fn get_manual_cdp_game_simulation(
    state: State<'_, AppState>,
) -> Result<Option<ManualCdpGameSimulation>, String> {
    Ok(state.manual_cdp_game.lock().await.active())
}

/// Read the saved global proxy policy. Never returns credentials.
#[tauri::command]
async fn get_proxy_settings(state: State<'_, AppState>) -> Result<ProxySettingsDto, String> {
    let runtime = state.proxy.clone();
    tokio::task::spawn_blocking(move || runtime.read_dto())
        .await
        .map_err(|error| format!("Proxy settings task failed: {error}"))?
        .map_err(|error| error.to_string())
}

/// Apply a global proxy policy. Credentials (if any) are written only to the OS
/// credential store; nothing is ever echoed back.
#[tauri::command]
async fn set_proxy_settings(
    input: ProxySettingsInput,
    state: State<'_, AppState>,
) -> Result<ProxySettingsDto, String> {
    // The whole transaction (validate, stage credentials, prepare transport,
    // persist, install) runs on a blocking thread under the shared coordination
    // gate. The replacement transport is built before persistence, so a failure
    // never reports success while traffic still uses the old policy, and a
    // concurrent login publication cannot interleave.
    let registry = state.accounts.clone();
    let runtime = state.proxy.clone();
    tokio::task::spawn_blocking(move || coordinated_proxy_set(&registry, &runtime, input))
        .await
        .map_err(|error| format!("Proxy settings task failed: {error}"))?
}

/// Delete any saved proxy credential and persist the credential-free state.
#[tauri::command]
async fn clear_proxy_credentials(state: State<'_, AppState>) -> Result<ProxySettingsDto, String> {
    let registry = state.accounts.clone();
    let runtime = state.proxy.clone();
    tokio::task::spawn_blocking(move || coordinated_clear_proxy_credentials(&registry, &runtime))
        .await
        .map_err(|error| format!("Proxy settings task failed: {error}"))?
}

/// Read one account's secret-free proxy override + effective inherited policy.
#[tauri::command]
async fn get_account_proxy_settings(
    account_id: String,
    state: State<'_, AppState>,
) -> Result<AccountProxySettingsDto, String> {
    let account_id = AccountId::parse(&account_id).map_err(|error| error.to_string())?;
    let runtime = state.proxy.clone();
    tokio::task::spawn_blocking(move || runtime.read_account_dto(&account_id))
        .await
        .map_err(|error| format!("Proxy settings task failed: {error}"))?
        .map_err(|error| error.to_string())
}

/// Set (or replace) one account's proxy override. Credentials go only to the OS
/// store under an account-scoped reference; every live account client is rebuilt
/// under the coordination gate.
#[tauri::command]
async fn set_account_proxy_override(
    account_id: String,
    input: AccountProxyOverrideInput,
    state: State<'_, AppState>,
) -> Result<AccountProxySettingsDto, String> {
    let account_id = AccountId::parse(&account_id).map_err(|error| error.to_string())?;
    let registry = state.accounts.clone();
    let runtime = state.proxy.clone();
    tokio::task::spawn_blocking(move || {
        coordinated_set_account_override(&registry, &runtime, account_id, input)
    })
    .await
    .map_err(|error| format!("Proxy settings task failed: {error}"))?
}

/// Remove one account's proxy override (restoring global inheritance) and
/// pending-delete its superseded credential.
#[tauri::command]
async fn clear_account_proxy_override(
    account_id: String,
    state: State<'_, AppState>,
) -> Result<AccountProxySettingsDto, String> {
    let account_id = AccountId::parse(&account_id).map_err(|error| error.to_string())?;
    let registry = state.accounts.clone();
    let runtime = state.proxy.clone();
    tokio::task::spawn_blocking(move || {
        coordinated_clear_account_override(&registry, &runtime, account_id)
    })
    .await
    .map_err(|error| format!("Proxy settings task failed: {error}"))?
}

/// Send one unauthenticated request through the effective policy to a fixed
/// Discord endpoint. Redirects are disabled and nothing is saved.
#[tauri::command]
async fn test_proxy_connection(state: State<'_, AppState>) -> Result<ProxyTestResult, String> {
    let runtime = state.proxy.clone();
    // Strict resolution: a corrupt/incomplete configuration blocks the probe
    // rather than silently testing under a fallback policy.
    let configuration = tokio::task::spawn_blocking(move || runtime.resolve_current())
        .await
        .map_err(|error| format!("Proxy settings task failed: {error}"))?
        .map_err(|error| error.to_string())?;

    let client = discord_api::build_probe_client(&configuration)
        .map_err(|error| format!("Could not prepare the proxy test: {error}"))?;

    match client.get(discord_api::PROXY_TEST_URL).send().await {
        Ok(response) => {
            let status = response.status().as_u16();
            let (ok, message) = if response.status().is_success() {
                (true, "Connected through the configured policy.".to_string())
            } else {
                (false, format!("Discord returned HTTP {status}."))
            };
            Ok(ProxyTestResult {
                ok,
                status: Some(status),
                message,
            })
        }
        Err(error) => {
            let message = if error.is_timeout() {
                "The proxy test timed out."
            } else if error.is_connect() {
                "Could not connect through the configured policy."
            } else {
                "The proxy test request failed."
            };
            Ok(ProxyTestResult {
                ok: false,
                status: None,
                message: message.to_string(),
            })
        }
    }
}

/// Get detectable games list (works with or without login)
#[tauri::command]
async fn fetch_detectable_games(state: State<'_, AppState>) -> Result<Vec<DetectableGame>, String> {
    // Use the authenticated client when available (carries auth headers + super-properties).
    // When not logged in, fall back to a plain public HTTP request — the detectable-games
    // endpoints require no authentication.
    let auth_client = optional_active_client(&state);

    if let Some(client) = auth_client {
        return client
            .fetch_detectable_games()
            .await
            .map_err(|e| format!("Failed to get games list: {}", e));
    }

    // ── Unauthenticated fallback ──────────────────────────────────────────
    // This public request must honor the same global proxy policy as the
    // authenticated client (System / Direct / Custom).
    let proxy = resolve_proxy_configuration(&state).await?;
    let http_builder = reqwest::Client::builder()
        .user_agent(super_properties::discord_user_agent(
            super_properties::DEFAULT_CLIENT_VERSION,
        ))
        .connect_timeout(std::time::Duration::from_secs(8))
        .timeout(std::time::Duration::from_secs(20));
    let http = discord_api::apply_proxy_policy(http_builder, &proxy)
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

    const API_BASE: &str = "https://discord.com/api/v9";
    let games_url = format!("{}/applications/detectable", API_BASE);
    let apps_url = format!("{}/applications/non-games/detectable", API_BASE);

    let (games_res, apps_res) =
        tokio::join!(http.get(&games_url).send(), http.get(&apps_url).send());

    let mut all_items: Vec<DetectableGame> = Vec::new();

    if let Ok(resp) = games_res {
        if resp.status().is_success() {
            if let Ok(mut list) = resp.json::<Vec<DetectableGame>>().await {
                for g in &mut list {
                    g.type_name = Some("Game".to_string());
                }
                all_items.extend(list);
            }
        }
    }

    if let Ok(resp) = apps_res {
        if resp.status().is_success() {
            if let Ok(mut list) = resp.json::<Vec<DetectableGame>>().await {
                for a in &mut list {
                    a.type_name = Some("App".to_string());
                }
                all_items.extend(list);
            }
        }
    }

    Ok(all_items)
}

/// Accept quest
#[tauri::command]
async fn accept_quest(
    quest_id: String,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let client = active_client(&state)?;

    let result = client
        .accept_quest(&quest_id)
        .await
        .map_err(|e| format!("Failed to accept quest: {}", e))?;

    Ok(result)
}

#[tauri::command]
async fn get_virtual_currency_balance(
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let client = active_client(&state)?;

    client
        .get_virtual_currency_balance()
        .await
        .map_err(|e| format!("Failed to get virtual currency balance: {}", e))
}

#[tauri::command]
async fn get_billing_subscriptions(
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let client = active_client(&state)?;

    client
        .get_billing_subscriptions()
        .await
        .map_err(|e| format!("Failed to get billing subscriptions: {}", e))
}

#[tauri::command]
async fn get_program_rewards(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let client = active_client(&state)?;

    client
        .get_program_rewards()
        .await
        .map_err(|e| format!("Failed to get program rewards: {}", e))
}

#[tauri::command]
async fn get_quest_decision_debug(
    placement: u64,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let client = active_client(&state)?;

    client
        .get_quest_decision_debug(placement)
        .await
        .map_err(|e| format!("Failed to get quest placement decision: {}", e))
}

#[tauri::command]
async fn get_quest_decisions_debug(
    placement: u64,
    num: u64,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let client = active_client(&state)?;

    client
        .get_quest_decisions_debug(placement, num)
        .await
        .map_err(|e| format!("Failed to get quest placement decisions: {}", e))
}

#[tauri::command]
async fn claim_quest_reward(
    quest_id: String,
    platform: Option<String>,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let client = active_client(&state)?;

    client
        .claim_quest_reward(&quest_id, platform)
        .await
        .map_err(|e| format!("Failed to claim quest reward: {}", e))
}

mod rpc;
mod runner;

use once_cell::sync::OnceCell;
static DISCORD_RPC_CLIENT: OnceCell<Mutex<Option<rpc::Client>>> = OnceCell::new();

fn get_discord_rpc_client() -> &'static Mutex<Option<rpc::Client>> {
    DISCORD_RPC_CLIENT.get_or_init(|| Mutex::new(None))
}

#[tauri::command(rename_all = "snake_case")]
fn connect_to_discord_rpc(handle: tauri::AppHandle, activity_json: String, action: String) {
    let _ = action;
    let app = handle.clone();

    let event_connecting = "client_connecting";
    let event_connected = "client_connected";
    let event_disconnect = "event_disconnect";

    let activity = runner::parse_activity_json(&activity_json).unwrap();

    let connecting_payload = serde_json::json!({
        "app_id": activity.app_id,
    });

    // Clear existing client
    {
        let mut client_guard = get_discord_rpc_client().lock().unwrap();
        client_guard.take();
    }

    let task = tauri::async_runtime::spawn(async move {
        handle
            .emit(event_connecting, connecting_payload)
            .unwrap_or_else(|e| eprintln!("Failed to emit event: {}", e));

        let client_result = runner::set_activity(activity_json).await;

        match client_result {
            Ok(client) => {
                let connected_payload = serde_json::json!({
                    "app_id": activity.app_id,
                });

                {
                    let mut client_guard = get_discord_rpc_client().lock().unwrap();
                    *client_guard = Some(client);
                }

                handle
                    .emit(event_connected, connected_payload)
                    .unwrap_or_else(|e| {
                        eprintln!("Failed to emit event: {}", e);
                    });

                handle.listen(event_disconnect, move |_| {
                    println!("Disconnecting from Discord RPC inner");
                    drop(tauri::async_runtime::spawn(async move {
                        let client_option = {
                            let mut client_guard = get_discord_rpc_client().lock().unwrap();
                            client_guard.take()
                        };
                        if let Some(client) = client_option {
                            client.discord.disconnect().await;
                            println!("Disconnected from Discord RPC inner");
                        }
                    }));
                });
            }
            Err(e) => {
                println!("Failed to set activity: {}", e);
            }
        }
    });

    app.listen(event_disconnect, move |_| {
        println!("Disconnecting from Discord RPC...");
        task.abort();
    });
}

#[tauri::command]
async fn disconnect_from_discord_rpc(app: tauri::AppHandle) -> Result<(), String> {
    // Cancel a connection task that may still be waiting for Discord. Without
    // this, a stop click immediately after launch could be followed by the
    // pending task storing a new RPC client and restoring the presence.
    let _ = app.emit("event_disconnect", ());

    let client = get_discord_rpc_client()
        .lock()
        .map_err(|_| "Discord RPC state lock is poisoned".to_string())?
        .take();

    if let Some(client) = client {
        client.discord.disconnect().await;
    }

    Ok(())
}

#[tauri::command]
async fn open_in_explorer(path: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let mut path = path.replace("/", "\\");
        // Explorer generally doesn't like the \\?\ prefix for opening folders
        if path.starts_with("\\\\?\\") {
            path = path[4..].to_string();
        }
        println!("Opening explorer at: {}", path);
        std::process::Command::new("explorer")
            .arg(path)
            .spawn()
            .map_err(|e| format!("Failed to open explorer: {}", e))?;
    }
    #[cfg(target_os = "macos")]
    {
        println!("Opening Finder at: {}", path);
        std::process::Command::new("/usr/bin/open")
            .arg(&path)
            .spawn()
            .map_err(|e| format!("Failed to open Finder: {}", e))?;
    }
    #[cfg(target_os = "linux")]
    {
        println!("Opening file manager at: {}", path);
        std::process::Command::new("xdg-open")
            .arg(&path)
            .spawn()
            .map_err(|e| format!("Failed to open directory: {}", e))?;
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = path; // Suppress unused variable warning on other platforms
    }
    Ok(())
}

/// Initialize the platform runtime identity before creating any window.
pub fn initialize_runtime_identity_and_run() {
    configure_linux_webkit_runtime();

    runtime_identity::initialize();

    // Set up cleanup hook for panics with recursion guard
    use std::sync::atomic::{AtomicBool, Ordering};
    static CLEANUP_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        if !CLEANUP_IN_PROGRESS.swap(true, Ordering::SeqCst) {
            // Use catch_unwind to safely run cleanup
            let cleanup_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                runtime_identity::cleanup_on_exit();
            }));

            if cleanup_result.is_err() {
                eprintln!("[Runtime] Error: panic occurred during cleanup in panic hook");
            }

            // Do NOT reset flag - if we panicked, we don't want to try cleaning up again
            // CLEANUP_IN_PROGRESS.store(false, Ordering::SeqCst);
        }
        // Wrap original_hook call in catch_unwind to prevent nested panics
        let hook_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            original_hook(panic_info);
        }));
        if hook_result.is_err() {
            eprintln!("[Runtime] Error: original panic hook panicked");
        }
    }));

    // Register Ctrl+C handler
    if let Err(e) = ctrlc::set_handler(move || {
        // Kill all simulated game child processes before exiting
        let cleanup_games_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            game_simulator::cleanup_all_simulated_games();
        }));
        if cleanup_games_result.is_err() {
            eprintln!("[Cleanup] Error: panic during game cleanup in Ctrl+C handler");
        }

        // Wrap runtime cleanup in catch_unwind to log any errors before exiting
        let cleanup_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime_identity::cleanup_on_exit();
        }));
        if cleanup_result.is_err() {
            eprintln!("[Runtime] Error: panic occurred during cleanup in Ctrl+C handler");
        }
        std::process::exit(0);
    }) {
        eprintln!("Warning: Failed to register Ctrl+C handler: {}", e);
    }

    // Run main application
    run();
}

/// WebKitGTK can create a window but render an entirely blank surface when its
/// accelerated compositing path runs inside a VMware guest with 3D enabled.
/// Configure the upstream-supported fallback before Tauri initializes GTK.
#[cfg(target_os = "linux")]
fn configure_linux_webkit_runtime() {
    // Tauri's AppImage GTK hook currently forces GDK_BACKEND=x11. If no X11
    // display exists but a Wayland socket was explicitly supplied, restore the
    // only usable backend before Tauri initializes GTK.
    if std::env::var_os("WAYLAND_DISPLAY").is_some()
        && std::env::var_os("DISPLAY").is_none()
        && std::env::var_os("GDK_BACKEND").as_deref() == Some(std::ffi::OsStr::new("x11"))
    {
        std::env::set_var("GDK_BACKEND", "wayland");
    }

    if std::env::var_os("WEBKIT_DISABLE_COMPOSITING_MODE").is_some()
        || std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_some()
    {
        return;
    }

    let product_name =
        std::fs::read_to_string("/sys/class/dmi/id/product_name").unwrap_or_default();
    let system_vendor = std::fs::read_to_string("/sys/class/dmi/id/sys_vendor").unwrap_or_default();

    if linux_webkit_needs_software_compositing(&product_name, &system_vendor) {
        std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
        println!("[WebKit] Disabled accelerated compositing for VMware compatibility");
    }
}

#[cfg(not(target_os = "linux"))]
fn configure_linux_webkit_runtime() {}

#[cfg(target_os = "linux")]
fn linux_webkit_needs_software_compositing(product_name: &str, system_vendor: &str) -> bool {
    product_name.to_ascii_lowercase().contains("vmware")
        || system_vendor.to_ascii_lowercase().contains("vmware")
}

#[cfg(all(test, target_os = "linux"))]
mod linux_webkit_runtime_tests {
    use super::linux_webkit_needs_software_compositing;

    #[test]
    fn detects_vmware_without_matching_physical_hosts() {
        assert!(linux_webkit_needs_software_compositing(
            "VMware Virtual Platform",
            "VMware, Inc."
        ));
        assert!(!linux_webkit_needs_software_compositing(
            "Precision 7680",
            "Dell Inc."
        ));
    }
}

fn create_main_window(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let window_config = app
        .config()
        .app
        .windows
        .first()
        .cloned()
        .ok_or("missing window configuration")?;

    let mut builder = WebviewWindowBuilder::from_config(app.handle(), &window_config)?;

    if runtime_identity::uses_temporary_runtime() {
        let title = runtime_identity::runtime_window_title();
        builder = builder.title(&title);
        if let Some(user_data) = runtime_identity::webview_user_data_dir() {
            std::fs::create_dir_all(&user_data)?;
            builder = builder.data_directory(user_data);
        }
    }

    builder.build()?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // State is managed here so the proxy runtime can use the resolved
            // app config directory for its versioned settings file.
            let config_dir = app.path().app_config_dir()?;
            let proxy_runtime = Arc::new(ProxyRuntime::new(
                Arc::new(KeyringCredentialStore),
                proxy_settings::proxy_settings_path(&config_dir),
            ));
            let accounts = Arc::new(AccountRegistry::new(account_runtime::accounts_path(
                &config_dir,
            )));
            // Load persisted non-secret profiles. A corrupt/unsupported file is
            // reported (and left untouched) rather than silently discarded, and
            // leaves the registry read-only so a later login cannot overwrite it.
            if let Err(error) = accounts.load_from_disk() {
                use crate::logger::{log, LogCategory, LogLevel};
                log(
                    LogLevel::Warn,
                    LogCategory::General,
                    "Saved account profiles could not be loaded; account persistence is disabled until the file is repaired",
                    Some(&error.to_string()),
                );
            }
            app.manage(AppState {
                accounts,
                quests: Arc::new(QuestRegistry::new()),
                resources: Arc::new(ResourceCoordinator::new()),
                manual_cdp_game: Arc::new(tokio::sync::Mutex::new(
                    ManualCdpGameSessionState::default(),
                )),
                leases: Arc::new(CdpPortLeases::new()),
                proxy: proxy_runtime.clone(),
            });

            // Load the saved policy once at startup on a blocking thread so a
            // locked keychain is reported without stalling the executor.
            let startup_runtime = proxy_runtime.clone();
            tauri::async_runtime::spawn_blocking(move || {
                if let Err(error) = startup_runtime.refresh_from_disk() {
                    use crate::logger::{log, LogCategory, LogLevel};
                    log(
                        LogLevel::Warn,
                        LogCategory::Api,
                        "Saved proxy settings could not be loaded at startup",
                        Some(&error.to_string()),
                    );
                }
            });

            // `pnpm tauri:dev` rebuilds the bundled launcher before Tauri
            // starts. If a Linux launcher entry was created previously,
            // refresh its binary, desktop entry, and icon on every dev start
            // so developers always test the current launcher build.
            #[cfg(all(debug_assertions, target_os = "linux"))]
            if let Some((port, channel, client, installation)) =
                linux_existing_cdp_launcher_options()
            {
                let app_handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    match create_discord_cdp_launcher_shortcut_internal(
                        &app_handle,
                        port,
                        channel,
                        client,
                        installation,
                    )
                    .await
                    {
                        Ok(path) => {
                            println!("[cdp-launcher-dev] Refreshed existing Linux launcher: {path}")
                        }
                        Err(error) => eprintln!(
                            "[cdp-launcher-dev] Failed to refresh existing Linux launcher: {error}"
                        ),
                    }
                });
            }

            create_main_window(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            auto_login_via_cdp,
            auto_add_account_via_cdp,
            preview_cdp_identity,
            confirm_add_cdp_account,
            reconnect_cdp_account,
            get_quests,
            get_quests_full,
            start_video_quest,
            start_stream_quest,
            start_game_heartbeat_quest,
            start_play_activity_quest,
            start_cdp_quest,
            start_video_quest_run,
            start_stream_quest_run,
            start_game_heartbeat_quest_run,
            start_play_activity_quest_run,
            start_cdp_quest_run,
            stop_quest,
            list_quest_runs,
            stop_quest_run,
            stop_all_quests,
            list_all_quest_runs,
            stop_account_quest_run,
            stop_account_quests,
            list_accounts,
            activate_account,
            activate_online_account,
            remove_account,
            get_proxy_settings,
            set_proxy_settings,
            clear_proxy_credentials,
            get_account_proxy_settings,
            set_account_proxy_override,
            clear_account_proxy_override,
            test_proxy_connection,
            create_simulated_game,
            run_simulated_game,
            stop_simulated_game,
            start_manual_cdp_game_simulation,
            stop_manual_cdp_game_simulation,
            get_manual_cdp_game_simulation,
            fetch_detectable_games,
            accept_quest,
            get_virtual_currency_balance,
            get_billing_subscriptions,
            get_program_rewards,
            get_quest_decision_debug,
            get_quest_decisions_debug,
            claim_quest_reward,
            connect_to_discord_rpc,
            disconnect_from_discord_rpc,
            open_in_explorer,
            force_video_progress,
            export_logs,
            get_debug_info,
            get_runner_info,
            check_cdp_status,
            fetch_super_properties_cdp,
            fetch_running_games_cdp,
            discord_cdp_commands::is_discord_running,
            discord_cdp_commands::get_desktop_client_state,
            discord_cdp_commands::add_desktop_client_installation,
            discord_cdp_commands::remove_desktop_client_installation,
            discord_cdp_commands::set_desktop_client_selection,
            discord_cdp_commands::launch_desktop_client_cdp,
            discord_cdp_commands::list_desktop_clients,
            discord_cdp_commands::list_running_discord_cdp_sessions,
            discord_cdp_commands::list_running_desktop_cdp_sessions,
            discord_cdp_commands::restore_desktop_client_session,
            discord_cdp_commands::launch_discord_cdp,
            discord_cdp_commands::restart_discord_cdp,
            create_discord_cdp_launcher_shortcut,
            start_discord_normal_restore_helper,
            prepare_app_exit,
            exit_app_now,
            get_super_properties_mode,
            auto_fetch_super_properties,
            retry_super_properties,
            capture_discord_headers_cdp,
            navigate_discord_spa,
            platform_capabilities::get_platform_capabilities,
            runtime_identity::get_runtime_identity_status,
            runtime_identity::get_runtime_identity_audit
        ])
        .on_window_event(|_window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                prepare_app_exit_fallback();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[tauri::command]
async fn prepare_app_exit(state: State<'_, AppState>) -> Result<(), String> {
    prepare_active_work_and_local_cleanup(&state).await
}

/// End the main process after the frontend has completed its best-effort
/// cleanup.  This must not go through Tauri's window-close machinery: that
/// machinery is intentionally intercepted to show the CDP warning dialog,
/// and routing the confirmed action back through it can leave the window
/// alive with the frontend's close guard latched.
#[tauri::command]
async fn exit_app_now(state: State<'_, AppState>) -> Result<(), String> {
    // The close UI fail-opens after a short prepare deadline so a hung Discord
    // evaluation cannot trap the window. This command is the last chance to
    // finish or retry CDP rollback before the process disappears.
    match tokio::time::timeout(
        APP_EXIT_FINAL_CLEANUP_TIMEOUT,
        prepare_active_work_and_local_cleanup(&state),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            eprintln!("Active-work cleanup failed during process exit: {error}");
            prepare_app_exit_fallback();
        }
        Err(_) => {
            eprintln!(
                "Final exit cleanup timed out after {APP_EXIT_FINAL_CLEANUP_TIMEOUT:?}; terminating anyway"
            );
            prepare_app_exit_fallback();
        }
    }
    std::process::exit(0);
}

/// Exit may only complete when no CDP port lease remains. A live lease keeps exit
/// retryable; there is deliberately no force-release path. Pure so the policy is
/// unit-testable.
fn exit_lease_error(leases: &CdpPortLeases) -> Option<String> {
    if leases.is_empty() {
        None
    } else {
        Some(format!(
            "{} CDP port lease(s) are still held after cleanup; refusing to finish exit while a CDP operation is live.",
            leases.len()
        ))
    }
}

async fn prepare_active_work_and_local_cleanup(state: &State<'_, AppState>) -> Result<(), String> {
    if APP_EXIT_CLEANUP.is_prepared() {
        return Ok(());
    }

    // Manual CDP injections must be removed while the Discord targets are
    // still reachable. Preserve any error until the remaining local cleanup
    // has run so an RPC/game cleanup failure cannot strand another resource.
    // Do not mark exit prepared until this cleanup succeeds; otherwise a
    // later prepare_app_exit (or a retried close) would skip rollback.
    let active_work_error = stop_all_work_internal(state).await.err();
    cleanup_local_resources_on_exit().await;
    // Unified lease: exit NEVER force-releases a live lease. It stops work /
    // manual cleanup to terminal (which drops those leases naturally) and only
    // completes when no CDP port lease remains. A failed cleanup or a still-live
    // operation retains its lease and leaves exit retryable.
    let active_work_error = active_work_error.or_else(|| exit_lease_error(state.leases.as_ref()));
    match active_work_error {
        Some(error) => Err(error),
        None => {
            APP_EXIT_CLEANUP.mark_prepared();
            Ok(())
        }
    }
}

fn prepare_app_exit_fallback() {
    if APP_EXIT_CLEANUP.is_prepared() {
        return;
    }
    // Fallback cannot reach Discord via CDP (no AppState). Still run the
    // one-shot local cleanup if prepare_app_exit has not claimed it yet.
    cleanup_local_resources_on_exit_sync();
}

fn take_discord_rpc_client_for_exit() -> Option<rpc::Client> {
    match get_discord_rpc_client().lock() {
        Ok(mut guard) => guard.take(),
        Err(_) => {
            eprintln!("Discord RPC state lock is poisoned during app exit");
            None
        }
    }
}

async fn cleanup_local_resources_on_exit() {
    if !APP_EXIT_CLEANUP.claim_local_cleanup() {
        return;
    }
    game_simulator::cleanup_all_simulated_games();
    if let Some(client) = take_discord_rpc_client_for_exit() {
        if tokio::time::timeout(APP_EXIT_RPC_DISCONNECT_TIMEOUT, client.discord.disconnect())
            .await
            .is_err()
        {
            eprintln!("Discord RPC disconnect timed out during app exit");
        }
    }
    runtime_identity::cleanup_on_exit();
}

fn cleanup_local_resources_on_exit_sync() {
    if !APP_EXIT_CLEANUP.claim_local_cleanup() {
        return;
    }
    game_simulator::cleanup_all_simulated_games();
    if let Some(client) = take_discord_rpc_client_for_exit() {
        tauri::async_runtime::spawn(async move {
            client.discord.disconnect().await;
        });
    }
    runtime_identity::cleanup_on_exit();
}

#[tauri::command]
async fn start_discord_normal_restore_helper(
    app_handle: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    // The helper restores every Discord client to normal, affecting all ports'
    // CDP state. Acquire the exclusive all-ports maintenance lease BEFORE any
    // helper process action; a live account/direct per-port lease fails with the
    // stable cdp_port_conflict error and nothing is force-released.
    let lease = state
        .leases
        .acquire_global_maintenance()
        .map_err(|error| error.to_string())?;
    let launcher = find_bundled_cdp_launcher(&app_handle)?;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    match runtime_bridge::verify_bundled_for_execution(&launcher) {
        Ok(()) => runtime_identity::record_helper_identity(Ok(())),
        Err(error) => {
            runtime_identity::record_helper_identity(Err(error.clone()));
            return Err(error);
        }
    }
    // The global lease lives in the blocking closure through the helper's
    // completion, so cancelling this command cannot release it early.
    let (result, _lease) =
        spawn_blocking_with_lease(lease, move || spawn_restore_helper(&launcher))
            .await
            .map_err(|error| format!("Discord restore helper task failed: {error}"))?;
    result
}

fn spawn_restore_helper(launcher: &std::path::Path) -> Result<(), String> {
    use std::process::{Command, Stdio};

    let mut command = Command::new(launcher);
    command
        .arg("--restore-normal-all")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    // Wait for the helper to finish so the caller's global maintenance lease is
    // held for the whole restore, not just the process launch.
    let mut child = command
        .spawn()
        .map_err(|error| format!("Failed to start Discord restore helper: {error}"))?;
    let status = child
        .wait()
        .map_err(|error| format!("Discord restore helper failed: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Discord restore helper exited with {status}"))
    }
}

/// Force update video progress (used for ensuring final progress is saved on stop)
#[tauri::command]
async fn force_video_progress(
    quest_id: String,
    timestamp: f64,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let client = active_client(&state)?;

    client
        .update_video_progress(&quest_id, timestamp)
        .await
        .map_err(|e| format!("Failed to force video progress: {}", e))?;

    Ok(())
}

/// Export application logs as JSON
#[tauri::command]
async fn export_logs() -> Result<String, String> {
    logger::export_logs().map_err(|e| format!("Failed to export logs: {}", e))
}

/// Get debug info including X-Super-Properties for the active account.
#[tauri::command]
async fn get_debug_info(state: State<'_, AppState>) -> Result<super_properties::DebugInfo, String> {
    Ok(active_super_properties(&state).get_debug_info())
}

/// Get embedded runner version information
#[tauri::command]
async fn get_runner_info() -> game_simulator::RunnerInfo {
    game_simulator::get_runner_info()
}

/// Check CDP status
#[tauri::command]
async fn check_cdp_status(port: Option<u16>) -> cdp_client::CdpStatus {
    let port = port.unwrap_or(cdp_client::DEFAULT_CDP_PORT);
    cdp_client::check_cdp_available(port).await
}

/// Fetch SuperProperties via CDP.
///
/// The target account (and its identity handle) is snapshotted BEFORE any CDP
/// await. An authenticated account runs the verified preflight transaction before
/// the capture; an offline/no-account target is scratch-only, may use only an
/// unbound port, and never updates a saved profile. A capture can therefore never
/// be written into another account's request identity or into an offline profile.
#[tauri::command]
async fn fetch_super_properties_cdp(
    port: Option<u16>,
    state: State<'_, AppState>,
) -> Result<cdp_client::CdpSuperProperties, String> {
    let port = port.unwrap_or(cdp_client::DEFAULT_CDP_PORT);
    let target = cdp_access_target(&state);
    // The permit is held across the capture and the identity write.
    with_cdp_access(&state, &target, port, || async {
        let result = cdp_client::fetch_super_properties_via_cdp(port)
            .await
            .map_err(|e| e.to_string())?;
        // Update the snapshotted account's identity (scratch if no account was active).
        target
            .handle()
            .set_from_cdp(&result.base64, &result.decoded);
        Ok(result)
    })
    .await
}

/// Read Discord's currently loaded game detector state via CDP.
///
/// This performs a CDP Runtime evaluation, so it takes the unified lease for the
/// same lifetime as the other capture commands.
#[tauri::command]
async fn fetch_running_games_cdp(
    port: Option<u16>,
    state: State<'_, AppState>,
) -> Result<cdp_client::CdpRunningGamesSnapshot, String> {
    let port = port.unwrap_or(cdp_client::DEFAULT_CDP_PORT);
    let target = cdp_access_target(&state);
    with_cdp_access(&state, &target, port, || async {
        cdp_client::fetch_running_games_via_cdp(port)
            .await
            .map_err(|e| e.to_string())
    })
    .await
}

/// Capture Discord API request headers via CDP Network interception.
///
/// Snapshots the target identity before any await. An authenticated account runs
/// the verified preflight transaction; an offline/no-account target is
/// scratch-only and may use only an unbound port, so captured headers can never
/// leak another account's identity into this account or into an offline profile.
#[tauri::command]
async fn capture_discord_headers_cdp(
    port: Option<u16>,
    duration_secs: Option<u64>,
    state: State<'_, AppState>,
) -> Result<cdp_client::CdpCapturedHeaders, String> {
    let port = port.unwrap_or(cdp_client::DEFAULT_CDP_PORT);
    let duration = duration_secs.unwrap_or(30);
    let target = cdp_access_target(&state);
    // The permit is held across the capture and the identity write.
    with_cdp_access(&state, &target, port, || async {
        let captured = cdp_client::capture_discord_headers_via_cdp(port, duration)
            .await
            .map_err(|e| e.to_string())?;
        // Apply captured headers to the snapshotted account's identity only.
        let identity = target.handle();
        for request in &captured.requests {
            identity.update_header_profile_from_headers(&request.headers);
        }
        Ok(captured)
    })
    .await
}

/// Get the active account's SuperProperties source mode and build number.
#[tauri::command]
fn get_super_properties_mode(state: State<'_, AppState>) -> serde_json::Value {
    let manager = active_super_properties(&state);
    serde_json::json!({
        "mode": manager.get_mode().as_str(),
        "mode_display": manager.get_mode().display_name(),
        "build_number": manager.get_build_number()
    })
}

/// Auto-fetch SuperProperties with fallback: CDP -> Default, for the account
/// snapshotted in `target` (scratch handle when no account is active). Rejects a
/// port the snapshotted account does not own before any capture.
async fn auto_fetch_super_properties_core(
    target: &CdpAccessTarget,
    port: u16,
) -> Result<serde_json::Value, String> {
    use crate::logger::{log, LogCategory, LogLevel};

    let manager = target.handle();

    // Priority 1: Try CDP
    log(
        LogLevel::Info,
        LogCategory::TokenExtraction,
        &format!("Auto-fetching SuperProperties, trying CDP on port {}", port),
        None,
    );

    if let Ok(cdp_result) = cdp_client::fetch_super_properties_via_cdp(port).await {
        manager.set_from_cdp(&cdp_result.base64, &cdp_result.decoded);
        log(
            LogLevel::Info,
            LogCategory::TokenExtraction,
            &format!(
                "SuperProperties obtained via CDP. Build: {:?}",
                manager.get_build_number()
            ),
            None,
        );
        return Ok(serde_json::json!({
            "success": true,
            "mode": "cdp",
            "build_number": manager.get_build_number()
        }));
    }

    // Safe build: do not fall back to remote JavaScript scraping. If CDP is
    // unavailable, keep the built-in defaults and wait for the user to start
    // an explicitly selected Discord CDP session.
    log(
        LogLevel::Warn,
        LogCategory::TokenExtraction,
        "CDP unavailable; using default values without remote JavaScript",
        None,
    );

    Ok(serde_json::json!({
        "success": false,
        "mode": "default",
        "build_number": manager.get_build_number()
    }))
}

/// Auto-fetch SuperProperties for the active account (scratch when none active).
#[tauri::command]
async fn auto_fetch_super_properties(
    cdp_port: Option<u16>,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let port = cdp_port.unwrap_or(cdp_client::DEFAULT_CDP_PORT);
    let target = cdp_access_target(&state);
    // Phase 6.3A: verified preflight, held across the fetch/mutation.
    with_cdp_access(&state, &target, port, || async {
        auto_fetch_super_properties_core(&target, port).await
    })
    .await
}

/// Reset the snapshotted target's captured identity for a fresh fetch. Callers
/// must have completed the binding preflight first, so a rejected/conflicting
/// port leaves the captured identity byte-for-byte unchanged.
fn prepare_super_properties_retry(target: &CdpAccessTarget) -> Result<(), String> {
    target.handle().reset();
    Ok(())
}

/// Retry fetching SuperProperties (resets the snapshotted account's identity).
#[tauri::command]
async fn retry_super_properties(
    cdp_port: Option<u16>,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let port = cdp_port.unwrap_or(cdp_client::DEFAULT_CDP_PORT);
    let target = cdp_access_target(&state);
    // Phase 6.3A: verified preflight, held across the retry reset and fetch.
    with_cdp_access(&state, &target, port, || async {
        prepare_super_properties_retry(&target)?;
        auto_fetch_super_properties_core(&target, port).await
    })
    .await
}

#[tauri::command]
async fn create_discord_cdp_launcher_shortcut(
    app_handle: tauri::AppHandle,
    port: Option<u16>,
    channel: Option<String>,
    client: Option<String>,
    installation_path: Option<String>,
) -> Result<String, String> {
    let channel = discord_cdp_launch_core::parse_discord_channel(channel.as_deref())
        .map_err(|error| error.to_string())?;
    let port = port.unwrap_or(cdp_client::DEFAULT_CDP_PORT);
    let client = discord_cdp_launch_core::parse_desktop_client_preference(client.as_deref())
        .map_err(|error| error.to_string())?;
    create_discord_cdp_launcher_shortcut_internal(
        &app_handle,
        port,
        channel,
        client,
        installation_path.map(std::path::PathBuf::from),
    )
    .await
}

async fn install_discord_cdp_launcher_internal(
    app_handle: &tauri::AppHandle,
) -> Result<std::path::PathBuf, String> {
    let app_handle = app_handle.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        install_discord_cdp_launcher_impl(&app_handle)
    })
    .await
    .map_err(|error| format!("Runtime bridge installation task failed: {error}"))?;
    match &result {
        Ok((_, Some(warning))) => runtime_identity::record_helper_degraded(warning.clone()),
        Ok((_, None)) => runtime_identity::record_helper_identity(Ok(())),
        Err(error) => runtime_identity::record_helper_identity(Err(error.clone())),
    }
    result.map(|(path, _)| path)
}

fn install_discord_cdp_launcher_impl(
    app_handle: &tauri::AppHandle,
) -> Result<(std::path::PathBuf, Option<String>), String> {
    let source = find_bundled_cdp_launcher(app_handle)?;

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let data_root = unix_runtime_data_root()?;
        let legacy = legacy_unix_cdp_launcher_path()?;
        let report = runtime_bridge::install(&source, &data_root, &legacy)?;
        Ok((report.executable, report.legacy_cleanup_warning))
    }

    #[cfg(windows)]
    {
        use std::fs;
        let target = stable_cdp_launcher_path()?;

        let source_size = fs::metadata(&source).map(|m| m.len()).unwrap_or(0);
        if cfg!(debug_assertions) {
            println!("[Runtime] Installing bridge payload ({source_size} bytes)");
        }

        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create CDP launcher directory: {}", e))?;
        }

        if source != target {
            fs::copy(&source, &target)
                .map_err(|e| format!("Failed to install runtime bridge: {e}"))?;
        }

        if let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") {
            migrate_legacy_windows_cdp_launcher_at(std::path::Path::new(&local_appdata), &target);
        }
        Ok((target, None))
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = source;
        Err("CDP launcher installation is unsupported on this platform".into())
    }
}

#[cfg(windows)]
fn stable_cdp_launcher_path() -> Result<std::path::PathBuf, String> {
    let local_appdata =
        std::env::var_os("LOCALAPPDATA").ok_or_else(|| "Could not get LOCALAPPDATA".to_string())?;
    let pointer = windows_cdp_runtime_pointer_path()?;
    Ok(resolve_windows_cdp_runtime_path(
        std::path::Path::new(&local_appdata),
        &pointer,
    ))
}

#[cfg(target_os = "macos")]
fn unix_runtime_data_root() -> Result<std::path::PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or_else(|| "Could not get HOME".to_string())?;
    Ok(std::path::PathBuf::from(home)
        .join("Library")
        .join("Application Support"))
}

#[cfg(target_os = "linux")]
fn unix_runtime_data_root() -> Result<std::path::PathBuf, String> {
    linux_xdg_data_home()
}

#[cfg(target_os = "macos")]
fn legacy_unix_cdp_launcher_path() -> Result<std::path::PathBuf, String> {
    Ok(unix_runtime_data_root()?
        .join("Discord Quest Helper")
        .join("discord-cdp-launcher"))
}

#[cfg(target_os = "linux")]
fn legacy_unix_cdp_launcher_path() -> Result<std::path::PathBuf, String> {
    Ok(unix_runtime_data_root()?
        .join("discord-quest-helper")
        .join("bin")
        .join("discord-cdp-launcher"))
}

#[cfg(any(windows, test))]
const WINDOWS_CDP_APP_CONFIG_DIR: &str = "com.masterain.discord-quest-helper";
#[cfg(any(windows, test))]
const WINDOWS_CDP_RUNTIME_POINTER: &str = "cdp-runtime-exe.txt";
#[cfg(any(windows, test))]
const WINDOWS_LEGACY_CDP_DIR: &str = "DiscordQuestHelper";
#[cfg(any(windows, test))]
const WINDOWS_LEGACY_CDP_EXE: &str = "DiscordCdpLauncher.exe";

#[cfg(any(windows, test))]
fn windows_cdp_runtime_pointer_path_from(appdata: &std::path::Path) -> std::path::PathBuf {
    appdata
        .join(WINDOWS_CDP_APP_CONFIG_DIR)
        .join(WINDOWS_CDP_RUNTIME_POINTER)
}

#[cfg(windows)]
fn windows_cdp_runtime_pointer_path() -> Result<std::path::PathBuf, String> {
    let appdata = std::env::var_os("APPDATA").ok_or_else(|| "Could not get APPDATA".to_string())?;
    Ok(windows_cdp_runtime_pointer_path_from(std::path::Path::new(
        &appdata,
    )))
}

/// `%LOCALAPPDATA%/<16 hex>/<12 hex>.exe` — layout only, not a full-path
/// substring scan (user profile names can contain product tokens).
#[cfg(any(windows, test))]
fn is_windows_bland_runtime_exe(path: &std::path::Path, local_appdata: &std::path::Path) -> bool {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("exe") => {}
        _ => return false,
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if !runtime_identity::is_hex_str(stem, runtime_identity::FILE_HEX_LEN) {
        return false;
    }
    let parent = match path.parent() {
        Some(dir) => dir,
        None => return false,
    };
    let parent_name = parent.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if !runtime_identity::is_hex_str(parent_name, runtime_identity::DIR_HEX_LEN) {
        return false;
    }
    let Some(grandparent) = parent.parent() else {
        return false;
    };
    runtime_identity::paths_eq(grandparent, local_appdata)
}

#[cfg(any(windows, test))]
fn allocate_windows_bland_runtime_exe(local_appdata: &std::path::Path) -> std::path::PathBuf {
    local_appdata
        .join(runtime_identity::generate_random_suffix(
            runtime_identity::DIR_HEX_LEN,
        ))
        .join(format!(
            "{}.exe",
            runtime_identity::generate_random_suffix(runtime_identity::FILE_HEX_LEN)
        ))
}

#[cfg(any(windows, test))]
fn resolve_windows_cdp_runtime_path(
    local_appdata: &std::path::Path,
    pointer_file: &std::path::Path,
) -> std::path::PathBuf {
    if let Ok(stored) = std::fs::read_to_string(pointer_file) {
        let stored = std::path::PathBuf::from(stored.trim());
        if is_windows_bland_runtime_exe(&stored, local_appdata) && stored.is_file() {
            return stored;
        }
    }
    let next = allocate_windows_bland_runtime_exe(local_appdata);
    if let Some(parent) = pointer_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(pointer_file, next.to_string_lossy().as_bytes());
    next
}

#[cfg(any(windows, test))]
fn migrate_legacy_windows_cdp_launcher_at(
    local_appdata: &std::path::Path,
    new_target: &std::path::Path,
) {
    let old_dir = local_appdata.join(WINDOWS_LEGACY_CDP_DIR);
    let old_exe = old_dir.join(WINDOWS_LEGACY_CDP_EXE);
    if old_exe.is_file() && !new_target.exists() {
        if let Some(parent) = new_target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::copy(&old_exe, new_target);
    }
    if old_dir.exists() {
        let _ = std::fs::remove_dir_all(&old_dir);
    }
}

#[cfg(test)]
fn runtime_name_has_product_tokens(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("discord")
        || lower.contains("quest")
        || lower.contains("cdp")
        || lower.contains("helper")
}

#[cfg(target_os = "linux")]
fn linux_xdg_data_home() -> Result<std::path::PathBuf, String> {
    std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| std::path::PathBuf::from(home).join(".local").join("share"))
        })
        .ok_or_else(|| "Could not determine XDG data home".to_string())
}

#[cfg(target_os = "linux")]
fn linux_cdp_launcher_desktop_path() -> Result<std::path::PathBuf, String> {
    Ok(linux_xdg_data_home()?
        .join("applications")
        .join("com.masterain.discord-quest-helper.cdp.desktop"))
}

#[cfg(target_os = "linux")]
fn linux_existing_cdp_launcher_options() -> Option<(
    u16,
    Option<discord_cdp_launch_core::DiscordChannel>,
    discord_cdp_launch_core::DesktopClientPreference,
    Option<std::path::PathBuf>,
)> {
    let desktop_path = linux_cdp_launcher_desktop_path().ok()?;
    if !desktop_path.exists() {
        return None;
    }

    let contents = std::fs::read_to_string(desktop_path).unwrap_or_default();
    Some(linux_cdp_launcher_options_from_desktop(&contents))
}

#[cfg(target_os = "linux")]
fn linux_cdp_launcher_options_from_desktop(
    contents: &str,
) -> (
    u16,
    Option<discord_cdp_launch_core::DiscordChannel>,
    discord_cdp_launch_core::DesktopClientPreference,
    Option<std::path::PathBuf>,
) {
    let mut port = cdp_client::DEFAULT_CDP_PORT;
    let mut channel = None;
    let mut client = discord_cdp_launch_core::DesktopClientPreference::Auto;
    let mut installation = None;
    let Some(exec) = contents.lines().find_map(|line| line.strip_prefix("Exec=")) else {
        return (port, channel, client, installation);
    };
    let args = discord_cdp_launch_core::parse_desktop_exec_arguments(exec);

    for pair in args.windows(2) {
        match pair[0].as_str() {
            "--port" => {
                if let Ok(value) = pair[1].parse::<u16>() {
                    if value != 0 {
                        port = value;
                    }
                }
            }
            "--channel" => {
                if let Ok(value) =
                    discord_cdp_launch_core::parse_discord_channel(Some(pair[1].as_str()))
                {
                    channel = value;
                }
            }
            "--client" | "--provider" => {
                if let Ok(value) =
                    discord_cdp_launch_core::parse_desktop_client_preference(Some(pair[1].as_str()))
                {
                    client = value;
                }
            }
            "--installation" => installation = Some(std::path::PathBuf::from(&pair[1])),
            _ => {}
        }
    }

    (port, channel, client, installation)
}

fn find_bundled_cdp_launcher(app_handle: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let names = cdp_launcher_binary_names();
    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
    let mut candidate_dirs = bundled_cdp_launcher_candidate_dirs(
        cfg!(debug_assertions),
        std::env::current_dir().ok().as_deref(),
        app_handle.path().resource_dir().ok().as_deref(),
        std::env::current_exe().ok().as_deref(),
    );

    #[cfg(target_os = "windows")]
    add_windows_cdp_launcher_install_dirs(&mut candidate_dirs);

    if let Some(candidate) = find_cdp_launcher_in_dirs(&names, &candidate_dirs) {
        return Ok(candidate);
    }

    if cfg!(debug_assertions) {
        let searched: Vec<String> = candidate_dirs
            .iter()
            .map(|directory| directory.display().to_string())
            .collect();
        Err(format!(
            "Runtime bridge is unavailable (names: {names:?}, searched: {searched:?}). \
             Run `pnpm build:cdp-launcher` and try again."
        ))
    } else {
        Err(
            "The packaged runtime bridge is unavailable or invalid. Reinstall the application."
                .to_string(),
        )
    }
}

fn bundled_cdp_launcher_candidate_dirs(
    include_development_dirs: bool,
    current_dir: Option<&std::path::Path>,
    resource_dir: Option<&std::path::Path>,
    current_exe: Option<&std::path::Path>,
) -> Vec<std::path::PathBuf> {
    let mut candidate_dirs = Vec::new();

    // Dev mode: cwd-based paths (cwd is typically the repo root during `tauri dev`).
    // This also covers Windows portable/install layouts where the sidecar is
    // placed at the install root.
    if include_development_dirs {
        if let Some(cwd) = current_dir {
            candidate_dirs.push(cwd.to_path_buf());
            candidate_dirs.push(cwd.join("src-tauri").join("binaries"));
            candidate_dirs.push(cwd.join("binaries"));
        }
    }

    if let Some(resource_dir) = resource_dir {
        candidate_dirs.push(resource_dir.to_path_buf());
        candidate_dirs.push(resource_dir.join("binaries"));
    }

    // Tauri puts external binaries next to the main executable in macOS app
    // bundles, Linux packages/AppImages, and installed Windows applications.
    if let Some(parent) = current_exe.and_then(std::path::Path::parent) {
        candidate_dirs.push(parent.to_path_buf());
        candidate_dirs.push(parent.join("binaries"));
        #[cfg(target_os = "macos")]
        candidate_dirs.push(parent.join("../Resources"));
    }

    candidate_dirs
}

fn find_cdp_launcher_in_dirs(
    names: &[&str],
    candidate_dirs: &[std::path::PathBuf],
) -> Option<std::path::PathBuf> {
    candidate_dirs.iter().find_map(|directory| {
        names.iter().find_map(|name| {
            let candidate = directory.join(name);
            // build-cdp-launcher.js creates an empty placeholder so Tauri can
            // validate its config before the real build. Never execute it, and
            // reject directories that happen to share the sidecar name.
            std::fs::metadata(&candidate)
                .ok()
                .filter(|metadata| metadata.is_file() && metadata.len() > 0)
                .map(|_| candidate)
        })
    })
}

#[cfg(test)]
mod bundled_cdp_launcher_tests {
    use super::{bundled_cdp_launcher_candidate_dirs, find_cdp_launcher_in_dirs};
    use std::fs;
    use std::path::PathBuf;

    fn unique_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{label}-{}-{}",
            std::process::id(),
            super::runtime_identity::generate_random_suffix(8)
        ))
    }

    #[test]
    fn locates_external_binary_next_to_packaged_main_executable() {
        for relative_main in [
            "Discord Quest Helper.app/Contents/MacOS/meridian",
            "appimage-mount/usr/bin/meridian",
            "deb-root/usr/bin/meridian",
        ] {
            let root = unique_root("dqh-bundled-launcher");
            let main = root.join(relative_main);
            let helper = main.parent().unwrap().join("waybridge");
            fs::create_dir_all(main.parent().unwrap()).unwrap();
            fs::write(&main, b"main").unwrap();
            fs::write(&helper, b"helper").unwrap();

            let directories = bundled_cdp_launcher_candidate_dirs(false, None, None, Some(&main));
            assert_eq!(
                find_cdp_launcher_in_dirs(&["waybridge"], &directories),
                Some(helper)
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn release_lookup_ignores_current_directory_sidecar() {
        let root = unique_root("dqh-bundled-launcher-release");
        let cwd = root.join("cwd");
        let resource = root.join("resource");
        let main = root.join("app/Contents/MacOS/meridian");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&resource).unwrap();
        fs::create_dir_all(main.parent().unwrap()).unwrap();
        fs::write(cwd.join("waybridge"), b"untrusted cwd helper").unwrap();
        fs::write(resource.join("waybridge"), b"packaged helper").unwrap();

        let directories =
            bundled_cdp_launcher_candidate_dirs(true, Some(&cwd), Some(&resource), Some(&main));
        assert_eq!(
            find_cdp_launcher_in_dirs(&["waybridge"], &directories),
            Some(cwd.join("waybridge"))
        );
        let release_directories =
            bundled_cdp_launcher_candidate_dirs(false, Some(&cwd), Some(&resource), Some(&main));
        assert_eq!(
            find_cdp_launcher_in_dirs(&["waybridge"], &release_directories),
            Some(resource.join("waybridge"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn skips_empty_placeholders_and_non_files() {
        let root = unique_root("dqh-bundled-launcher-invalid");
        let empty_dir = root.join("empty");
        let directory_dir = root.join("directory");
        let valid_dir = root.join("valid");
        fs::create_dir_all(&empty_dir).unwrap();
        fs::create_dir_all(directory_dir.join("waybridge")).unwrap();
        fs::create_dir_all(&valid_dir).unwrap();
        fs::write(empty_dir.join("waybridge"), []).unwrap();
        fs::write(valid_dir.join("waybridge"), b"helper").unwrap();

        assert_eq!(
            find_cdp_launcher_in_dirs(
                &["waybridge"],
                &[empty_dir, directory_dir, valid_dir.clone()]
            ),
            Some(valid_dir.join("waybridge"))
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(target_os = "windows")]
fn add_windows_cdp_launcher_install_dirs(candidate_dirs: &mut Vec<std::path::PathBuf>) {
    const PRODUCT_DIR: &str = "Discord Quest Helper";

    for var_name in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Some(root) = std::env::var_os(var_name) {
            candidate_dirs.push(std::path::PathBuf::from(root).join(PRODUCT_DIR));
        }
    }

    if let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") {
        let local_appdata = std::path::PathBuf::from(local_appdata);
        candidate_dirs.push(local_appdata.join("Programs").join(PRODUCT_DIR));
        candidate_dirs.push(local_appdata.join(PRODUCT_DIR));
    }
}

fn cdp_launcher_binary_names() -> Vec<&'static str> {
    #[cfg(target_os = "windows")]
    {
        vec![
            // Tauri bundles externalBin sidecars under the base name in installed apps.
            "waybridge.exe",
            // Dev/build trees keep the target triple because Tauri validates this input name.
            "waybridge-x86_64-pc-windows-msvc.exe",
        ]
    }

    #[cfg(target_os = "macos")]
    {
        #[cfg(target_arch = "aarch64")]
        {
            vec!["waybridge", "waybridge-aarch64-apple-darwin"]
        }
        #[cfg(target_arch = "x86_64")]
        {
            vec!["waybridge", "waybridge-x86_64-apple-darwin"]
        }
    }

    #[cfg(target_os = "linux")]
    {
        #[cfg(target_arch = "aarch64")]
        {
            vec!["waybridge", "waybridge-aarch64-unknown-linux-gnu"]
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            vec!["waybridge", "waybridge-x86_64-unknown-linux-gnu"]
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        Vec::new()
    }
}

async fn create_discord_cdp_launcher_shortcut_internal(
    app_handle: &tauri::AppHandle,
    port: u16,
    channel: Option<discord_cdp_launch_core::DiscordChannel>,
    client: discord_cdp_launch_core::DesktopClientPreference,
    installation_path: Option<std::path::PathBuf>,
) -> Result<String, String> {
    let launcher_path = install_discord_cdp_launcher_internal(app_handle).await?;
    let arguments = cdp_launcher_shortcut_arguments(port, channel, client, installation_path)?;
    create_platform_cdp_launcher_shortcut(&launcher_path, &arguments)
}

fn cdp_launcher_shortcut_arguments(
    port: u16,
    channel: Option<discord_cdp_launch_core::DiscordChannel>,
    client: discord_cdp_launch_core::DesktopClientPreference,
    installation_path: Option<std::path::PathBuf>,
) -> Result<Vec<String>, String> {
    if port == 0 {
        return Err("CDP port must be between 1 and 65535.".to_string());
    }
    if installation_path.is_some()
        && client == discord_cdp_launch_core::DesktopClientPreference::Auto
    {
        return Err("An exact installation requires an explicit desktop client.".to_string());
    }
    let mut arguments = vec![
        "--port".to_string(),
        port.to_string(),
        "--channel".to_string(),
        channel
            .map(|value| value.as_str())
            .unwrap_or("auto")
            .to_string(),
        "--client".to_string(),
        client.as_str().to_string(),
    ];
    if let Some(path) = installation_path {
        let path = path.to_string_lossy().into_owned();
        if path.contains(char::is_control) || path.contains('"') {
            return Err("Installation path contains unsupported characters.".to_string());
        }
        arguments.push("--installation".to_string());
        arguments.push(path);
    }
    Ok(arguments)
}

#[cfg(target_os = "windows")]
fn create_platform_cdp_launcher_shortcut(
    _launcher_path: &std::path::Path,
    _arguments: &[String],
) -> Result<String, String> {
    Err(
        "Windows CDP shortcut creation is disabled in the safe build; PowerShell is not used."
            .to_string(),
    )
}

#[cfg(target_os = "macos")]
fn create_platform_cdp_launcher_shortcut(
    launcher_path: &std::path::Path,
    arguments: &[String],
) -> Result<String, String> {
    let home = std::env::var_os("HOME").ok_or_else(|| "Could not get HOME".to_string())?;
    let desktop = std::path::PathBuf::from(home).join("Desktop");
    create_macos_cdp_launcher_shortcut_at(&desktop, launcher_path, arguments)
}

#[cfg(target_os = "macos")]
fn create_macos_cdp_launcher_shortcut_at(
    desktop: &std::path::Path,
    launcher_path: &std::path::Path,
    arguments: &[String],
) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;

    if !desktop.is_dir() {
        return Err("Could not get desktop path".to_string());
    }
    let script_path = desktop.join("Discord CDP Launcher.command");

    // Use single quotes to prevent shell metacharacter expansion ($, `, \, ")
    fn shell_single_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    let arguments = arguments
        .iter()
        .map(|argument| shell_single_quote(argument))
        .collect::<Vec<_>>()
        .join(" ");
    let script_content = format!(
        "#!/bin/bash\n{} {}\n",
        shell_single_quote(&launcher_path.to_string_lossy()),
        arguments
    );

    std::fs::write(&script_path, &script_content)
        .map_err(|e| format!("Failed to write launcher command: {}", e))?;
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("Failed to mark launcher command executable: {}", e))?;

    Ok(script_path.to_string_lossy().to_string())
}

#[cfg(all(test, target_os = "macos"))]
mod macos_cdp_shortcut_tests {
    use super::create_macos_cdp_launcher_shortcut_at;
    use discord_cdp_launch_core::DiscordChannel;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn creates_executable_script_with_shell_quoted_launcher_path() {
        let root = std::env::temp_dir().join(format!(
            "dqh-macos-shortcut-{}-{}",
            std::process::id(),
            super::runtime_identity::generate_random_suffix(8)
        ));
        let desktop = root.join("Desktop");
        fs::create_dir_all(&desktop).unwrap();
        let launcher = root.join("it'works/waybridge");

        let created = create_macos_cdp_launcher_shortcut_at(
            &desktop,
            &launcher,
            &[
                "--port".into(),
                "9444".into(),
                "--channel".into(),
                DiscordChannel::Canary.as_str().into(),
                "--client".into(),
                "vesktop".into(),
            ],
        )
        .unwrap();
        let created = std::path::PathBuf::from(created);
        let contents = fs::read_to_string(&created).unwrap();
        assert!(contents.contains(
            "it'\\''works/waybridge' '--port' '9444' '--channel' 'canary' '--client' 'vesktop'"
        ));
        assert_ne!(
            fs::metadata(&created).unwrap().permissions().mode() & 0o111,
            0
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(target_os = "linux")]
fn create_platform_cdp_launcher_shortcut(
    launcher_path: &std::path::Path,
    arguments: &[String],
) -> Result<String, String> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let data_home = linux_xdg_data_home()?;

    let applications_dir = data_home.join("applications");
    std::fs::create_dir_all(&applications_dir)
        .map_err(|e| format!("Failed to create applications directory: {}", e))?;

    // Desktop Entry icon names resolve through the freedesktop icon theme, not
    // through Tauri's bundled resources. Install the launcher's dedicated icon
    // alongside the .desktop entry so GNOME/KDE do not fall back to a generic
    // executable icon (especially in dev builds where the main app is not
    // installed system-wide).
    const ICON_NAME: &str = "com.masterain.discord-quest-helper.cdp";
    const ICON_BYTES: &[u8] = include_bytes!("../../public/icons/launcher-logo.png");
    let icon_theme_dir = data_home.join("icons").join("hicolor");
    let icon_dir = icon_theme_dir.join("512x512").join("apps");
    std::fs::create_dir_all(&icon_dir)
        .map_err(|e| format!("Failed to create launcher icon directory: {}", e))?;
    let icon_path = icon_dir.join(format!("{ICON_NAME}.png"));
    std::fs::write(&icon_path, ICON_BYTES)
        .map_err(|e| format!("Failed to install CDP launcher icon: {}", e))?;

    let desktop_path = linux_cdp_launcher_desktop_path()?;

    let launcher_display = launcher_path.to_string_lossy();
    // A newline anywhere in the path would close the `Exec=`/`TryExec=` value
    // and let the rest be parsed as further Desktop Entry keys. Quoting cannot
    // express control characters, so reject them outright rather than emit a
    // file whose meaning depends on the reader's leniency.
    if launcher_display.contains(char::is_control) {
        return Err(
            "Launcher path contains control characters; refusing to write a desktop entry."
                .to_string(),
        );
    }
    let exec_program = desktop_entry_exec_quote(&launcher_display);
    let exec_arguments = arguments
        .iter()
        .map(|argument| desktop_entry_exec_quote(argument))
        .collect::<Vec<_>>()
        .join(" ");

    let contents = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Discord CDP Launcher\n\
         Comment=Launch Discord with CDP enabled\n\
         Exec={exec} {arguments}\n\
         TryExec={tryexec}\n\
         Icon={icon}\n\
         Terminal=false\n\
         Categories=Utility;\n\
         StartupNotify=true\n",
        exec = exec_program,
        arguments = exec_arguments,
        tryexec = launcher_display,
        // Use the absolute path in the desktop entry. GNOME Shell can retain a
        // generic fallback cached before a newly installed themed icon exists;
        // a direct path avoids that stale theme lookup entirely.
        icon = icon_path.to_string_lossy(),
    );

    // Write to a temp file in the same directory, then atomically replace any
    // existing desktop entry. `rename` replaces the destination on Linux.
    let tmp_path = applications_dir.join(format!(
        ".com.masterain.discord-quest-helper.cdp.desktop.{}.tmp",
        std::process::id()
    ));
    {
        let mut file = std::fs::File::create(&tmp_path)
            .map_err(|e| format!("Failed to write desktop entry: {}", e))?;
        file.write_all(contents.as_bytes())
            .map_err(|e| format!("Failed to write desktop entry: {}", e))?;
        file.set_permissions(std::fs::Permissions::from_mode(0o644))
            .map_err(|e| format!("Failed to set desktop entry permissions: {}", e))?;
    }
    std::fs::rename(&tmp_path, &desktop_path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        format!("Failed to install desktop entry: {}", e)
    })?;

    // Best-effort refresh of the desktop database; failure is non-fatal.
    let _ = std::process::Command::new("update-desktop-database")
        .arg(&applications_dir)
        .status();
    let _ = std::process::Command::new("gtk-update-icon-cache")
        .args(["-f", "-t"])
        .arg(&icon_theme_dir)
        .status();

    Ok(desktop_path.to_string_lossy().to_string())
}

/// Escape a value for use inside a double-quoted Desktop Entry `Exec` argument.
/// Reserved characters are escaped with a backslash; backslash is escaped first.
/// Field codes (`%f`, `%u`, …) are expanded before quoting is undone, so a
/// literal percent sign must be written as `%%` even inside quotes.
#[cfg(target_os = "linux")]
fn desktop_entry_exec_quote(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('`', "\\`")
        .replace('$', "\\$")
        .replace('%', "%%");
    format!("\"{}\"", escaped)
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn create_platform_cdp_launcher_shortcut(
    _launcher_path: &std::path::Path,
    _arguments: &[String],
) -> Result<String, String> {
    Err("Shortcut creation is only supported on Windows, macOS and Linux.".to_string())
}

#[cfg(all(test, target_os = "linux"))]
mod desktop_entry_tests {
    use super::{desktop_entry_exec_quote, linux_cdp_launcher_options_from_desktop};
    use discord_cdp_launch_core::{DesktopClientPreference, DiscordChannel};

    #[test]
    fn quotes_plain_paths() {
        assert_eq!(
            desktop_entry_exec_quote("/usr/bin/discord-cdp-launcher"),
            "\"/usr/bin/discord-cdp-launcher\""
        );
    }

    #[test]
    fn escapes_reserved_shell_characters() {
        assert_eq!(
            desktop_entry_exec_quote(r#"/tmp/we"ir$d`\path"#),
            r#""/tmp/we\"ir\$d\`\\path""#
        );
    }

    #[test]
    fn doubles_literal_percent_so_it_is_not_read_as_a_field_code() {
        // `%f`/`%u` are expanded before quoting is undone, so a path containing
        // a percent sign must be written `%%` or the entry silently mangles it.
        assert_eq!(
            desktop_entry_exec_quote("/opt/My %f App/launcher"),
            "\"/opt/My %%f App/launcher\""
        );
    }

    #[test]
    fn keeps_existing_launcher_port_and_channel_during_dev_refresh() {
        let desktop = r#"[Desktop Entry]
Exec="/opt/Discord Quest Helper/discord-cdp-launcher" --port 9444 --channel canary
"#;
        assert_eq!(
            linux_cdp_launcher_options_from_desktop(desktop),
            (
                9444,
                Some(DiscordChannel::Canary),
                DesktopClientPreference::Auto,
                None,
            )
        );
    }

    #[test]
    fn invalid_existing_launcher_options_fall_back_to_defaults() {
        let desktop = "Exec=/tmp/launcher --port 0 --channel unsupported\n";
        assert_eq!(
            linux_cdp_launcher_options_from_desktop(desktop),
            (
                super::cdp_client::DEFAULT_CDP_PORT,
                None,
                DesktopClientPreference::Auto,
                None,
            )
        );
    }

    #[test]
    fn preserves_quoted_installation_paths_with_spaces() {
        let desktop = r#"Exec="/tmp/launcher" --provider vesktop --installation "/opt/Vesktop Portable/vesktop" --port 9555
"#;
        assert_eq!(
            linux_cdp_launcher_options_from_desktop(desktop),
            (
                9555,
                None,
                DesktopClientPreference::Vesktop,
                Some(std::path::PathBuf::from("/opt/Vesktop Portable/vesktop")),
            )
        );
    }
}

#[cfg(test)]
mod windows_cdp_runtime_path_tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn unique_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "dqh-cdp-runtime-{}",
            runtime_identity::generate_random_suffix(8)
        ))
    }

    fn assert_bland_leaves(path: &Path) {
        let file = path.file_stem().and_then(|s| s.to_str()).unwrap();
        let dir = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap();
        assert!(
            runtime_identity::is_hex_str(dir, runtime_identity::DIR_HEX_LEN),
            "{dir}"
        );
        assert!(
            runtime_identity::is_hex_str(file, runtime_identity::FILE_HEX_LEN),
            "{file}"
        );
        assert!(!runtime_name_has_product_tokens(dir));
        assert!(!runtime_name_has_product_tokens(file));
        assert!(!runtime_name_has_product_tokens(&format!("{file}.exe")));
    }

    #[test]
    fn pointer_file_uses_app_config_dir() {
        let pointer = windows_cdp_runtime_pointer_path_from(Path::new("/roaming"));
        assert_eq!(
            pointer.file_name().and_then(|n| n.to_str()),
            Some(WINDOWS_CDP_RUNTIME_POINTER)
        );
        assert_eq!(
            pointer
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str()),
            Some(WINDOWS_CDP_APP_CONFIG_DIR)
        );
    }

    #[test]
    fn allocates_hex_layout_without_product_names() {
        let root = unique_root();
        let local = root.join("Local");
        let pointer = windows_cdp_runtime_pointer_path_from(&root.join("Roaming"));
        fs::create_dir_all(&local).unwrap();
        let path = resolve_windows_cdp_runtime_path(&local, &pointer);
        assert_bland_leaves(&path);
        assert_eq!(
            path.parent().and_then(|p| p.parent()),
            Some(local.as_path())
        );
        let stored = fs::read_to_string(&pointer).unwrap();
        assert_eq!(PathBuf::from(stored.trim()), path);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reuses_pointer_when_file_still_exists() {
        let root = unique_root();
        let local = root.join("Local");
        let pointer = windows_cdp_runtime_pointer_path_from(&root.join("Roaming"));
        fs::create_dir_all(&local).unwrap();
        let first = resolve_windows_cdp_runtime_path(&local, &pointer);
        fs::create_dir_all(first.parent().unwrap()).unwrap();
        fs::write(&first, b"exe").unwrap();
        let second = resolve_windows_cdp_runtime_path(&local, &pointer);
        assert_eq!(first, second);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reallocates_when_pointer_target_is_missing() {
        let root = unique_root();
        let local = root.join("Local");
        let pointer = windows_cdp_runtime_pointer_path_from(&root.join("Roaming"));
        fs::create_dir_all(&local).unwrap();
        let first = resolve_windows_cdp_runtime_path(&local, &pointer);
        let second = resolve_windows_cdp_runtime_path(&local, &pointer);
        assert_ne!(first, second);
        assert_bland_leaves(&second);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn ignores_legacy_product_path_in_pointer() {
        let root = unique_root();
        let local = root.join("Local");
        let pointer = windows_cdp_runtime_pointer_path_from(&root.join("Roaming"));
        fs::create_dir_all(pointer.parent().unwrap()).unwrap();
        fs::create_dir_all(&local).unwrap();
        let legacy = local
            .join(WINDOWS_LEGACY_CDP_DIR)
            .join(WINDOWS_LEGACY_CDP_EXE);
        fs::write(&pointer, legacy.to_string_lossy().as_bytes()).unwrap();
        let path = resolve_windows_cdp_runtime_path(&local, &pointer);
        assert!(is_windows_bland_runtime_exe(&path, &local));
        assert_bland_leaves(&path);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn migrates_legacy_discord_quest_helper_dir() {
        let root = unique_root();
        let local = root.join("Local");
        let old_dir = local.join(WINDOWS_LEGACY_CDP_DIR);
        fs::create_dir_all(&old_dir).unwrap();
        fs::write(old_dir.join(WINDOWS_LEGACY_CDP_EXE), b"old").unwrap();
        let new_target = allocate_windows_bland_runtime_exe(&local);
        migrate_legacy_windows_cdp_launcher_at(&local, &new_target);
        assert!(!old_dir.exists());
        assert!(new_target.is_file());
        assert_eq!(fs::read(&new_target).unwrap(), b"old");
        assert_bland_leaves(&new_target);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn migrate_does_not_overwrite_existing_target() {
        let root = unique_root();
        let local = root.join("Local");
        let old_dir = local.join(WINDOWS_LEGACY_CDP_DIR);
        fs::create_dir_all(&old_dir).unwrap();
        fs::write(old_dir.join(WINDOWS_LEGACY_CDP_EXE), b"old").unwrap();
        let new_target = allocate_windows_bland_runtime_exe(&local);
        fs::create_dir_all(new_target.parent().unwrap()).unwrap();
        fs::write(&new_target, b"new").unwrap();
        migrate_legacy_windows_cdp_launcher_at(&local, &new_target);
        assert!(!old_dir.exists());
        assert_eq!(fs::read(&new_target).unwrap(), b"new");
        let _ = fs::remove_dir_all(&root);
    }
}

/// Deterministic tests for the shared coordination invariant:
/// a proxy settings transaction and a login publication can never interleave,
/// and the published client always reflects the current committed policy.
#[cfg(test)]
mod proxy_client_coordination_tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct InMemoryCredentials {
        entries: Mutex<HashMap<String, String>>,
    }

    impl proxy_settings::CredentialStore for InMemoryCredentials {
        fn store(
            &self,
            reference: &str,
            credentials: &proxy_settings::ProxyCredentials,
        ) -> Result<(), proxy_settings::CredentialError> {
            let secret = credentials.to_stored_json();
            self.entries
                .lock()
                .unwrap()
                .insert(reference.to_string(), secret.to_string());
            Ok(())
        }

        fn load(
            &self,
            reference: &str,
        ) -> Result<Option<proxy_settings::ProxyCredentials>, proxy_settings::CredentialError>
        {
            match self.entries.lock().unwrap().get(reference).cloned() {
                Some(secret) => Ok(Some(proxy_settings::ProxyCredentials::from_stored_json(
                    &secret,
                )?)),
                None => Ok(None),
            }
        }

        fn delete(&self, reference: &str) -> Result<(), proxy_settings::CredentialError> {
            self.entries.lock().unwrap().remove(reference);
            Ok(())
        }
    }

    fn temp_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "dqh-coord-{label}-{}.json",
            uuid::Uuid::new_v4().simple()
        ))
    }

    fn input(mode: ProxyMode, endpoint: Option<&str>) -> ProxySettingsInput {
        ProxySettingsInput {
            mode,
            endpoint: endpoint.map(str::to_string),
            no_proxy: None,
            username: None,
            password: None,
        }
    }

    fn runtime_at(path: &std::path::Path) -> ProxyRuntime {
        ProxyRuntime::new(Arc::new(InMemoryCredentials::default()), path.to_path_buf())
    }

    fn user(id: &str, name: &str) -> DiscordUser {
        DiscordUser {
            id: id.to_string(),
            username: name.to_string(),
            discriminator: "0".to_string(),
            avatar: None,
            global_name: Some(format!("{name} Display")),
            premium_type: None,
        }
    }

    /// A per-account identity carrying a distinguishable `os`, built without any
    /// global manager.
    fn identity_with_os(os: &str) -> SuperPropertiesHandle {
        use base64::Engine as _;
        let handle = SuperPropertiesHandle::new();
        let props = super_properties::SuperProperties {
            os: os.to_string(),
            ..Default::default()
        };
        let json = serde_json::to_string(&props).unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&json);
        handle.set_from_cdp(&encoded, &serde_json::to_value(&props).unwrap());
        handle
    }

    fn publish_request(
        account: DiscordUser,
        cdp_port: u16,
        used_at_ms: u64,
        token: &str,
        identity: SuperPropertiesHandle,
    ) -> PublishAccountRequest {
        PublishAccountRequest {
            id: AccountId::from_user(&account).unwrap(),
            user: account,
            cdp_port: Some(cdp_port),
            used_at_ms,
            token: token.to_string(),
            identity,
        }
    }

    fn validated_login(
        account: DiscordUser,
        token: &str,
        identity: SuperPropertiesHandle,
    ) -> ValidatedCdpLogin {
        ValidatedCdpLogin {
            id: AccountId::from_user(&account).unwrap(),
            user: account,
            token: token.to_string(),
            identity,
        }
    }

    fn seed_online_account(
        registry: &AccountRegistry,
        runtime: &ProxyRuntime,
        resources: &ResourceCoordinator,
        account: DiscordUser,
        port: u16,
        token: &str,
        identity: SuperPropertiesHandle,
    ) {
        coordinated_publish_account(
            registry,
            runtime,
            resources,
            publish_request(account, port, 1, token, identity),
        )
        .unwrap();
    }

    // Login's CDP/network work captured an old (System) policy, then a settings
    // change commits, then login publishes. The coordinated publish re-resolves,
    // so the long-lived client is Custom, never the stale System build.
    #[test]
    fn login_publish_re_resolves_policy_committed_during_login() {
        let proxy_path = temp_path("publish");
        let registry_path = temp_path("publish-accounts");
        let runtime = runtime_at(&proxy_path);
        let registry = Arc::new(AccountRegistry::new(registry_path.clone()));
        let resources = ResourceCoordinator::new();

        // A login already built this stale System client before the update.
        let stale =
            DiscordApiClient::new_with_proxy("test-token".to_string(), ProxyConfiguration::System)
                .unwrap();
        assert_eq!(stale.proxy_configuration().mode(), ProxyMode::System);

        // Concurrent settings transaction commits Custom.
        coordinated_proxy_set(
            &registry,
            &runtime,
            input(ProxyMode::Custom, Some("http://127.0.0.1:8080")),
        )
        .unwrap();

        // Publish must not use the stale build.
        let alice = user("111111111111111111", "alice");
        coordinated_publish_account(
            &registry,
            &runtime,
            &resources,
            PublishAccountRequest {
                id: AccountId::from_user(&alice).unwrap(),
                user: alice,
                cdp_port: Some(9223),
                used_at_ms: 1,
                token: "test-token".to_string(),
                identity: SuperPropertiesHandle::new(),
            },
        )
        .unwrap();

        let published = registry
            .active_runtime()
            .and_then(|account| account.client())
            .expect("client published");
        assert_eq!(published.proxy_configuration().mode(), ProxyMode::Custom);
        assert_eq!(runtime.resolve_current().unwrap().mode(), ProxyMode::Custom);
        assert!(Arc::ptr_eq(
            &registry.active_runtime().unwrap().publication_gate(),
            &registry.coordination_gate()
        ));

        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&registry_path);
    }

    // The account-consistency validation client is also built from a policy
    // snapshot taken under the gate, so it cannot observe a torn policy.
    #[test]
    fn consistency_validation_client_uses_current_policy() {
        let proxy_path = temp_path("consistency");
        let registry_path = temp_path("consistency-accounts");
        let runtime = runtime_at(&proxy_path);
        let registry = Arc::new(AccountRegistry::new(registry_path.clone()));

        coordinated_proxy_set(&registry, &runtime, input(ProxyMode::Direct, None)).unwrap();

        let validation = coordinated_build_client(
            &registry,
            &runtime,
            &AccountId::parse("111111111111111111").unwrap(),
            "test-token".to_string(),
            SuperPropertiesHandle::new(),
        )
        .unwrap();
        assert_eq!(validation.proxy_configuration().mode(), ProxyMode::Direct);

        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&registry_path);
    }

    // Under real thread concurrency, the active account's published client policy
    // must always match the final committed policy, even while activation races
    // the proxy update.
    #[test]
    fn concurrent_policy_updates_and_account_publishes_keep_client_current() {
        let proxy_path = temp_path("race");
        let registry_path = temp_path("race-accounts");
        let runtime = Arc::new(runtime_at(&proxy_path));
        let registry = Arc::new(AccountRegistry::new(registry_path.clone()));
        let resources = Arc::new(ResourceCoordinator::new());

        // Seed an active account with an initial System client.
        let alice = user("111111111111111111", "alice");
        let alice_id = AccountId::from_user(&alice).unwrap();
        let account = registry.ensure_runtime(&alice).unwrap();
        account.publish_client(Some(
            DiscordApiClient::new_with_proxy("test-token".to_string(), ProxyConfiguration::System)
                .unwrap(),
        ));
        registry.activate(alice_id, account.profile()).unwrap();

        std::thread::scope(|scope| {
            for index in 0..8u32 {
                let registry = Arc::clone(&registry);
                let runtime = Arc::clone(&runtime);
                let resources = Arc::clone(&resources);
                scope.spawn(move || {
                    if index % 2 == 0 {
                        let endpoint = format!("http://127.0.0.1:{}", 9100 + index);
                        let _ = coordinated_proxy_set(
                            &registry,
                            &runtime,
                            input(ProxyMode::Custom, Some(&endpoint)),
                        );
                    } else {
                        // Alternate accounts so activation also races the update.
                        // Each account uses its own CDP port: Phase 6.3A binds a
                        // port to at most one account, so the race must stay
                        // about policy/activation, not port ownership.
                        let (id, name, port) = if index % 4 == 1 {
                            ("111111111111111111", "alice", 9223)
                        } else {
                            ("222222222222222222", "bob", 9333)
                        };
                        let account = user(id, name);
                        let _ = coordinated_publish_account(
                            &registry,
                            &runtime,
                            resources.as_ref(),
                            PublishAccountRequest {
                                id: AccountId::from_user(&account).unwrap(),
                                user: account,
                                cdp_port: Some(port),
                                used_at_ms: u64::from(index),
                                token: "test-token".to_string(),
                                identity: SuperPropertiesHandle::new(),
                            },
                        );
                    }
                });
            }
        });

        let active = registry.active_runtime().expect("an account is active");
        let published = active.client().expect("client present");
        let current = runtime.resolve_current().unwrap();
        assert_eq!(published.proxy_configuration().mode(), ProxyMode::Custom);
        assert_eq!(published.proxy_configuration().mode(), current.mode());

        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&registry_path);
    }

    // The active-client helper keeps the legacy single-account error semantics.
    #[test]
    fn require_active_client_preserves_not_logged_in_semantics() {
        assert!(matches!(
            require_active_client(None),
            Err(ref message) if message == "Not logged in"
        ));

        let client =
            DiscordApiClient::new_with_proxy("test-token".to_string(), ProxyConfiguration::Direct)
                .unwrap();
        let resolved = require_active_client(Some(client)).expect("client present");
        assert_eq!(resolved.proxy_configuration().mode(), ProxyMode::Direct);
    }

    // Registry active lookup reflects activation without mutating the active
    // account's online state.
    #[test]
    fn active_lookup_tracks_activation() {
        let registry_path = temp_path("active-lookup");
        let registry = AccountRegistry::new(registry_path.clone());
        assert!(registry.active_runtime().is_none());

        let alice = user("111111111111111111", "alice");
        let account = registry.ensure_runtime(&alice).unwrap();
        account.publish_client(Some(
            DiscordApiClient::new_with_proxy("test-token".to_string(), ProxyConfiguration::Direct)
                .unwrap(),
        ));
        registry
            .activate(AccountId::from_user(&alice).unwrap(), account.profile())
            .unwrap();

        let active = registry.active_runtime().expect("active runtime");
        assert!(active.has_client());
        assert_eq!(active.id().as_str(), "111111111111111111");

        let _ = std::fs::remove_file(&registry_path);
    }

    // Hazard A: the account-id accessor uses the same unauthenticated error class
    // as the other active accessors.
    #[test]
    fn require_active_account_id_preserves_not_logged_in_semantics() {
        assert!(matches!(
            require_active_account_id(None),
            Err(ref message) if message == "Not logged in"
        ));

        let registry_path = temp_path("require-id");
        let registry = AccountRegistry::new(registry_path.clone());
        let alice = user("111111111111111111", "alice");
        let runtime = registry.ensure_runtime(&alice).unwrap();
        assert_eq!(
            require_active_account_id(Some(runtime)).unwrap(),
            "111111111111111111"
        );
        let _ = std::fs::remove_file(&registry_path);
    }

    // P1-2: the first login must persist the *new* active id (not `null`).
    #[test]
    fn first_login_persists_the_new_active_account() {
        let proxy_path = temp_path("first-login-proxy");
        let registry_path = temp_path("first-login-accounts");
        let runtime = runtime_at(&proxy_path);
        let registry = AccountRegistry::new(registry_path.clone());
        let resources = ResourceCoordinator::new();

        let alice = user("111111111111111111", "alice");
        let alice_id = AccountId::from_user(&alice).unwrap();
        coordinated_publish_account(
            &registry,
            &runtime,
            &resources,
            PublishAccountRequest {
                id: alice_id.clone(),
                user: alice,
                cdp_port: Some(9223),
                used_at_ms: 7,
                token: "test-token".to_string(),
                identity: SuperPropertiesHandle::new(),
            },
        )
        .unwrap();

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&registry_path).unwrap()).unwrap();
        assert_eq!(value["active"], "111111111111111111");
        assert_eq!(value["accounts"].as_array().unwrap().len(), 1);
        assert_eq!(registry.active_id().unwrap(), alice_id);
        assert!(registry.active_runtime().unwrap().has_client());

        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&registry_path);
    }

    #[test]
    fn add_known_online_account_preserves_all_existing_account_state() {
        let proxy_path = temp_path("add-known-online-proxy");
        let registry_path = temp_path("add-known-online-accounts");
        let runtime = runtime_at(&proxy_path);
        let registry = AccountRegistry::new(registry_path.clone());
        let resources = ResourceCoordinator::new();
        let alice = user("111111111111111111", "alice");
        let bob = user("222222222222222222", "bob");
        let alice_identity = identity_with_os("alice-original");
        let bob_identity = identity_with_os("bob-active");

        coordinated_publish_account(
            &registry,
            &runtime,
            &resources,
            publish_request(
                alice.clone(),
                9223,
                10,
                "alice-original-token",
                alice_identity.clone(),
            ),
        )
        .unwrap();
        coordinated_publish_account(
            &registry,
            &runtime,
            &resources,
            publish_request(bob.clone(), 9333, 20, "bob-token", bob_identity.clone()),
        )
        .unwrap();

        let alice_id = AccountId::from_user(&alice).unwrap();
        let bob_id = AccountId::from_user(&bob).unwrap();
        let alice_runtime = registry.runtime(&alice_id).unwrap();
        let bob_runtime = registry.runtime(&bob_id).unwrap();
        let before_profile = alice_runtime.profile();
        let before_authenticated_user = alice_runtime.authenticated_user().unwrap();
        let before_client = alice_runtime.client().unwrap();
        let persisted_before = std::fs::read(&registry_path).unwrap();

        // Simulate a different capture presentation/token/identity for A. Add
        // must return the captured user but leave the known runtime untouched.
        let captured_alice = user("111111111111111111", "alice-captured-again");
        let result = coordinated_add_account(
            &registry,
            &runtime,
            &resources,
            publish_request(
                captured_alice.clone(),
                9444,
                999,
                "alice-new-token",
                identity_with_os("alice-new-identity"),
            ),
        )
        .unwrap();

        assert!(result.already_known);
        assert_eq!(result.user.username, captured_alice.username);
        assert_eq!(registry.active_id().unwrap(), bob_id);
        assert_eq!(alice_runtime.profile(), before_profile);
        assert_eq!(
            alice_runtime.authenticated_user().unwrap().username,
            before_authenticated_user.username
        );
        assert_eq!(alice_runtime.profile().last_cdp_port, Some(9223));
        assert!(alice_runtime.super_properties().is_same(&alice_identity));
        let after_client = alice_runtime.client().unwrap();
        assert_eq!(after_client.get_token(), before_client.get_token());
        assert!(after_client.super_properties().is_same(&alice_identity));
        assert!(bob_runtime
            .client()
            .unwrap()
            .super_properties()
            .is_same(&bob_identity));
        assert_eq!(std::fs::read(&registry_path).unwrap(), persisted_before);

        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&registry_path);
    }

    #[test]
    fn add_known_offline_profile_does_not_authenticate_or_activate_it() {
        let proxy_path = temp_path("add-known-offline-proxy");
        let registry_path = temp_path("add-known-offline-accounts");
        let seed_registry = AccountRegistry::new(registry_path.clone());
        let alice = user("111111111111111111", "alice-saved");
        let alice_id = AccountId::from_user(&alice).unwrap();
        let offline = seed_registry.ensure_runtime(&alice).unwrap();
        let mut saved_profile = offline.profile();
        saved_profile.last_cdp_port = Some(9223);
        saved_profile.last_used_at_ms = Some(123);
        offline.set_profile(saved_profile.clone());
        seed_registry.persist().unwrap();
        assert!(seed_registry.active_id().is_none());

        let registry = AccountRegistry::new(registry_path.clone());
        registry.load_from_disk().unwrap();
        let runtime = runtime_at(&proxy_path);
        let resources = ResourceCoordinator::new();
        let persisted_before = std::fs::read(&registry_path).unwrap();

        let result = coordinated_add_account(
            &registry,
            &runtime,
            &resources,
            publish_request(
                user("111111111111111111", "alice-captured"),
                9444,
                456,
                "captured-token",
                identity_with_os("captured-identity"),
            ),
        )
        .unwrap();

        assert!(result.already_known);
        assert_eq!(registry.active_id(), None);
        let loaded = registry.runtime(&alice_id).unwrap();
        assert_eq!(loaded.profile(), saved_profile);
        assert!(!loaded.has_client());
        assert!(loaded.authenticated_user().is_none());
        assert_eq!(std::fs::read(&registry_path).unwrap(), persisted_before);

        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&registry_path);
    }

    #[test]
    fn add_unknown_account_publishes_once_and_saves_selected_port() {
        let proxy_path = temp_path("add-unknown-proxy");
        let registry_path = temp_path("add-unknown-accounts");
        let runtime = runtime_at(&proxy_path);
        let registry = AccountRegistry::new(registry_path.clone());
        let resources = ResourceCoordinator::new();
        let bob = user("222222222222222222", "bob-added");
        let identity = identity_with_os("bob-added");

        let result = coordinated_add_account(
            &registry,
            &runtime,
            &resources,
            publish_request(bob.clone(), 9333, 42, "bob-added-token", identity.clone()),
        )
        .unwrap();

        assert!(!result.already_known);
        assert_eq!(result.user.username, bob.username);
        let id = AccountId::from_user(&bob).unwrap();
        let account = registry.runtime(&id).expect("one runtime was published");
        assert_eq!(registry.active_id(), Some(id.clone()));
        assert_eq!(account.profile().last_cdp_port, Some(9333));
        assert_eq!(account.profile().last_used_at_ms, Some(42));
        assert!(account.has_client());
        assert!(account
            .client()
            .unwrap()
            .super_properties()
            .is_same(&identity));

        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&registry_path).unwrap()).unwrap();
        assert_eq!(persisted["accounts"].as_array().unwrap().len(), 1);
        assert_eq!(persisted["accounts"][0]["lastCdpPort"], 9333);
        assert_eq!(persisted["active"], id.as_str());

        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&registry_path);
    }

    #[test]
    fn legacy_publish_of_known_account_refreshes_and_activates_it() {
        let proxy_path = temp_path("legacy-refresh-proxy");
        let registry_path = temp_path("legacy-refresh-accounts");
        let runtime = runtime_at(&proxy_path);
        let registry = AccountRegistry::new(registry_path.clone());
        let resources = ResourceCoordinator::new();
        let alice = user("111111111111111111", "alice");
        let bob = user("222222222222222222", "bob");

        coordinated_publish_account(
            &registry,
            &runtime,
            &resources,
            publish_request(
                alice.clone(),
                9223,
                10,
                "alice-old-token",
                identity_with_os("alice-old"),
            ),
        )
        .unwrap();
        coordinated_publish_account(
            &registry,
            &runtime,
            &resources,
            publish_request(bob.clone(), 9333, 20, "bob-token", identity_with_os("bob")),
        )
        .unwrap();

        let refreshed_alice = user("111111111111111111", "alice-refreshed");
        let refreshed_identity = identity_with_os("alice-refreshed");
        coordinated_publish_account(
            &registry,
            &runtime,
            &resources,
            publish_request(
                refreshed_alice.clone(),
                9444,
                30,
                "alice-refreshed-token",
                refreshed_identity.clone(),
            ),
        )
        .unwrap();

        let alice_id = AccountId::from_user(&refreshed_alice).unwrap();
        let account = registry.runtime(&alice_id).unwrap();
        assert_eq!(registry.active_id(), Some(alice_id));
        assert_eq!(account.profile().username, "alice-refreshed");
        assert_eq!(account.profile().last_cdp_port, Some(9444));
        assert_eq!(account.profile().last_used_at_ms, Some(30));
        assert_eq!(
            account.authenticated_user().unwrap().username,
            "alice-refreshed"
        );
        assert_eq!(
            account.client().unwrap().get_token(),
            "alice-refreshed-token"
        );
        assert!(account.super_properties().is_same(&refreshed_identity));

        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&registry_path);
    }

    #[test]
    fn concurrent_adds_for_one_id_publish_exactly_once() {
        let proxy_path = temp_path("add-race-proxy");
        let registry_path = temp_path("add-race-accounts");
        let runtime = Arc::new(runtime_at(&proxy_path));
        let registry = Arc::new(AccountRegistry::new(registry_path.clone()));
        let resources = Arc::new(ResourceCoordinator::new());
        let barrier = Arc::new(std::sync::Barrier::new(3));

        let first_user = user("222222222222222222", "bob-first");
        let second_user = user("222222222222222222", "bob-second");
        let first = {
            let registry = Arc::clone(&registry);
            let runtime = Arc::clone(&runtime);
            let resources = Arc::clone(&resources);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                coordinated_add_account(
                    &registry,
                    &runtime,
                    &resources,
                    publish_request(
                        first_user,
                        9333,
                        1,
                        "first-token",
                        identity_with_os("first"),
                    ),
                )
                .unwrap()
            })
        };
        let second = {
            let registry = Arc::clone(&registry);
            let runtime = Arc::clone(&runtime);
            let resources = Arc::clone(&resources);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                coordinated_add_account(
                    &registry,
                    &runtime,
                    &resources,
                    publish_request(
                        second_user,
                        9444,
                        2,
                        "second-token",
                        identity_with_os("second"),
                    ),
                )
                .unwrap()
            })
        };

        barrier.wait();
        let results = [first.join().unwrap(), second.join().unwrap()];
        assert_eq!(
            results
                .iter()
                .filter(|result| !result.already_known)
                .count(),
            1
        );
        assert_eq!(
            results.iter().filter(|result| result.already_known).count(),
            1
        );

        let id = AccountId::parse("222222222222222222").unwrap();
        let account = registry.runtime(&id).expect("the winner was published");
        assert_eq!(registry.active_id(), Some(id));
        let port = account.profile().last_cdp_port.unwrap();
        assert!(port == 9333 || port == 9444);
        let (expected_name, expected_token) = if port == 9333 {
            ("bob-first", "first-token")
        } else {
            ("bob-second", "second-token")
        };
        assert_eq!(account.profile().username, expected_name);
        assert_eq!(account.client().unwrap().get_token(), expected_token);
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&registry_path).unwrap()).unwrap();
        assert_eq!(persisted["accounts"].as_array().unwrap().len(), 1);

        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&registry_path);
    }

    #[test]
    fn add_races_with_final_remove_as_one_serial_registry_decision() {
        let proxy_path = temp_path("add-remove-race-proxy");
        let registry_path = temp_path("add-remove-race-accounts");
        let runtime = Arc::new(runtime_at(&proxy_path));
        let registry = Arc::new(AccountRegistry::new(registry_path.clone()));
        let resources = Arc::new(ResourceCoordinator::new());
        let alice = user("111111111111111111", "alice");
        let id = AccountId::from_user(&alice).unwrap();
        coordinated_publish_account(
            &registry,
            &runtime,
            &resources,
            publish_request(
                alice.clone(),
                9223,
                1,
                "alice-token",
                identity_with_os("alice"),
            ),
        )
        .unwrap();

        let barrier = Arc::new(std::sync::Barrier::new(3));
        let add = {
            let registry = Arc::clone(&registry);
            let runtime = Arc::clone(&runtime);
            let resources = Arc::clone(&resources);
            let barrier = Arc::clone(&barrier);
            let alice = alice.clone();
            std::thread::spawn(move || {
                barrier.wait();
                coordinated_add_account(
                    &registry,
                    &runtime,
                    &resources,
                    publish_request(alice, 9444, 2, "alice-add-token", identity_with_os("add")),
                )
                .unwrap()
            })
        };
        let remove = {
            let registry = Arc::clone(&registry);
            let barrier = Arc::clone(&barrier);
            let id = id.clone();
            std::thread::spawn(move || {
                barrier.wait();
                coordinated_remove_account(&registry, &id).unwrap()
            })
        };

        barrier.wait();
        let result = add.join().unwrap();
        assert!(remove.join().unwrap(), "seeded account was removed");
        if result.already_known {
            // Add linearized first; final removal then removed the known profile.
            assert!(registry.runtime(&id).is_none());
            assert!(registry.active_id().is_none());
        } else {
            // Removal linearized first; Add then performed the one new publish.
            let account = registry.runtime(&id).expect("Add published after remove");
            assert_eq!(registry.active_id(), Some(id.clone()));
            assert_eq!(account.profile().last_cdp_port, Some(9444));
            assert!(account.has_client());
        }

        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&registry_path);
    }

    #[test]
    fn add_races_with_activation_without_tearing_known_account_state() {
        let proxy_path = temp_path("add-activate-race-proxy");
        let registry_path = temp_path("add-activate-race-accounts");
        let runtime = Arc::new(runtime_at(&proxy_path));
        let registry = Arc::new(AccountRegistry::new(registry_path.clone()));
        let resources = Arc::new(ResourceCoordinator::new());
        let alice = user("111111111111111111", "alice");
        let id = AccountId::from_user(&alice).unwrap();
        coordinated_publish_account(
            &registry,
            &runtime,
            &resources,
            publish_request(
                alice.clone(),
                9223,
                1,
                "alice-token",
                identity_with_os("alice"),
            ),
        )
        .unwrap();
        let account = registry.runtime(&id).unwrap();
        let before_profile = account.profile();
        let before_token = account.client().unwrap().get_token().to_string();

        let barrier = Arc::new(std::sync::Barrier::new(3));
        let add = {
            let registry = Arc::clone(&registry);
            let runtime = Arc::clone(&runtime);
            let resources = Arc::clone(&resources);
            let barrier = Arc::clone(&barrier);
            let alice = alice.clone();
            std::thread::spawn(move || {
                barrier.wait();
                coordinated_add_account(
                    &registry,
                    &runtime,
                    &resources,
                    publish_request(alice, 9444, 2, "ignored-token", identity_with_os("ignored")),
                )
                .unwrap()
            })
        };
        let activate = {
            let registry = Arc::clone(&registry);
            let barrier = Arc::clone(&barrier);
            let id = id.clone();
            std::thread::spawn(move || {
                barrier.wait();
                coordinated_activate_account(&registry, &id).unwrap()
            })
        };

        barrier.wait();
        assert!(add.join().unwrap().already_known);
        assert_eq!(activate.join().unwrap().id(), &id);
        assert_eq!(registry.active_id(), Some(id));
        assert_eq!(account.profile(), before_profile);
        assert_eq!(account.client().unwrap().get_token(), before_token.as_str());

        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&registry_path);
    }

    #[test]
    fn cdp_identity_preview_returns_only_user_and_leaves_registry_unchanged() {
        let proxy_path = temp_path("identity-preview-proxy");
        let registry_path = temp_path("identity-preview-accounts");
        let runtime = runtime_at(&proxy_path);
        let registry = AccountRegistry::new(registry_path.clone());
        let resources = ResourceCoordinator::new();
        let alice = user("111111111111111111", "alice");
        let alice_identity = identity_with_os("alice-preview-baseline");
        seed_online_account(
            &registry,
            &runtime,
            &resources,
            alice.clone(),
            9223,
            "alice-token",
            alice_identity.clone(),
        );

        let id = AccountId::from_user(&alice).unwrap();
        let account = registry.runtime(&id).unwrap();
        let profile_before = account.profile();
        let client_before = account.client().unwrap();
        let persisted_before = std::fs::read(&registry_path).unwrap();
        let preview = cdp_identity_preview_from_validated(
            validated_login(
                user("222222222222222222", "bob-preview"),
                "captured-token",
                identity_with_os("captured-identity"),
            ),
            9444,
        );

        assert_eq!(preview.port, 9444);
        assert_eq!(preview.user.id, "222222222222222222");
        assert_eq!(registry.active_id(), Some(id));
        assert_eq!(account.profile(), profile_before);
        assert_eq!(
            account.client().unwrap().get_token(),
            client_before.get_token()
        );
        assert!(account.super_properties().is_same(&alice_identity));
        assert_eq!(std::fs::read(&registry_path).unwrap(), persisted_before);

        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&registry_path);
    }

    #[test]
    fn previewed_add_identity_changed_before_confirm_is_a_noop_without_complete_progress() {
        let proxy_path = temp_path("identity-change-proxy");
        let registry_path = temp_path("identity-change-accounts");
        let runtime = runtime_at(&proxy_path);
        let registry = AccountRegistry::new(registry_path.clone());
        let resources = ResourceCoordinator::new();
        let alice = user("111111111111111111", "alice");
        let alice_identity = identity_with_os("alice-baseline");
        seed_online_account(
            &registry,
            &runtime,
            &resources,
            alice.clone(),
            9223,
            "alice-token",
            alice_identity.clone(),
        );

        // The preview authorizes Alice, but the selected client now authenticates
        // as Bob. Confirmation must return the live identity without publishing.
        let preview = cdp_identity_preview_from_validated(
            validated_login(
                alice.clone(),
                "alice-preview-token",
                identity_with_os("alice-preview-identity"),
            ),
            9333,
        );
        let expected_id = AccountId::from_user(&preview.user).unwrap();
        let active_before = registry.active_id();
        let profile_before = registry.profiles();
        let persisted_before = std::fs::read(&registry_path).unwrap();
        let result = coordinated_confirm_add_cdp_account(
            &registry,
            &runtime,
            &resources,
            &expected_id,
            preview.port,
            validated_login(
                user("222222222222222222", "bob-live"),
                "bob-live-token",
                identity_with_os("bob-live-identity"),
            ),
        )
        .unwrap();

        assert_eq!(result.status, CdpConfirmStatus::IdentityChanged);
        assert_eq!(result.user.id, "222222222222222222");
        assert_eq!(result.port, 9333);
        let mut progress = Vec::new();
        report_cdp_confirm_completion(result.status, |item| progress.push(item.phase));
        assert!(
            progress.is_empty(),
            "identity mismatch must not report Complete"
        );
        assert_eq!(registry.active_id(), active_before);
        assert_eq!(registry.profiles(), profile_before);
        assert_eq!(
            registry
                .runtime(&expected_id)
                .unwrap()
                .profile()
                .last_cdp_port,
            Some(9223)
        );
        assert!(registry
            .runtime(&expected_id)
            .unwrap()
            .super_properties()
            .is_same(&alice_identity));
        assert_eq!(std::fs::read(&registry_path).unwrap(), persisted_before);

        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&registry_path);
    }

    #[test]
    fn confirmed_duplicate_add_returns_already_saved_without_mutation() {
        let proxy_path = temp_path("confirmed-duplicate-proxy");
        let registry_path = temp_path("confirmed-duplicate-accounts");
        let runtime = runtime_at(&proxy_path);
        let registry = AccountRegistry::new(registry_path.clone());
        let resources = ResourceCoordinator::new();
        let alice = user("111111111111111111", "alice");
        let bob = user("222222222222222222", "bob-active");
        let alice_identity = identity_with_os("alice-original");
        seed_online_account(
            &registry,
            &runtime,
            &resources,
            alice.clone(),
            9223,
            "alice-original-token",
            alice_identity.clone(),
        );
        seed_online_account(
            &registry,
            &runtime,
            &resources,
            bob.clone(),
            9333,
            "bob-token",
            identity_with_os("bob"),
        );

        let alice_id = AccountId::from_user(&alice).unwrap();
        let alice_runtime = registry.runtime(&alice_id).unwrap();
        let alice_profile = alice_runtime.profile();
        let alice_token = alice_runtime.client().unwrap().get_token().to_string();
        let active_before = registry.active_id();
        let profiles_before = registry.profiles();
        let persisted_before = std::fs::read(&registry_path).unwrap();

        let result = coordinated_confirm_add_cdp_account(
            &registry,
            &runtime,
            &resources,
            &alice_id,
            9444,
            validated_login(
                user("111111111111111111", "alice-captured-again"),
                "alice-new-token",
                identity_with_os("alice-new-identity"),
            ),
        )
        .unwrap();

        assert_eq!(result.status, CdpConfirmStatus::AlreadySaved);
        assert_eq!(result.user.username, "alice-captured-again");
        assert_eq!(registry.active_id(), active_before);
        assert_eq!(registry.profiles(), profiles_before);
        assert_eq!(alice_runtime.profile(), alice_profile);
        assert_eq!(
            alice_runtime.client().unwrap().get_token(),
            alice_token.as_str()
        );
        assert!(alice_runtime.super_properties().is_same(&alice_identity));
        assert_eq!(std::fs::read(&registry_path).unwrap(), persisted_before);

        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&registry_path);
    }

    #[test]
    fn confirmed_new_add_publishes_expected_identity_and_selected_port() {
        let proxy_path = temp_path("confirmed-add-proxy");
        let registry_path = temp_path("confirmed-add-accounts");
        let runtime = runtime_at(&proxy_path);
        let registry = AccountRegistry::new(registry_path.clone());
        let resources = ResourceCoordinator::new();
        let bob = user("222222222222222222", "bob");
        let bob_id = AccountId::from_user(&bob).unwrap();
        let bob_identity = identity_with_os("bob-confirmed");

        let result = coordinated_confirm_add_cdp_account(
            &registry,
            &runtime,
            &resources,
            &bob_id,
            9444,
            validated_login(bob.clone(), "bob-token", bob_identity.clone()),
        )
        .unwrap();

        assert_eq!(result.status, CdpConfirmStatus::Added);
        assert_eq!(result.user.id, bob.id);
        assert_eq!(result.port, 9444);
        let account = registry
            .runtime(&bob_id)
            .expect("confirmed Add publishes B");
        assert_eq!(registry.active_id(), Some(bob_id.clone()));
        assert_eq!(account.profile().last_cdp_port, Some(9444));
        assert_eq!(account.client().unwrap().get_token(), "bob-token");
        assert!(account.super_properties().is_same(&bob_identity));

        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&registry_path);
    }

    struct OfflineReconnectFixture {
        alice: DiscordUser,
        alice_id: AccountId,
        bob_id: AccountId,
        alice_identity: SuperPropertiesHandle,
    }

    fn seed_offline_beside_online_a(
        registry: &AccountRegistry,
        runtime: &ProxyRuntime,
        resources: &ResourceCoordinator,
    ) -> OfflineReconnectFixture {
        let alice = user("111111111111111111", "alice-online");
        let alice_identity = identity_with_os("alice-online");
        seed_online_account(
            registry,
            runtime,
            resources,
            alice.clone(),
            9223,
            "alice-online-token",
            alice_identity.clone(),
        );
        let bob = user("222222222222222222", "bob-offline");
        let bob_id = AccountId::from_user(&bob).unwrap();
        let bob_runtime = registry.ensure_runtime(&bob).unwrap();
        let mut profile = bob_runtime.profile();
        profile.last_cdp_port = Some(9444);
        profile.last_used_at_ms = Some(77);
        bob_runtime.set_profile(profile);
        registry.persist().unwrap();
        OfflineReconnectFixture {
            alice_id: AccountId::from_user(&alice).unwrap(),
            alice,
            bob_id,
            alice_identity,
        }
    }

    #[test]
    fn reconnect_offline_b_rejects_live_a_without_mutation() {
        let proxy_path = temp_path("reconnect-mismatch-proxy");
        let registry_path = temp_path("reconnect-mismatch-accounts");
        let runtime = runtime_at(&proxy_path);
        let registry = AccountRegistry::new(registry_path.clone());
        let resources = ResourceCoordinator::new();
        let fixture = seed_offline_beside_online_a(&registry, &runtime, &resources);
        let alice_runtime = registry.runtime(&fixture.alice_id).unwrap();
        let bob_runtime = registry.runtime(&fixture.bob_id).unwrap();
        let alice_profile = alice_runtime.profile();
        let bob_profile = bob_runtime.profile();
        let active_before = registry.active_id();
        let persisted_before = std::fs::read(&registry_path).unwrap();

        require_known_reconnect_account(&registry, &fixture.bob_id).unwrap();
        let result = coordinated_reconnect_cdp_account(
            &registry,
            &runtime,
            &resources,
            &fixture.bob_id,
            9333,
            validated_login(
                fixture.alice.clone(),
                "alice-live-token",
                identity_with_os("wrong-a"),
            ),
        )
        .unwrap();

        assert_eq!(result.status, CdpConfirmStatus::IdentityChanged);
        assert_eq!(result.user.id, fixture.alice.id);
        assert_eq!(registry.active_id(), active_before);
        assert_eq!(alice_runtime.profile(), alice_profile);
        assert_eq!(bob_runtime.profile(), bob_profile);
        assert!(!bob_runtime.has_client());
        assert!(bob_runtime.authenticated_user().is_none());
        assert!(alice_runtime.has_client());
        assert!(alice_runtime
            .super_properties()
            .is_same(&fixture.alice_identity));
        assert_eq!(std::fs::read(&registry_path).unwrap(), persisted_before);

        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&registry_path);
    }

    #[test]
    fn reconnect_saved_b_with_matching_live_identity_publishes_b() {
        let proxy_path = temp_path("reconnect-match-proxy");
        let registry_path = temp_path("reconnect-match-accounts");
        let runtime = runtime_at(&proxy_path);
        let registry = AccountRegistry::new(registry_path.clone());
        let resources = ResourceCoordinator::new();
        let fixture = seed_offline_beside_online_a(&registry, &runtime, &resources);
        let alice_runtime = registry.runtime(&fixture.alice_id).unwrap();
        let bob_runtime = registry.runtime(&fixture.bob_id).unwrap();
        assert!(!bob_runtime.has_client());

        require_known_reconnect_account(&registry, &fixture.bob_id).unwrap();
        let refreshed_bob = user("222222222222222222", "bob-reconnected");
        let bob_identity = identity_with_os("bob-reconnected");
        let result = coordinated_reconnect_cdp_account(
            &registry,
            &runtime,
            &resources,
            &fixture.bob_id,
            9555,
            validated_login(
                refreshed_bob.clone(),
                "bob-live-token",
                bob_identity.clone(),
            ),
        )
        .unwrap();

        assert_eq!(result.status, CdpConfirmStatus::Reconnected);
        assert_eq!(result.user.username, refreshed_bob.username);
        assert_eq!(result.port, 9555);
        assert_eq!(registry.active_id(), Some(fixture.bob_id));
        assert_eq!(bob_runtime.profile().username, "bob-reconnected");
        assert_eq!(bob_runtime.profile().last_cdp_port, Some(9555));
        assert_eq!(bob_runtime.client().unwrap().get_token(), "bob-live-token");
        assert!(bob_runtime.super_properties().is_same(&bob_identity));
        assert!(
            alice_runtime.has_client(),
            "reconnecting B leaves A's runtime online"
        );

        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&registry_path).unwrap()).unwrap();
        assert_eq!(persisted["active"], "222222222222222222");
        let bob_profile = persisted["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|profile| profile["id"] == "222222222222222222")
            .unwrap();
        assert_eq!(bob_profile["lastCdpPort"], 9555);

        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&registry_path);
    }

    #[test]
    fn reconnect_unknown_account_fails_before_any_commit() {
        let proxy_path = temp_path("reconnect-unknown-proxy");
        let registry_path = temp_path("reconnect-unknown-accounts");
        let runtime = runtime_at(&proxy_path);
        let registry = AccountRegistry::new(registry_path.clone());
        let resources = ResourceCoordinator::new();
        let alice = user("111111111111111111", "alice");
        seed_online_account(
            &registry,
            &runtime,
            &resources,
            alice,
            9223,
            "alice-token",
            identity_with_os("alice"),
        );
        let unknown_id = AccountId::parse("222222222222222222").unwrap();
        let active_before = registry.active_id();
        let profiles_before = registry.profiles();
        let persisted_before = std::fs::read(&registry_path).unwrap();

        assert_eq!(
            require_known_reconnect_account(&registry, &unknown_id),
            Err("Unknown account.".to_string())
        );
        assert_eq!(registry.active_id(), active_before);
        assert_eq!(registry.profiles(), profiles_before);
        assert_eq!(std::fs::read(&registry_path).unwrap(), persisted_before);

        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&registry_path);
    }

    #[test]
    fn failed_confirm_add_save_leaves_registry_and_existing_bytes_unchanged() {
        let proxy_path = temp_path("confirm-save-proxy");
        let blocker = temp_path("confirm-save-blocker");
        let original = b"not a directory";
        std::fs::write(&blocker, original).unwrap();
        let registry_path = blocker.join("accounts.v1.json");
        let runtime = runtime_at(&proxy_path);
        let registry = AccountRegistry::new(registry_path);
        let resources = ResourceCoordinator::new();
        let bob = user("222222222222222222", "bob");
        let bob_id = AccountId::from_user(&bob).unwrap();

        assert!(coordinated_confirm_add_cdp_account(
            &registry,
            &runtime,
            &resources,
            &bob_id,
            9444,
            validated_login(bob, "bob-token", identity_with_os("bob")),
        )
        .is_err());

        assert!(registry.runtime(&bob_id).is_none());
        assert!(registry.active_id().is_none());
        assert_eq!(std::fs::read(&blocker).unwrap(), original);
        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&blocker);
    }

    #[tokio::test]
    async fn competing_direct_lease_prevents_capture_and_keeps_registry_unchanged() {
        let proxy_path = temp_path("identity-busy-proxy");
        let registry_path = temp_path("identity-busy-accounts");
        let runtime = runtime_at(&proxy_path);
        let registry = AccountRegistry::new(registry_path.clone());
        let resources = ResourceCoordinator::new();
        let alice = user("111111111111111111", "alice");
        seed_online_account(
            &registry,
            &runtime,
            &resources,
            alice.clone(),
            9223,
            "alice-token",
            identity_with_os("alice"),
        );
        let leases = CdpPortLeases::new();
        let _holder = leases.acquire_direct(9444).unwrap();
        let capture_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let capture_flag = Arc::clone(&capture_started);
        let captured_alice = alice.clone();
        let active_before = registry.active_id();
        let profiles_before = registry.profiles();
        let persisted_before = std::fs::read(&registry_path).unwrap();

        let result = capture_with_direct_cdp_lease(&leases, 9444, move || async move {
            capture_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(validated_login(
                captured_alice,
                "captured-token",
                identity_with_os("captured"),
            ))
        })
        .await;

        assert!(result.is_err());
        assert!(!capture_started.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(registry.active_id(), active_before);
        assert_eq!(registry.profiles(), profiles_before);
        assert_eq!(std::fs::read(&registry_path).unwrap(), persisted_before);

        drop(_holder);
        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&registry_path);
    }

    #[tokio::test]
    async fn cancelled_capture_releases_lease_without_changing_saved_state() {
        let proxy_path = temp_path("identity-cancel-proxy");
        let registry_path = temp_path("identity-cancel-accounts");
        let runtime = runtime_at(&proxy_path);
        let registry = AccountRegistry::new(registry_path.clone());
        let resources = ResourceCoordinator::new();
        let alice = user("111111111111111111", "alice");
        seed_online_account(
            &registry,
            &runtime,
            &resources,
            alice,
            9223,
            "alice-token",
            identity_with_os("alice"),
        );
        let persisted_before = std::fs::read(&registry_path).unwrap();
        let profiles_before = registry.profiles();
        let active_before = registry.active_id();
        let leases = Arc::new(CdpPortLeases::new());
        let task_leases = Arc::clone(&leases);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            capture_with_direct_cdp_lease(task_leases.as_ref(), 9444, || async move {
                let _ = started_tx.send(());
                std::future::pending::<Result<(), String>>().await
            })
            .await
        });

        started_rx.await.unwrap();
        assert!(leases.snapshot(9444).is_some());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());

        assert!(leases.snapshot(9444).is_none());
        assert_eq!(registry.active_id(), active_before);
        assert_eq!(registry.profiles(), profiles_before);
        assert_eq!(std::fs::read(&registry_path).unwrap(), persisted_before);

        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&registry_path);
    }

    // P1-2: a save failure must publish nothing and leave prior active/runtime
    // state unchanged.
    #[test]
    fn publish_save_failure_leaves_prior_state_unchanged() {
        let proxy_path = temp_path("save-fail-proxy");
        // A regular file where the accounts directory should be makes the save
        // fail deterministically on every platform.
        let blocker = temp_path("save-fail-blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let registry_path = blocker.join("accounts.v1.json");
        let runtime = runtime_at(&proxy_path);
        let registry = AccountRegistry::new(registry_path);
        let resources = ResourceCoordinator::new();

        // Prior state: alice active and online.
        let alice = user("111111111111111111", "alice");
        let alice_id = AccountId::from_user(&alice).unwrap();
        let alice_runtime = registry.ensure_runtime(&alice).unwrap();
        alice_runtime.publish_client(Some(
            DiscordApiClient::new_with_proxy("test-token".to_string(), ProxyConfiguration::Direct)
                .unwrap(),
        ));
        alice_runtime.mark_authenticated(&alice, Some(9223), 1);
        registry
            .activate(alice_id.clone(), alice_runtime.profile())
            .unwrap();

        let bob = user("222222222222222222", "bob");
        let bob_id = AccountId::from_user(&bob).unwrap();
        let result = coordinated_publish_account(
            &registry,
            &runtime,
            &resources,
            PublishAccountRequest {
                id: bob_id.clone(),
                user: bob,
                cdp_port: Some(9333),
                used_at_ms: 2,
                token: "test-token".to_string(),
                identity: SuperPropertiesHandle::new(),
            },
        );
        assert!(result.is_err());

        assert_eq!(registry.active_id().unwrap(), alice_id);
        assert!(alice_runtime.has_client());
        assert_eq!(
            alice_runtime.authenticated_user().unwrap().username,
            "alice"
        );
        // Bob was neither created nor published.
        assert!(registry.runtime(&bob_id).is_none());

        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&blocker);
    }

    // P1-1: a failed load blocks the login publish and preserves the file bytes.
    #[test]
    fn failed_load_blocks_login_publish_and_preserves_file() {
        let proxy_path = temp_path("readonly-proxy");
        let registry_path = temp_path("readonly-accounts");
        let original: &[u8] = b"{ not json";
        std::fs::write(&registry_path, original).unwrap();

        let registry = AccountRegistry::new(registry_path.clone());
        assert!(registry.load_from_disk().is_err());

        let runtime = runtime_at(&proxy_path);
        let resources = ResourceCoordinator::new();
        let alice = user("111111111111111111", "alice");
        let result = coordinated_publish_account(
            &registry,
            &runtime,
            &resources,
            PublishAccountRequest {
                id: AccountId::from_user(&alice).unwrap(),
                user: alice,
                cdp_port: None,
                used_at_ms: 1,
                token: "test-token".to_string(),
                identity: SuperPropertiesHandle::new(),
            },
        );
        assert!(result.is_err());

        assert_eq!(std::fs::read(&registry_path).unwrap(), original);
        assert!(registry.active_runtime().is_none());
        assert!(registry.persistence_error().is_some());

        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&registry_path);
    }

    // Phase 6.2: publishing two accounts wires each runtime and its published
    // client to that account's captured identity only. No identity is shared.
    #[test]
    fn published_accounts_keep_identity_isolated_per_runtime_and_client() {
        let proxy_path = temp_path("identity-publish-proxy");
        let registry_path = temp_path("identity-publish-accounts");
        let runtime = runtime_at(&proxy_path);
        let registry = AccountRegistry::new(registry_path.clone());
        let resources = ResourceCoordinator::new();

        let alice = user("111111111111111111", "alice");
        let bob = user("222222222222222222", "bob");
        let alice_identity = identity_with_os("alice-os");
        let bob_identity = identity_with_os("bob-os");

        coordinated_publish_account(
            &registry,
            &runtime,
            &resources,
            PublishAccountRequest {
                id: AccountId::from_user(&alice).unwrap(),
                user: alice.clone(),
                cdp_port: Some(9223),
                used_at_ms: 1,
                token: "token-alice".to_string(),
                identity: alice_identity.clone(),
            },
        )
        .unwrap();
        coordinated_publish_account(
            &registry,
            &runtime,
            &resources,
            PublishAccountRequest {
                id: AccountId::from_user(&bob).unwrap(),
                user: bob.clone(),
                cdp_port: Some(9333),
                used_at_ms: 2,
                token: "token-bob".to_string(),
                identity: bob_identity.clone(),
            },
        )
        .unwrap();

        let alice_runtime = registry
            .runtime(&AccountId::from_user(&alice).unwrap())
            .unwrap();
        let bob_runtime = registry
            .runtime(&AccountId::from_user(&bob).unwrap())
            .unwrap();

        assert!(alice_runtime.super_properties().is_same(&alice_identity));
        assert!(bob_runtime.super_properties().is_same(&bob_identity));
        assert!(!alice_runtime.super_properties().is_same(&bob_identity));

        let alice_client = alice_runtime.client().expect("alice client published");
        let bob_client = bob_runtime.client().expect("bob client published");
        assert!(alice_client.super_properties().is_same(&alice_identity));
        assert!(bob_client.super_properties().is_same(&bob_identity));
        assert!(!alice_client.super_properties().is_same(&bob_identity));

        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&registry_path);
    }

    // 6.4A follow-up: login publication uses each account's own effective policy,
    // and a global update batch-rebuilds an overridden account (kept) plus an
    // inheriting account (new global) — never the raw global policy for all.
    #[test]
    fn publish_and_global_update_use_each_accounts_effective_policy() {
        let proxy_path = temp_path("effective-publish");
        let registry_path = temp_path("effective-publish-accounts");
        let runtime = runtime_at(&proxy_path);
        let registry = Arc::new(AccountRegistry::new(registry_path.clone()));
        let resources = ResourceCoordinator::new();

        // Persisted global G0 plus an override for Alice only.
        std::fs::write(
            &proxy_path,
            br#"{"version":1,"global":{"mode":"custom","endpoint":"http://127.0.0.1:8000"},"accounts":{"111111111111111111":{"mode":"custom","endpoint":"http://127.0.0.1:9001"}}}"#,
        )
        .unwrap();

        let alice = user("111111111111111111", "alice");
        let alice_id = AccountId::from_user(&alice).unwrap();
        let bob = user("222222222222222222", "bob");
        let bob_id = AccountId::from_user(&bob).unwrap();

        for (account, id, token) in [
            (&alice, &alice_id, "token-alice"),
            (&bob, &bob_id, "token-bob"),
        ] {
            coordinated_publish_account(
                &registry,
                &runtime,
                &resources,
                PublishAccountRequest {
                    id: id.clone(),
                    user: account.clone(),
                    cdp_port: Some(9223),
                    used_at_ms: 1,
                    token: token.to_string(),
                    identity: SuperPropertiesHandle::new(),
                },
            )
            .unwrap();
        }

        match registry
            .runtime(&alice_id)
            .unwrap()
            .client()
            .unwrap()
            .proxy_configuration()
        {
            ProxyConfiguration::Custom(custom) => {
                assert_eq!(custom.endpoint, "http://127.0.0.1:9001")
            }
            other => panic!("alice expected her override, got {other:?}"),
        }
        match registry
            .runtime(&bob_id)
            .unwrap()
            .client()
            .unwrap()
            .proxy_configuration()
        {
            ProxyConfiguration::Custom(custom) => {
                assert_eq!(custom.endpoint, "http://127.0.0.1:8000")
            }
            other => panic!("bob expected the global policy, got {other:?}"),
        }

        // Global update to G1 batch-rebuilds both clients.
        coordinated_proxy_set(
            &registry,
            &runtime,
            input(ProxyMode::Custom, Some("http://127.0.0.1:8002")),
        )
        .unwrap();
        match registry
            .runtime(&alice_id)
            .unwrap()
            .client()
            .unwrap()
            .proxy_configuration()
        {
            ProxyConfiguration::Custom(custom) => {
                assert_eq!(custom.endpoint, "http://127.0.0.1:9001")
            }
            other => panic!("alice's override must survive a global set, got {other:?}"),
        }
        match registry
            .runtime(&bob_id)
            .unwrap()
            .client()
            .unwrap()
            .proxy_configuration()
        {
            ProxyConfiguration::Custom(custom) => {
                assert_eq!(custom.endpoint, "http://127.0.0.1:8002")
            }
            other => panic!("bob must inherit the new global, got {other:?}"),
        }

        let _ = std::fs::remove_file(&proxy_path);
        let _ = std::fs::remove_file(&registry_path);
    }
}

/// Phase 6.2 account-isolation regressions.
///
/// Pure/unit only: no live CDP, network, or keyring. They prove that a quest
/// start snapshots its account identity together with its client, and that both
/// account-scoped stop and admission stay bound to that snapshot even after the
/// registry's active account switches.
#[cfg(test)]
mod quest_start_account_isolation_tests {
    use super::*;
    use std::time::Duration;

    fn temp_registry_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "dqh-start-isolation-{label}-{}.json",
            uuid::Uuid::new_v4().simple()
        ))
    }

    fn user(id: &str, name: &str) -> DiscordUser {
        DiscordUser {
            id: id.to_string(),
            username: name.to_string(),
            discriminator: "0".to_string(),
            avatar: None,
            global_name: Some(format!("{name} Display")),
            premium_type: None,
        }
    }

    fn identity_with_os(os: &str) -> SuperPropertiesHandle {
        use base64::Engine as _;
        let handle = SuperPropertiesHandle::new();
        let props = super_properties::SuperProperties {
            os: os.to_string(),
            ..Default::default()
        };
        let json = serde_json::to_string(&props).unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&json);
        handle.set_from_cdp(&encoded, &serde_json::to_value(&props).unwrap());
        handle
    }

    fn client_with_identity(identity: SuperPropertiesHandle) -> DiscordApiClient {
        DiscordApiClient::new_with_super_properties(
            "test-token".to_string(),
            ProxyConfiguration::Direct,
            identity,
        )
        .unwrap()
    }

    /// A worker that parks until cancelled and then reports `Stopped`, so an
    /// account-scoped stop can observe a terminal outcome without any network.
    fn waiting_worker() -> QuestWorkerFactory {
        Box::new(
            |_guards: Vec<ResourceGuard>,
             mut cancel: tokio::sync::watch::Receiver<bool>,
             _progress: Arc<std::sync::atomic::AtomicU64>,
             _run_id: uuid::Uuid| {
                Box::pin(async move {
                    let _ = cancel.changed().await;
                    QuestOutcome::Stopped
                })
            },
        )
    }

    struct Fixture {
        registry: AccountRegistry,
        path: std::path::PathBuf,
        alice_id: AccountId,
        bob_id: AccountId,
        alice_identity: SuperPropertiesHandle,
        bob_identity: SuperPropertiesHandle,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// Alice (active) and Bob, each online with a client bound to a distinct
    /// identity.
    fn fixture(label: &str) -> Fixture {
        let path = temp_registry_path(label);
        let registry = AccountRegistry::new(path.clone());

        let alice = user("111111111111111111", "alice");
        let bob = user("222222222222222222", "bob");
        let alice_id = AccountId::from_user(&alice).unwrap();
        let bob_id = AccountId::from_user(&bob).unwrap();
        let alice_identity = identity_with_os("alice-os");
        let bob_identity = identity_with_os("bob-os");

        let alice_runtime = registry.ensure_runtime(&alice).unwrap();
        alice_runtime.publish_client(Some(client_with_identity(alice_identity.clone())));
        alice_runtime.mark_authenticated(&alice, Some(9223), 1);

        let bob_runtime = registry.ensure_runtime(&bob).unwrap();
        bob_runtime.publish_client(Some(client_with_identity(bob_identity.clone())));
        bob_runtime.mark_authenticated(&bob, Some(9333), 2);

        registry
            .activate(alice_id.clone(), alice_runtime.profile())
            .unwrap();

        Fixture {
            registry,
            path,
            alice_id,
            bob_id,
            alice_identity,
            bob_identity,
        }
    }

    fn switch_active_to_bob(fixture: &Fixture) {
        let bob_profile = fixture.registry.runtime(&fixture.bob_id).unwrap().profile();
        fixture
            .registry
            .activate(fixture.bob_id.clone(), bob_profile)
            .unwrap();
    }

    #[test]
    fn start_context_snapshots_account_and_client_across_a_switch() {
        let fixture = fixture("context");

        // Snapshot while Alice is active.
        let context = snapshot_quest_start(fixture.registry.active_runtime(), true)
            .expect("active account with client");

        // The active account switches to Bob before the start proceeds.
        switch_active_to_bob(&fixture);

        assert_eq!(fixture.registry.active_id().unwrap(), fixture.bob_id);
        assert_eq!(context.account_id, fixture.alice_id);

        let client = context.client.expect("client snapshot");
        assert!(client.super_properties().is_same(&fixture.alice_identity));
        assert!(!client.super_properties().is_same(&fixture.bob_identity));
        assert_eq!(
            context
                .authenticated_user
                .as_ref()
                .map(|user| user.id.as_str()),
            Some("111111111111111111")
        );
    }

    #[tokio::test]
    async fn admission_uses_the_snapshotted_account_not_the_new_active_account() {
        let fixture = fixture("admission");
        let context = snapshot_quest_start(fixture.registry.active_runtime(), true)
            .expect("active account with client");

        switch_active_to_bob(&fixture);

        let quests = QuestRegistry::new();
        let resources = ResourceCoordinator::new();
        let admitted = admit_quest_run_core(
            &quests,
            &resources,
            context.account_id.clone(),
            "quest-a".to_string(),
            QuestKind::Video,
            QuestTransport::Rest,
            waiting_worker(),
        )
        .await
        .expect("run admitted for the snapshotted account");

        // The run belongs to snapshot A, never the newly active B.
        assert_eq!(admitted.control.account_id, fixture.alice_id);
        assert!(quests.has_live_runs_for_account(&fixture.alice_id));
        assert!(!quests.has_live_runs_for_account(&fixture.bob_id));

        admitted.control.abort.abort();
    }

    #[tokio::test]
    async fn account_scoped_stop_never_touches_another_accounts_runs() {
        let quests = Arc::new(QuestRegistry::new());
        let resources = ResourceCoordinator::new();
        let alice_id = AccountId::parse("111111111111111111").unwrap();
        let bob_id = AccountId::parse("222222222222222222").unwrap();

        let alice_run = admit_quest_run_core(
            &quests,
            &resources,
            alice_id.clone(),
            "quest-a".to_string(),
            QuestKind::Video,
            QuestTransport::Rest,
            waiting_worker(),
        )
        .await
        .unwrap();
        let bob_run = admit_quest_run_core(
            &quests,
            &resources,
            bob_id.clone(),
            "quest-b".to_string(),
            QuestKind::Video,
            QuestTransport::Rest,
            waiting_worker(),
        )
        .await
        .unwrap();

        // Wire each run's terminal outcome so stop's bounded wait observes it.
        let alice_quests = Arc::clone(&quests);
        tokio::spawn(async move {
            quest_runtime::monitor_run(alice_quests.as_ref(), alice_run, |_, _| {}).await;
        });
        let bob_quests = Arc::clone(&quests);
        tokio::spawn(async move {
            quest_runtime::monitor_run(bob_quests.as_ref(), bob_run, |_, _| {}).await;
        });

        // Stop is bound to one explicit account id (Alice), not to whichever
        // account happens to be active.
        let stopped = stop_account_quests_core(&quests, &alice_id, Duration::from_secs(5)).await;

        assert!(stopped.completed.contains(&"quest-a".to_string()));
        assert!(!stopped.completed.contains(&"quest-b".to_string()));

        // Bob's run is untouched and still live.
        assert!(quests.has_live_runs_for_account(&bob_id));
    }
}

/// Phase 6.3A: identity-capture target snapshotting. A saved/restored profile is
/// not authority; only an authenticated runtime may update its identity, and the
/// binding preflight runs before any mutation. Pure/unit only: no live CDP,
/// network, or keyring.
#[cfg(test)]
mod cdp_identity_target_tests {
    use super::*;

    fn temp_registry_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "dqh-cdp-identity-{label}-{}.json",
            uuid::Uuid::new_v4().simple()
        ))
    }

    fn user(id: &str, name: &str) -> DiscordUser {
        DiscordUser {
            id: id.to_string(),
            username: name.to_string(),
            discriminator: "0".to_string(),
            avatar: None,
            global_name: Some(format!("{name} Display")),
            premium_type: None,
        }
    }

    fn identity_with_os(os: &str) -> SuperPropertiesHandle {
        use base64::Engine as _;
        let handle = SuperPropertiesHandle::new();
        let props = super_properties::SuperProperties {
            os: os.to_string(),
            ..Default::default()
        };
        let json = serde_json::to_string(&props).unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&json);
        handle.set_from_cdp(&encoded, &serde_json::to_value(&props).unwrap());
        handle
    }

    fn client_with_identity(identity: SuperPropertiesHandle) -> DiscordApiClient {
        DiscordApiClient::new_with_super_properties(
            "test-token".to_string(),
            ProxyConfiguration::Direct,
            identity,
        )
        .unwrap()
    }

    /// Bring a runtime fully online (coherent authenticated user *and* client).
    fn make_online(registry: &AccountRegistry, alice: &DiscordUser) -> Arc<AccountRuntime> {
        let runtime = registry.ensure_runtime(alice).unwrap();
        runtime.set_super_properties(identity_with_os("alice-os"));
        runtime.publish_client(Some(client_with_identity(identity_with_os("alice-os"))));
        runtime.mark_authenticated(alice, Some(9223), 1);
        runtime
    }

    // A profile restored from disk (metadata + historical `last_cdp_port`) but
    // not authenticated is not authoritative: it snapshots as a Direct target, so
    // a capture never updates its identity.
    #[test]
    fn offline_restored_profile_is_direct_and_never_updates_its_identity() {
        let path = temp_registry_path("offline");
        let registry = AccountRegistry::new(path.clone());
        let alice = user("111111111111111111", "alice");
        let alice_runtime = registry.ensure_runtime(&alice).unwrap();

        // Historical metadata only: the runtime is NOT signed in.
        let mut profile = alice_runtime.profile();
        profile.last_cdp_port = Some(9223);
        alice_runtime.set_profile(profile);
        let before = alice_runtime.super_properties().get_super_properties();

        let target = cdp_access_target_for_registry(&registry);
        assert!(matches!(target, CdpAccessTarget::Direct(_)));

        // A scratch capture (a reset here stands in for any identity write) never
        // touches the offline runtime.
        target.handle().reset();
        assert_eq!(
            serde_json::to_value(&before).unwrap(),
            serde_json::to_value(alice_runtime.super_properties().get_super_properties()).unwrap()
        );
        assert_eq!(
            alice_runtime.super_properties().get_super_properties().os,
            before.os
        );

        let _ = std::fs::remove_file(&path);
    }

    // A runtime with a user but no client is a torn state and must be a Direct
    // target: it gets no authority and cannot be updated.
    #[test]
    fn user_without_client_is_direct_and_cannot_update_identity() {
        let path = temp_registry_path("user-no-client");
        let registry = AccountRegistry::new(path.clone());
        let alice = user("111111111111111111", "alice");
        let alice_runtime = registry.ensure_runtime(&alice).unwrap();
        alice_runtime.set_super_properties(identity_with_os("alice-os"));
        alice_runtime.mark_authenticated(&alice, Some(9223), 1);
        assert!(alice_runtime.authenticated_user().is_some());
        assert!(!alice_runtime.has_client());
        assert!(registry.active_online_session().is_none());

        let target = cdp_access_target_for_registry(&registry);
        assert!(matches!(target, CdpAccessTarget::Direct(_)));

        let before = alice_runtime.super_properties().get_super_properties();
        target.handle().reset();
        assert_eq!(
            serde_json::to_value(&before).unwrap(),
            serde_json::to_value(alice_runtime.super_properties().get_super_properties()).unwrap()
        );

        let _ = std::fs::remove_file(&path);
    }

    // A coherent online runtime (user AND client) snapshots as an account target
    // carrying that exact session.
    #[test]
    fn coherent_online_runtime_is_an_account_target() {
        let path = temp_registry_path("online");
        let registry = AccountRegistry::new(path.clone());
        let alice = user("111111111111111111", "alice");
        let alice_id = AccountId::from_user(&alice).unwrap();
        let alice_runtime = make_online(&registry, &alice);
        // Active account is required for `active_online_session`.
        registry
            .activate(alice_id.clone(), alice_runtime.profile())
            .unwrap();

        let target = cdp_access_target_for_registry(&registry);
        match target {
            CdpAccessTarget::Account(session) => assert_eq!(session.account_id(), &alice_id),
            CdpAccessTarget::Direct(_) => panic!("coherent runtime must be an account target"),
        }
        assert_eq!(
            registry
                .active_online_session()
                .map(|session| session.account_id().clone()),
            Some(alice_id)
        );

        let _ = std::fs::remove_file(&path);
    }

    // The shared authority gate for all three account-targeted CDP starts
    // (CDP quest, CDP PLAY_ACTIVITY, manual spoof): restored/user-only/client-only
    // runtimes yield no online session, so every path returns `Not logged in`
    // before any side effect.
    #[test]
    fn account_starts_require_a_coherent_pair() {
        let path = temp_registry_path("cdp-context");
        let registry = AccountRegistry::new(path.clone());

        // Restored profile: neither user nor client.
        let restored = user("111111111111111111", "restored");
        registry.ensure_runtime(&restored).unwrap();
        assert!(registry.active_online_session().is_none());

        // User present, client absent.
        let user_only = user("222222222222222222", "user-only");
        let user_only_runtime = registry.ensure_runtime(&user_only).unwrap();
        user_only_runtime.mark_authenticated(&user_only, Some(9223), 1);
        registry
            .activate(
                AccountId::from_user(&user_only).unwrap(),
                user_only_runtime.profile(),
            )
            .unwrap();
        assert!(registry.active_online_session().is_none());

        // Client present, user absent.
        let client_only = user("333333333333333333", "client-only");
        let client_only_runtime = registry.ensure_runtime(&client_only).unwrap();
        client_only_runtime.publish_client(Some(client_with_identity(identity_with_os("x"))));
        registry
            .activate(
                AccountId::from_user(&client_only).unwrap(),
                client_only_runtime.profile(),
            )
            .unwrap();
        assert!(registry.active_online_session().is_none());

        // Coherent pair is accepted and carries both fields.
        let online = user("444444444444444444", "online");
        let online_runtime = make_online(&registry, &online);
        registry
            .activate(
                AccountId::from_user(&online).unwrap(),
                online_runtime.profile(),
            )
            .unwrap();
        let session = registry
            .active_online_session()
            .expect("coherent pair accepted");
        assert_eq!(session.account_id().as_str(), "444444444444444444");
        assert_eq!(session.user().id, "444444444444444444");

        let _ = std::fs::remove_file(&path);
    }

    // A publication/logout transition cannot yield a torn snapshot: the public
    // authority path is fail-closed on a user-only (client-cleared) state.
    #[test]
    fn publication_gate_never_observes_a_torn_session() {
        let path = temp_registry_path("gate");
        let registry = AccountRegistry::new(path.clone());
        let alice = user("111111111111111111", "alice");
        let runtime = registry.ensure_runtime(&alice).unwrap();
        registry
            .activate(AccountId::from_user(&alice).unwrap(), runtime.profile())
            .unwrap();

        // User present but client cleared (mid-logout) is never an online session.
        runtime.mark_authenticated(&alice, Some(9223), 1);
        runtime.publish_client(None);
        assert!(runtime.authenticated_user().is_some());
        assert!(registry.active_online_session().is_none());

        // Publishing the client makes the pair coherent.
        runtime.publish_client(Some(client_with_identity(identity_with_os("alice-os"))));
        let session = registry.active_online_session().expect("coherent pair");
        assert_eq!(session.user().id, "111111111111111111");

        let _ = std::fs::remove_file(&path);
    }

    // A conflicting account-lease acquisition aborts before the retry reset,
    // leaving the captured identity byte-for-byte unchanged.
    #[tokio::test]
    async fn conflicting_lease_aborts_before_retry_reset() {
        let path = temp_registry_path("retry-conflict");
        let registry = AccountRegistry::new(path.clone());
        let alice = user("111111111111111111", "alice");
        let alice_runtime = make_online(&registry, &alice);
        registry
            .activate(
                AccountId::from_user(&alice).unwrap(),
                alice_runtime.profile(),
            )
            .unwrap();
        let session = registry.active_online_session().unwrap();

        let target = cdp_access_target_for_registry(&registry);
        let before = alice_runtime.super_properties().get_super_properties();

        // A direct holder already owns the port, so the account lease is Busy.
        let leases = CdpPortLeases::new();
        let _holder = leases.acquire_direct(9223).unwrap();
        let mut reset_ran = false;
        let result = leases
            .acquire_account(9223, &session, || async { Ok::<(), String>(()) })
            .await;
        if result.is_ok() {
            prepare_super_properties_retry(&target).unwrap();
            reset_ran = true;
        }
        assert!(result.is_err());
        assert!(!reset_ran);
        assert_eq!(
            serde_json::to_value(&before).unwrap(),
            serde_json::to_value(alice_runtime.super_properties().get_super_properties()).unwrap()
        );
        assert_eq!(
            alice_runtime.super_properties().get_super_properties().os,
            "alice-os"
        );

        let _ = std::fs::remove_file(&path);
    }

    // A successful account lease is verified/active before the retry resets the
    // captured identity for the following auto-fetch.
    #[tokio::test]
    async fn retry_reset_runs_only_after_a_successful_account_lease() {
        let path = temp_registry_path("retry-match");
        let registry = AccountRegistry::new(path.clone());
        let alice = user("111111111111111111", "alice");
        let alice_runtime = make_online(&registry, &alice);
        registry
            .activate(
                AccountId::from_user(&alice).unwrap(),
                alice_runtime.profile(),
            )
            .unwrap();
        let session = registry.active_online_session().unwrap();

        let target = cdp_access_target_for_registry(&registry);
        assert_eq!(target.handle().get_super_properties().os, "alice-os");

        let leases = CdpPortLeases::new();
        let lease = leases
            .acquire_account(9223, &session, || async { Ok::<(), String>(()) })
            .await
            .unwrap();
        assert_eq!(
            leases.snapshot(9223).unwrap().phase,
            cdp_port_lease::LeasePhase::Active
        );

        prepare_super_properties_retry(&target).unwrap();
        assert_eq!(target.handle().get_super_properties().os, "Windows");
        assert_ne!(target.handle().get_super_properties().os, "alice-os");

        drop(lease);
        assert!(leases.snapshot(9223).is_none());

        let _ = std::fs::remove_file(&path);
    }
}

/// Phase 6.3A.2 Integration C: the production CDP worker lease wrapper and exit
/// lease policy. Pure/offline: no live Discord, desktop CDP, network, or keyring.
#[cfg(test)]
mod cdp_lease_worker_tests {
    use super::*;

    fn account(id: &str) -> AccountId {
        AccountId::parse(id).expect("valid test account id")
    }

    // The one production worker wrapper used by both CDP starts holds the lease
    // for the parked worker's whole lifetime; termination releases it.
    #[tokio::test]
    async fn worker_wrapper_holds_the_lease_until_termination() {
        let leases = Arc::new(CdpPortLeases::new());
        let quests = QuestRegistry::new();
        let resources = ResourceCoordinator::new();
        let alice = account("111111111111111111");

        let lease = leases.acquire_direct(9223).unwrap();
        let factory = cdp_worker_factory(lease, |mut cancel, _progress, _run_id| async move {
            while !*cancel.borrow_and_update() {
                if cancel.changed().await.is_err() {
                    break;
                }
            }
            QuestOutcome::Stopped
        });

        let admitted = admit_quest_run_core(
            &quests,
            &resources,
            alice,
            "quest-a".to_string(),
            QuestKind::Video,
            QuestTransport::Cdp { port: 9223 },
            factory,
        )
        .await
        .expect("admitted");

        // Parked worker holds the lease; a direct competitor is rejected.
        assert!(leases.snapshot(9223).is_some());
        assert!(leases.acquire_direct(9223).is_err());

        // Terminate the worker; the wrapper drops its lease.
        admitted.control.cancel.send(true).unwrap();
        assert_eq!(admitted.worker.await.unwrap(), QuestOutcome::Stopped);
        assert!(leases.snapshot(9223).is_none());
        assert!(leases.acquire_direct(9223).is_ok());
    }

    // Admission failure never invokes the factory, so the lease it captured is
    // dropped rather than leaked.
    #[tokio::test]
    async fn worker_admission_failure_drops_the_lease() {
        let leases = CdpPortLeases::new();
        let quests = QuestRegistry::new();
        let resources = ResourceCoordinator::new();
        let alice = account("111111111111111111");

        let lease = leases.acquire_direct(9223).unwrap();
        let factory = cdp_worker_factory(lease, |_cancel, _progress, _run_id| async {
            QuestOutcome::Stopped
        });

        // Hold AccountActivity so a PLAY_ACTIVITY admission is rejected before
        // the factory runs.
        let _held = resources
            .try_acquire_all(&alice, &[QuestResource::AccountActivity])
            .unwrap();
        let result = admit_quest_run_core(
            &quests,
            &resources,
            alice,
            "quest-a".to_string(),
            QuestKind::PlayActivity,
            QuestTransport::Cdp { port: 9223 },
            factory,
        )
        .await;
        assert!(result.is_err());
        assert!(leases.snapshot(9223).is_none());
        assert!(leases.acquire_direct(9223).is_ok());
    }

    // A cancelled worker future drops its lease.
    #[tokio::test]
    async fn worker_cancellation_drops_the_lease() {
        let leases = Arc::new(CdpPortLeases::new());
        let quests = QuestRegistry::new();
        let resources = ResourceCoordinator::new();
        let alice = account("111111111111111111");

        let lease = leases.acquire_direct(9223).unwrap();
        let factory = cdp_worker_factory(lease, |mut cancel, _progress, _run_id| async move {
            // Park forever until the receiver is dropped by cancellation.
            while cancel.changed().await.is_ok() {}
            QuestOutcome::Stopped
        });
        let admitted = admit_quest_run_core(
            &quests,
            &resources,
            alice,
            "quest-a".to_string(),
            QuestKind::Video,
            QuestTransport::Cdp { port: 9223 },
            factory,
        )
        .await
        .expect("admitted");
        assert!(leases.snapshot(9223).is_some());

        admitted.control.abort.abort();
        assert!(admitted.worker.await.is_err());
        assert!(leases.snapshot(9223).is_none());
    }

    // Exit requires an empty lease snapshot; a live lease keeps it retryable.
    #[test]
    fn exit_requires_an_empty_lease_snapshot() {
        let leases = CdpPortLeases::new();
        assert!(exit_lease_error(&leases).is_none());

        let lease = leases.acquire_direct(9223).unwrap();
        let error = exit_lease_error(&leases).expect("live lease blocks exit");
        assert!(error.contains("still held"));
        assert!(exit_lease_error(&leases).is_some());

        drop(lease);
        assert!(exit_lease_error(&leases).is_none());
    }
}

/// 6.3A.2 consolidated remediation tests: cancellation-safe blocking leases, the
/// all-ports maintenance lease, manual-session generations, and actual waiter
/// aborts of the owned start/stop operations. Pure/offline.
#[cfg(test)]
mod leased_lifecycle_tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn session(name: &str) -> ManualCdpGameSimulation {
        ManualCdpGameSimulation {
            app_id: "123456".to_string(),
            app_name: name.to_string(),
            cdp_port: 9223,
        }
    }

    fn account(id: &str) -> AccountId {
        AccountId::parse(id).expect("valid test account id")
    }

    fn alice() -> AccountId {
        account("111111111111111111")
    }

    async fn wait_until<F>(mut predicate: F, label: &str)
    where
        F: FnMut() -> bool,
    {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !predicate() {
            assert!(Instant::now() < deadline, "timed out waiting for {label}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    // Fix 1: a blocking lease owned by the blocking task survives aborting the
    // async waiter; all contenders stay busy until the work completes.
    #[tokio::test]
    async fn blocking_lease_survives_waiter_abort() {
        let leases = CdpPortLeases::new();
        let lease = leases.acquire_direct(9223).unwrap();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let blocking = spawn_blocking_with_lease(lease, move || {
            let _ = release_rx.recv();
            7u32
        });

        // Abort the async waiter before the blocking work completes.
        assert!(tokio::time::timeout(Duration::from_millis(15), blocking)
            .await
            .is_err());

        // The blocking task still owns the lease.
        assert!(leases.snapshot(9223).is_some());
        assert!(leases.acquire_direct(9223).is_err());

        release_tx.send(()).unwrap();
        wait_until(|| leases.snapshot(9223).is_none(), "blocking lease release").await;
        assert!(leases.acquire_direct(9223).is_ok());
    }

    // Fix 2: a global maintenance lease held in a blocking closure survives an
    // aborted waiter and keeps every per-port acquisition busy until release.
    #[tokio::test]
    async fn global_maintenance_lease_survives_waiter_abort() {
        let leases = Arc::new(CdpPortLeases::new());
        let global = leases.acquire_global_maintenance().unwrap();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let blocking = spawn_blocking_with_lease(global, move || {
            let _ = release_rx.recv();
        });

        assert!(tokio::time::timeout(Duration::from_millis(15), blocking)
            .await
            .is_err());

        // Global still blocks per-port and global acquisitions.
        assert!(leases.acquire_direct(9223).is_err());
        assert!(leases.acquire_global_maintenance().is_err());
        assert!(!leases.is_empty());

        release_tx.send(()).unwrap();
        wait_until(|| leases.is_empty(), "global lease release").await;
        assert!(leases.acquire_direct(9223).is_ok());
    }

    // Fix 3: aborting the outer start waiter leaves the inner owned operation to
    // reach a terminal state; success ends Absent and releases the lease.
    #[tokio::test]
    async fn owned_start_waiter_abort_reaches_terminal_absent() {
        let owner = alice();
        let leases = Arc::new(CdpPortLeases::new());
        let sessions = Arc::new(tokio::sync::Mutex::new(ManualCdpGameSessionState::default()));
        let lease = leases.acquire_direct(9223).unwrap();
        let generation = sessions
            .lock()
            .await
            .begin_start(owner.clone(), Vec::new(), Some(lease))
            .unwrap();

        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let inner_sessions = Arc::clone(&sessions);
        let inner_owner = owner.clone();
        let outer = run_owned_operation(move || {
            let inner_sessions = inner_sessions;
            async move {
                let _ = release_rx.await;
                let mut guard = inner_sessions.lock().await;
                guard
                    .commit_start(&inner_owner, generation, session("A"))
                    .unwrap();
                let claimed = guard.claim_cleanup(ManualStopScope::Account(inner_owner.clone()));
                if let Some(claimed) = claimed {
                    guard.finish_cleanup(&claimed, Ok(())).unwrap();
                }
                Ok::<(), String>(())
            }
        });

        // Abort the outer waiter; the inner task keeps the lease while parked.
        assert!(tokio::time::timeout(Duration::from_millis(20), outer)
            .await
            .is_err());
        assert!(leases.snapshot(9223).is_some());

        release_tx.send(()).unwrap();
        let inner_sessions = Arc::clone(&sessions);
        wait_until(
            || {
                // Poll without holding the lock across the await boundary: use a
                // best-effort try_lock snapshot.
                inner_sessions
                    .try_lock()
                    .map(|guard| {
                        !matches!(
                            guard.status(),
                            ManualSessionStatus::Active { .. }
                                | ManualSessionStatus::Starting { .. }
                                | ManualSessionStatus::CleaningUp { .. }
                        )
                    })
                    .unwrap_or(false)
            },
            "inner start terminal Absent",
        )
        .await;
        assert!(leases.snapshot(9223).is_none());
    }

    // Fix 3: aborting the outer stop waiter leaves a failed cleanup retryable:
    // the inner operation finishes with the session Active and the lease held.
    #[tokio::test]
    async fn owned_stop_waiter_abort_keeps_retryable_active() {
        let owner = alice();
        let leases = Arc::new(CdpPortLeases::new());
        let sessions = Arc::new(tokio::sync::Mutex::new(ManualCdpGameSessionState::default()));
        {
            let mut guard = sessions.lock().await;
            let lease = leases.acquire_direct(9223).unwrap();
            let generation = guard
                .begin_start(owner.clone(), Vec::new(), Some(lease))
                .unwrap();
            guard
                .commit_start(&owner, generation, session("A"))
                .unwrap();
        }

        let release = Arc::new(tokio::sync::Notify::new());
        let inner_sessions = Arc::clone(&sessions);
        let inner_scope = ManualStopScope::Account(owner.clone());
        let inner_release = Arc::clone(&release);
        let outer = run_owned_operation(move || {
            let inner_sessions = inner_sessions;
            async move {
                run_scoped_manual_stop(inner_sessions.as_ref(), inner_scope, move |_port| {
                    let inner_release = Arc::clone(&inner_release);
                    async move {
                        inner_release.notified().await;
                        Err("cleanup failed".to_string())
                    }
                })
                .await
            }
        });

        // Abort the outer stop waiter while cleanup is in flight.
        assert!(tokio::time::timeout(Duration::from_millis(40), outer)
            .await
            .is_err());

        // Cleanup is claimed or the session is still Active (never Absent) and
        // the lease is retained while the cleanup is in flight.
        assert!(!matches!(
            sessions.lock().await.status(),
            ManualSessionStatus::Absent
        ));
        assert!(leases.snapshot(9223).is_some());

        release.notify_one();
        let inner_sessions = Arc::clone(&sessions);
        wait_until(
            || {
                inner_sessions
                    .try_lock()
                    .map(|guard| matches!(guard.status(), ManualSessionStatus::Active { .. }))
                    .unwrap_or(false)
            },
            "retryable Active after failed cleanup",
        )
        .await;
        assert!(sessions.lock().await.active().is_some());
        assert!(leases.snapshot(9223).is_some());
    }

    // Fix 3: a stale finalizer from session A cannot alter a newer same-owner
    // session B or drop B's lease.
    #[tokio::test]
    async fn stale_finalizer_cannot_alter_a_newer_same_owner_session() {
        let owner = alice();
        let leases = CdpPortLeases::new();
        let mut state = ManualCdpGameSessionState::default();

        // Session A: start, commit, claim, finish success (Absent, lease free).
        let lease_a = leases.acquire_direct(9223).unwrap();
        let generation_a = state
            .begin_start(owner.clone(), Vec::new(), Some(lease_a))
            .unwrap();
        state
            .commit_start(&owner, generation_a, session("A"))
            .unwrap();
        let claimed_a = state
            .claim_cleanup(ManualStopScope::Account(owner.clone()))
            .unwrap();
        assert_eq!(claimed_a.generation, generation_a);
        state.finish_cleanup(&claimed_a, Ok(())).unwrap();
        assert!(state.active().is_none());
        assert!(leases.snapshot(9223).is_none());

        // Session B: same owner, a strictly newer generation.
        let lease_b = leases.acquire_direct(9223).unwrap();
        let generation_b = state
            .begin_start(owner.clone(), Vec::new(), Some(lease_b))
            .unwrap();
        assert_ne!(generation_a, generation_b);
        state
            .commit_start(&owner, generation_b, session("B"))
            .unwrap();
        assert!(leases.snapshot(9223).is_some());

        // Stale A finalizers change nothing.
        assert!(state
            .commit_start(&owner, generation_a, session("A-stale"))
            .is_err());
        state.cancel_start(&owner, generation_a);
        assert_eq!(state.active().unwrap().app_name, "B");
        // A stale claim/finish is a no-op for the newer session.
        state.finish_cleanup(&claimed_a, Ok(())).unwrap();
        assert_eq!(state.active().unwrap().app_name, "B");
        assert!(leases.snapshot(9223).is_some());

        // B's own cleanup works.
        let claimed_b = state
            .claim_cleanup(ManualStopScope::Account(owner.clone()))
            .unwrap();
        assert_eq!(claimed_b.generation, generation_b);
        state.finish_cleanup(&claimed_b, Ok(())).unwrap();
        assert!(state.active().is_none());
        assert!(leases.snapshot(9223).is_none());
    }
}

/// Phase 6.4A: account DTOs, account-scoped run listing, and event envelopes.
#[cfg(test)]
mod account_ipc_tests {
    use super::*;
    use std::time::Duration;

    fn user(id: &str, name: &str) -> DiscordUser {
        DiscordUser {
            id: id.to_string(),
            username: name.to_string(),
            discriminator: "0".to_string(),
            avatar: None,
            global_name: Some(format!("{name} Display")),
            premium_type: None,
        }
    }

    fn temp_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "dqh-account-ipc-{label}-{}.json",
            uuid::Uuid::new_v4().simple()
        ))
    }

    fn parking_worker() -> QuestWorkerFactory {
        Box::new(
            |_guards: Vec<ResourceGuard>,
             mut cancel: tokio::sync::watch::Receiver<bool>,
             _progress: Arc<std::sync::atomic::AtomicU64>,
             _run_id: uuid::Uuid| {
                Box::pin(async move {
                    let _ = cancel.changed().await;
                    QuestOutcome::Stopped
                })
            },
        )
    }

    fn make_online_runtime(
        registry: &AccountRegistry,
        account: &DiscordUser,
        port: u16,
        token: &str,
    ) -> Arc<AccountRuntime> {
        let runtime = registry.ensure_runtime(account).unwrap();
        let client =
            DiscordApiClient::new_with_proxy(token.to_string(), ProxyConfiguration::Direct)
                .unwrap();
        let gate = registry.coordination_gate();
        let _gate = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        runtime.mark_authenticated(account, Some(port), 1);
        runtime.publish_client(Some(client));
        runtime
    }

    fn persist_and_activate_seed(
        registry: &AccountRegistry,
        account_id: &AccountId,
        runtime: &Arc<AccountRuntime>,
    ) {
        let gate = registry.coordination_gate();
        let _gate = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = runtime.profile();
        registry.save_activation(account_id, &profile).unwrap();
        registry.activate(account_id.clone(), profile).unwrap();
    }

    fn seed_accounts_with_a_active(
        path: &std::path::Path,
        bob_online: bool,
    ) -> (
        AccountRegistry,
        AccountId,
        AccountId,
        Arc<AccountRuntime>,
        Arc<AccountRuntime>,
    ) {
        let registry = AccountRegistry::new(path.to_path_buf());
        let alice = user("111111111111111111", "alice");
        let alice_id = AccountId::from_user(&alice).unwrap();
        let alice_runtime = make_online_runtime(&registry, &alice, 9223, "alice-token");
        persist_and_activate_seed(&registry, &alice_id, &alice_runtime);

        let bob = user("222222222222222222", "bob");
        let bob_id = AccountId::from_user(&bob).unwrap();
        let bob_runtime = if bob_online {
            make_online_runtime(&registry, &bob, 9333, "bob-token")
        } else {
            registry.ensure_runtime(&bob).unwrap()
        };
        registry.persist().unwrap();
        (registry, alice_id, bob_id, alice_runtime, bob_runtime)
    }

    #[test]
    fn online_only_activation_rejects_account_that_went_offline_without_mutation() {
        let path = temp_path("online-activate-offline");
        let (registry, alice_id, bob_id, alice_runtime, bob_runtime) =
            seed_accounts_with_a_active(&path, true);
        bob_runtime.clear_authenticated();
        let persisted_before = std::fs::read(&path).unwrap();
        let bob_profile = bob_runtime.profile();
        let alice_token = alice_runtime.client().unwrap().get_token().to_string();

        let result = coordinated_activate_online_account(&registry, &bob_id);

        assert!(matches!(
            result,
            Err(ref error) if error == "Account is offline. Reconnect it before switching."
        ));
        assert_eq!(registry.active_id(), Some(alice_id));
        assert_eq!(std::fs::read(&path).unwrap(), persisted_before);
        assert_eq!(bob_runtime.profile(), bob_profile);
        assert!(!bob_runtime.has_client());
        assert_eq!(
            alice_runtime.client().unwrap().get_token(),
            alice_token.as_str()
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn online_only_activation_persists_and_activates_online_account() {
        let path = temp_path("online-activate-success");
        let (registry, _alice_id, bob_id, alice_runtime, bob_runtime) =
            seed_accounts_with_a_active(&path, true);

        let account = coordinated_activate_online_account(&registry, &bob_id).unwrap();

        assert_eq!(account.id(), &bob_id);
        assert_eq!(registry.active_id(), Some(bob_id.clone()));
        assert!(bob_runtime.has_client());
        assert!(alice_runtime.has_client());
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted["active"], bob_id.as_str());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn legacy_activation_still_accepts_offline_profiles() {
        let path = temp_path("legacy-offline-activate");
        let (registry, _alice_id, bob_id, alice_runtime, bob_runtime) =
            seed_accounts_with_a_active(&path, false);

        let account = coordinated_activate_account(&registry, &bob_id).unwrap();

        assert_eq!(account.id(), &bob_id);
        assert_eq!(registry.active_id(), Some(bob_id));
        assert!(!bob_runtime.has_client());
        assert!(bob_runtime.authenticated_user().is_none());
        assert!(alice_runtime.has_client());

        let _ = std::fs::remove_file(&path);
    }

    // The account summary DTO is camelCase and never carries secrets.
    #[test]
    fn account_summary_dto_is_camel_case_and_secret_free() {
        let path = temp_path("dto");
        let registry = AccountRegistry::new(path.clone());
        let alice = user("111111111111111111", "alice");
        let runtime = registry.ensure_runtime(&alice).unwrap();
        runtime.mark_authenticated(&alice, Some(9223), 1);

        let dto = account_summary_dto(&registry, runtime.profile());
        assert!(!dto.is_authenticated);
        let value = serde_json::to_value(&dto).unwrap();
        assert_eq!(value["id"], "111111111111111111");
        assert_eq!(value["username"], "alice");
        assert_eq!(value["globalName"], "alice Display");
        assert_eq!(value["lastCdpPort"], 9223);
        assert_eq!(value["isAuthenticated"], false);
        assert!(value.get("global_name").is_none());
        assert!(value.get("last_cdp_port").is_none());

        let text = serde_json::to_string(&dto).unwrap().to_ascii_lowercase();
        for forbidden in ["token", "credential", "password", "secret"] {
            assert!(!text.contains(forbidden), "DTO leaked {forbidden}");
        }
        let _ = std::fs::remove_file(&path);
    }

    // The snapshot reflects the active id and each account's live auth state; an
    // authenticated client on A does not mark offline B authenticated.
    #[test]
    fn accounts_snapshot_reports_active_and_authentication() {
        let path = temp_path("snapshot");
        let registry = AccountRegistry::new(path.clone());
        let alice = user("111111111111111111", "alice");
        let bob = user("222222222222222222", "bob");
        let alice_runtime = registry.ensure_runtime(&alice).unwrap();
        alice_runtime.mark_authenticated(&alice, Some(9223), 1);
        alice_runtime.publish_client(Some(
            DiscordApiClient::new_with_proxy("t".to_string(), ProxyConfiguration::Direct).unwrap(),
        ));
        registry
            .activate(
                AccountId::from_user(&alice).unwrap(),
                alice_runtime.profile(),
            )
            .unwrap();
        registry.ensure_runtime(&bob).unwrap();

        let snapshot = accounts_snapshot(&registry);
        assert_eq!(
            snapshot.active_account_id.as_deref(),
            Some("111111111111111111")
        );
        assert_eq!(snapshot.accounts.len(), 2);
        let a = snapshot
            .accounts
            .iter()
            .find(|dto| dto.id == "111111111111111111")
            .unwrap();
        let b = snapshot
            .accounts
            .iter()
            .find(|dto| dto.id == "222222222222222222")
            .unwrap();
        assert!(a.is_authenticated);
        assert!(!b.is_authenticated);
        let value = serde_json::to_value(&snapshot).unwrap();
        assert!(value.get("activeAccountId").is_some());
        let _ = std::fs::remove_file(&path);
    }

    // All-account listing carries each account's identity, so identical quest ids
    // on A and B are distinguishable; an account-scoped stop only stops A.
    #[tokio::test]
    async fn all_runs_carry_each_account_identity_and_isolated_stop() {
        let quests = Arc::new(QuestRegistry::new());
        let resources = ResourceCoordinator::new();
        let a = AccountId::parse("111111111111111111").unwrap();
        let b = AccountId::parse("222222222222222222").unwrap();

        let run_a = admit_quest_run_core(
            &quests,
            &resources,
            a.clone(),
            "quest-x".to_string(),
            QuestKind::Video,
            QuestTransport::Rest,
            parking_worker(),
        )
        .await
        .unwrap();
        let run_b = admit_quest_run_core(
            &quests,
            &resources,
            b.clone(),
            "quest-x".to_string(),
            QuestKind::Video,
            QuestTransport::Rest,
            parking_worker(),
        )
        .await
        .unwrap();
        {
            let quests = Arc::clone(&quests);
            tokio::spawn(async move {
                quest_runtime::monitor_run(quests.as_ref(), run_a, |_, _| {}).await;
            });
        }
        {
            let quests = Arc::clone(&quests);
            tokio::spawn(async move {
                quest_runtime::monitor_run(quests.as_ref(), run_b, |_, _| {}).await;
            });
        }

        let dtos: Vec<QuestRunDto> = quests
            .snapshot()
            .iter()
            .map(|control| quest_run_dto(control))
            .collect();
        assert_eq!(dtos.len(), 2);
        assert!(dtos.iter().all(|dto| dto.quest_id == "quest-x"));
        let accounts: std::collections::HashSet<&str> =
            dtos.iter().map(|dto| dto.account_id.as_str()).collect();
        assert_eq!(accounts.len(), 2);

        // Account-scoped stop affects only A.
        let stopped = stop_account_quests_core(&quests, &a, Duration::from_secs(5)).await;
        assert!(stopped.completed.contains(&"quest-x".to_string()));
        assert!(quests.has_live_runs_for_account(&b));
    }

    // The event envelope is camelCase and preserves the exact identity.
    #[test]
    fn quest_event_envelope_is_camel_case_with_exact_ids() {
        let envelope = QuestEventEnvelope {
            account_id: "111111111111111111".to_string(),
            quest_id: "quest-x".to_string(),
            run_id: "run-1".to_string(),
            progress: Some(0.5),
            message: None,
            kind: Some("complete".to_string()),
        };
        let value = serde_json::to_value(&envelope).unwrap();
        assert_eq!(value["accountId"], "111111111111111111");
        assert_eq!(value["questId"], "quest-x");
        assert_eq!(value["runId"], "run-1");
        assert_eq!(value["progress"], 0.5);
        assert_eq!(value["kind"], "complete");
        assert!(value.get("message").is_none());
    }
}
