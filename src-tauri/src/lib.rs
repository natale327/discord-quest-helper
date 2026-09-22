// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod cdp_client;
mod cdp_game_spoof;
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

use discord_api::DiscordApiClient;
use models::*;
use once_cell::sync::Lazy;
use proxy_settings::{KeyringCredentialStore, ProxyConfiguration, ProxyRuntime};
use quest_runtime::{
    AdmittedRun, DoneWait, QuestEventSink, QuestKind, QuestOutcome, QuestRegistry, QuestResource,
    QuestTransport, ResourceCoordinator, ResourceGuard, StopClass, StopSignal,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use super_properties::XSuperPropertiesManager;
use tauri::ipc::Channel;
use tauri::{Emitter, Listener, Manager, State, WebviewWindowBuilder};

/// Global X-Super-Properties manager (session-level)
/// Automatically generates key validation fields, fetches latest version info from Discord after login
static SUPER_PROPERTIES_MANAGER: Lazy<Mutex<XSuperPropertiesManager>> =
    Lazy::new(|| Mutex::new(XSuperPropertiesManager::new()));

const APP_EXIT_RPC_DISCONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
/// Bound for waiting on a cancelled quest task. CDP cancel cleanup uses one
/// 15s evaluation; keep headroom for a poll-loop select to notice cancel.
const QUEST_STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(45);
/// Last-chance wait inside `exit_app_now` after the frontend's short prepare
/// deadline. Covers verified manual CDP cleanup (five 15s evaluations) plus
/// a cancelled quest task so `process::exit` does not abort in-flight rollback.
const APP_EXIT_FINAL_CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

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

/// Global state: Discord API client plus the quest run registry and the shared
/// resource coordinator that serializes account-level Discord activity.
struct AppState {
    client: Mutex<Option<DiscordApiClient>>,
    authenticated_user: Mutex<Option<DiscordUser>>,
    quests: Arc<QuestRegistry>,
    resources: Arc<ResourceCoordinator>,
    manual_cdp_game: tokio::sync::Mutex<ManualCdpGameSessionState>,
    /// Effective global proxy policy plus its (blocking) OS credential store.
    proxy: Arc<ProxyRuntime>,
}

/// Resolve the saved proxy policy off the main executor. Keyring access can
/// block (notably on Linux), so it runs on a blocking thread. Structural
/// problems fall back to System; a locked/unavailable keychain surfaces an
/// actionable error instead of silently dropping credentials.
async fn resolve_proxy_configuration(
    state: &State<'_, AppState>,
) -> Result<ProxyConfiguration, String> {
    let runtime = state.proxy.clone();
    tokio::task::spawn_blocking(move || runtime.resolve_current_for_login())
        .await
        .map_err(|error| format!("Proxy settings task failed: {error}"))?
        .map_err(|error| error.to_string())
}

/// Rebuild the active authenticated client (and therefore all its clones) for a
/// new policy. A no-op when no client exists yet.
fn apply_proxy_to_active_client(
    state: &State<'_, AppState>,
    configuration: &ProxyConfiguration,
) -> Result<(), String> {
    let client = {
        let guard = state
            .client
            .lock()
            .map_err(|_| "Discord client state is unavailable".to_string())?;
        guard.as_ref().cloned()
    };
    if let Some(client) = client {
        client.apply_proxy_configuration(configuration).map_err(|error| {
            format!("Proxy settings were saved, but applying them to the active session failed: {error}")
        })?;
    }
    Ok(())
}

#[derive(Debug, Default)]
struct ManualCdpGameSessionState {
    active: Option<ManualCdpGameSimulation>,
    /// Held for the whole manual spoof lifetime so account activity and the CDP
    /// port stay reserved until verified cleanup finishes.
    guards: Vec<ResourceGuard>,
}

impl ManualCdpGameSessionState {
    fn ensure_idle(&self) -> Result<(), String> {
        match &self.active {
            Some(session) => Err(format!(
                "A manual CDP game simulation is already active for {}",
                session.app_name
            )),
            None => Ok(()),
        }
    }

    fn activate(&mut self, session: ManualCdpGameSimulation, guards: Vec<ResourceGuard>) {
        self.active = Some(session);
        self.guards = guards;
    }

    fn active(&self) -> Option<ManualCdpGameSimulation> {
        self.active.clone()
    }

    fn clear(&mut self) {
        self.active = None;
        // Dropping the guards releases the reserved resources.
        self.guards.clear();
    }

    fn finish_cleanup(&mut self, result: Result<(), String>) -> Result<(), String> {
        result?;
        self.clear();
        Ok(())
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

    #[test]
    fn only_one_manual_cdp_game_can_be_active() {
        let mut state = ManualCdpGameSessionState::default();
        state.ensure_idle().unwrap();
        state.activate(session("First"), Vec::new());

        assert!(state.ensure_idle().is_err());
        assert_eq!(state.active().unwrap().app_name, "First");
    }

    #[test]
    fn failed_start_does_not_record_a_session() {
        let state = ManualCdpGameSessionState::default();
        state.ensure_idle().unwrap();

        // CDP startup failed before activate() was called.
        assert!(state.active().is_none());
    }

    #[test]
    fn cleanup_failure_keeps_the_session_for_retry() {
        let mut state = ManualCdpGameSessionState::default();
        state.activate(session("Retry Me"), Vec::new());

        assert!(state
            .finish_cleanup(Err("Discord target disconnected".to_string()))
            .is_err());
        assert_eq!(state.active().unwrap().app_name, "Retry Me");

        state.finish_cleanup(Ok(())).unwrap();
        assert!(state.active().is_none());
    }

    #[test]
    fn session_uses_the_frontend_camel_case_contract() {
        let value = serde_json::to_value(session("Contract")).unwrap();
        assert_eq!(value["appId"], "123456");
        assert_eq!(value["appName"], "Contract");
        assert_eq!(value["cdpPort"], 9223);
        assert!(value.get("app_id").is_none());
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
    use super::capture_cdp_session_with_progress;
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
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine};

    let cdp_port = port.unwrap_or(cdp_client::DEFAULT_CDP_PORT);

    log(
        LogLevel::Info,
        LogCategory::TokenExtraction,
        &format!("Starting CDP auto-login on port {}", cdp_port),
        None,
    );

    // 1. Capture the current session's Authorization over CDP. The token stays
    //    inside `session` (a zero-on-drop wrapper) and is never returned to the
    //    UI, logged, or persisted.
    let progress_channel = on_progress.clone();
    let session = capture_cdp_session_with_progress(
        cdp_client::capture_discord_auth_via_cdp(cdp_port, std::time::Duration::from_secs(20)),
        move |progress| {
            let _ = progress_channel.send(progress);
        },
    )
    .await
    .map_err(|e| e.to_string())?;

    let _ = on_progress.send(AuthProgress::phase(AuthProgressPhase::ValidatingCdpSession));

    // 2. Build an API client from the captured token and validate it via
    //    /users/@me. An invalid capture is rejected here. The saved proxy policy
    //    is resolved on a blocking thread so keyring access never stalls the
    //    async runtime.
    let proxy = resolve_proxy_configuration(&state).await?;
    let client = DiscordApiClient::new_with_proxy(session.authorization.to_string(), proxy)
        .map_err(|e| format!("Failed to create API client: {}", e))?;
    let user = client
        .get_current_user()
        .await
        .map_err(|e| format!("Captured Discord session is not valid: {}", e))?;

    let _ = on_progress.send(AuthProgress::phase(AuthProgressPhase::PreparingSession));

    // 3. Bootstrap SuperProperties. Prefer the exact `x-super-properties` we
    //    captured (the value the client actually sends); fall back to a fresh
    //    CDP fetch. Either way the manager has built-in defaults on failure.
    let mut super_properties_ready = false;
    if let Some(base64) = session.super_properties.as_ref() {
        if let Ok(decoded_bytes) = BASE64.decode(base64) {
            if let Ok(decoded) = serde_json::from_slice::<serde_json::Value>(&decoded_bytes) {
                if let Ok(mut manager) = SUPER_PROPERTIES_MANAGER.lock() {
                    manager.set_from_cdp(base64, &decoded);
                    super_properties_ready = true;
                }
            }
        }
    }
    if !super_properties_ready {
        if let Ok(cdp_result) = cdp_client::fetch_super_properties_via_cdp(cdp_port).await {
            if let Ok(mut manager) = SUPER_PROPERTIES_MANAGER.lock() {
                manager.set_from_cdp(&cdp_result.base64, &cdp_result.decoded);
            }
        }
    }

    // 4. Save the client last so no request runs with stale super properties.
    *state.authenticated_user.lock().unwrap() = Some(user.clone());
    *state.client.lock().unwrap() = Some(client);

    log(
        LogLevel::Info,
        LogCategory::TokenExtraction,
        "CDP auto-login succeeded",
        None,
    );

    let _ = on_progress.send(AuthProgress::phase(AuthProgressPhase::Complete));

    Ok(user)
}

/// Refuse CDP mutations when Helper's authenticated account differs from the
/// account currently open in the selected desktop client. Without this guard,
/// injection can affect account B while progress polling still targets A.
async fn ensure_cdp_account_consistency(
    state: &State<'_, AppState>,
    cdp_port: u16,
) -> Result<(), String> {
    let expected = state
        .authenticated_user
        .lock()
        .map_err(|_| "Authenticated account state is unavailable".to_string())?
        .clone()
        .ok_or_else(|| "Not logged in".to_string())?;

    let session = cdp_client::capture_discord_auth_via_cdp(
        cdp_port,
        std::time::Duration::from_secs(8),
    )
    .await
    .map_err(|error| {
        format!(
            "Could not verify the account open in the desktop client on CDP port {cdp_port}: {error}"
        )
    })?;
    let proxy = resolve_proxy_configuration(state).await?;
    let client = DiscordApiClient::new_with_proxy(session.authorization.to_string(), proxy)
        .map_err(|error| format!("Could not validate the desktop client account: {error}"))?;
    let actual = client
        .get_current_user()
        .await
        .map_err(|error| format!("Could not read the desktop client account: {error}"))?;
    if actual.id == expected.id {
        return Ok(());
    }

    let owner = match discord_cdp_launch_core::inspect_cdp_port_owner(cdp_port) {
        discord_cdp_launch_core::CdpPortOwner::Official => "Discord",
        discord_cdp_launch_core::CdpPortOwner::Vesktop => "Vesktop",
        discord_cdp_launch_core::CdpPortOwner::None => "the selected desktop client",
        discord_cdp_launch_core::CdpPortOwner::Other => "an unrecognized desktop client",
    };
    let expected_name = expected
        .global_name
        .as_deref()
        .unwrap_or(&expected.username);
    let actual_name = actual.global_name.as_deref().unwrap_or(&actual.username);
    Err(format!(
        "account_mismatch: Helper is signed in as {expected_name} ({}), but {owner} is signed in as {actual_name} ({}). Sign both into the same account before starting a CDP task.",
        expected.id, actual.id
    ))
}

/// Get quest list (via HTTP API /quests/@me endpoint)
#[tauri::command]
async fn get_quests(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let client = {
        let guard = state.client.lock().unwrap();
        guard
            .as_ref()
            .ok_or_else(|| "Not logged in".to_string())?
            .clone()
    };

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
    let client = {
        let guard = state.client.lock().unwrap();
        guard
            .as_ref()
            .ok_or_else(|| "Not logged in".to_string())?
            .clone()
    };

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
    let client = {
        let guard = state.client.lock().unwrap();
        guard
            .as_ref()
            .ok_or_else(|| "Not logged in".to_string())?
            .clone()
    };

    // Preserve today's replacement UX only for the legacy wrapper.
    if preempt {
        stop_active_work_internal(state).await?;
    }

    let kind = QuestKind::Video;
    let transport = QuestTransport::Rest;
    let worker_handle = app_handle.clone();
    let worker_quest_id = quest_id.clone();
    admit_quest_run(
        state,
        app_handle,
        quest_id,
        kind,
        transport,
        Box::new(move |guards, cancel_watch, progress| {
            let app_handle = worker_handle;
            let quest_id = worker_quest_id;
            Box::pin(async move {
                let _guards = guards;
                let cancelled = cancel_watch.clone();
                let cancel_rx = bridge_cancel(cancel_watch);
                let emitter = QuestEventSink::new(app_handle, progress, quest_id.clone());
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
    let client = {
        let guard = state.client.lock().unwrap();
        guard
            .as_ref()
            .ok_or_else(|| "Not logged in".to_string())?
            .clone()
    };

    if preempt {
        stop_active_work_internal(state).await?;
    }

    let kind = QuestKind::Stream;
    let transport = QuestTransport::Rest;
    let worker_handle = app_handle.clone();
    let worker_quest_id = quest_id.clone();
    admit_quest_run(
        state,
        app_handle,
        quest_id,
        kind,
        transport,
        Box::new(move |guards, cancel_watch, progress| {
            let app_handle = worker_handle;
            let quest_id = worker_quest_id;
            Box::pin(async move {
                let _guards = guards;
                let cancelled = cancel_watch.clone();
                let cancel_rx = bridge_cancel(cancel_watch);
                let emitter = QuestEventSink::new(app_handle, progress, quest_id.clone());
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
    let client = {
        let guard = state.client.lock().unwrap();
        guard
            .as_ref()
            .ok_or_else(|| "Not logged in".to_string())?
            .clone()
    };

    if preempt {
        stop_active_work_internal(state).await?;
    }

    let kind = QuestKind::Game;
    let transport = QuestTransport::Rest;
    let worker_handle = app_handle.clone();
    let worker_quest_id = quest_id.clone();
    admit_quest_run(
        state,
        app_handle,
        quest_id,
        kind,
        transport,
        Box::new(move |guards, cancel_watch, progress| {
            let app_handle = worker_handle;
            let quest_id = worker_quest_id;
            Box::pin(async move {
                let _guards = guards;
                let cancelled = cancel_watch.clone();
                let cancel_rx = bridge_cancel(cancel_watch);
                let emitter = QuestEventSink::new(app_handle, progress, quest_id.clone());
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

    let client = state.client.lock().unwrap().clone();
    if transport == PlayActivityTransport::DirectApi && client.is_none() {
        return Err("Not logged in".to_string());
    }

    if preempt {
        stop_active_work_internal(state).await?;
    }
    if transport == PlayActivityTransport::Cdp {
        ensure_cdp_account_consistency(state, cdp_port).await?;
    }

    let kind = QuestKind::PlayActivity;
    let quest_transport = match transport {
        PlayActivityTransport::Cdp => QuestTransport::Cdp { port: cdp_port },
        PlayActivityTransport::DirectApi => QuestTransport::Rest,
    };
    let worker_handle = app_handle.clone();
    let worker_quest_id = quest_id.clone();
    admit_quest_run(
        state,
        app_handle,
        quest_id,
        kind,
        quest_transport,
        Box::new(move |guards, cancel_watch, progress| {
            let app_handle = worker_handle;
            let quest_id = worker_quest_id;
            Box::pin(async move {
                let _guards = guards;
                let cancelled = cancel_watch.clone();
                let cancel_rx = bridge_cancel(cancel_watch);
                let emitter = QuestEventSink::new(app_handle, progress, quest_id.clone());
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
            })
        }),
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

    if preempt {
        stop_active_work_internal(state).await?;
    }
    ensure_cdp_account_consistency(state, cdp_port).await?;

    let quest_transport = QuestTransport::Cdp { port: cdp_port };
    // Clone the API client for progress polling (play/stream quests)
    let client = state.client.lock().unwrap().clone();
    let worker_quest_id = quest_id.clone();
    let worker_quest_type = quest_type.clone();
    let worker_handle = app_handle.clone();

    admit_quest_run(
        state,
        app_handle,
        quest_id,
        kind,
        quest_transport,
        Box::new(move |guards, cancel_watch, progress| {
            let app_handle = worker_handle;
            let quest_id = worker_quest_id;
            let quest_type = worker_quest_type;
            Box::pin(async move {
                let _guards = guards;
                let cancelled = cancel_watch.clone();
                let cancel_rx = bridge_cancel(cancel_watch);
                let emitter = QuestEventSink::new(app_handle, progress, quest_id.clone());
                let result = match quest_type.as_str() {
                    "play" => {
                        cdp_quest::complete_play_quest_via_cdp(
                            cdp_port,
                            quest_id,
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
                            quest_id,
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
                            quest_id,
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
                            quest_id,
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
            })
        }),
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

/// List every live quest run for the future parallel UI.
#[tauri::command]
async fn list_quest_runs(state: State<'_, AppState>) -> Result<Vec<QuestRunDto>, String> {
    let account_id = current_account_id(&state)?;
    Ok(state
        .quests
        .snapshot()
        .iter()
        .map(|control| quest_run_dto(control, &account_id))
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
    let result = match state.quests.signal_stop(&quest_id, run_id.as_deref()) {
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

/// Signal every run first, then await them concurrently.
#[tauri::command]
async fn stop_all_quests(state: State<'_, AppState>) -> Result<StopAllResult, String> {
    Ok(stop_all_quests_internal(&state).await)
}

async fn stop_all_quests_internal(state: &State<'_, AppState>) -> StopAllResult {
    let results = quest_runtime::stop_all_runs(state.quests.as_ref(), QUEST_STOP_WAIT).await;
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

/// Boxed worker future produced by a [`QuestWorkerFactory`].
type QuestWorkerFuture = std::pin::Pin<Box<dyn std::future::Future<Output = QuestOutcome> + Send>>;

/// Boxed worker factory. The shared admit path is generic over this so legacy
/// and non-preemptive start commands cannot drift in how they register a run.
type QuestWorkerFactory = Box<
    dyn FnOnce(
            Vec<ResourceGuard>,
            tokio::sync::watch::Receiver<bool>,
            Arc<std::sync::atomic::AtomicU64>,
        ) -> QuestWorkerFuture
        + Send,
>;

/// Build the same DTO `list_quest_runs` returns, so a newly admitted run is
/// immediately observable through that command.
fn quest_run_dto(control: &quest_runtime::QuestControl, account_id: &str) -> QuestRunDto {
    QuestRunDto {
        account_id: account_id.to_string(),
        quest_id: control.quest_id.clone(),
        run_id: control.run_id.to_string(),
        kind: control.kind.as_str().to_string(),
        transport: control.transport.as_str(),
        phase: control.phase().as_str().to_string(),
        progress: control.progress(),
    }
}

fn current_account_id(state: &State<'_, AppState>) -> Result<String, String> {
    Ok(state
        .authenticated_user
        .lock()
        .map_err(|_| "Authenticated account state is unavailable".to_string())?
        .as_ref()
        .map(|user| user.id.clone())
        .unwrap_or_default())
}

/// Shared admit + monitor setup for every quest start. It never preempts; the
/// caller decides whether to stop existing work first, so the legacy and
/// non-preemptive APIs share exactly one admission path.
async fn admit_quest_run(
    state: &State<'_, AppState>,
    app_handle: tauri::AppHandle,
    quest_id: String,
    kind: QuestKind,
    transport: QuestTransport,
    make_worker: QuestWorkerFactory,
) -> Result<QuestRunDto, String> {
    let account_id = current_account_id(state)?;
    let admitted = quest_runtime::admit_run(
        state.quests.as_ref(),
        state.resources.as_ref(),
        quest_id,
        kind,
        transport,
        kind.required_resources(transport),
        make_worker,
    )
    .await
    .map_err(|error| error.to_string())?;

    let dto = quest_run_dto(&admitted.control, &account_id);
    spawn_quest_monitor(state.quests.clone(), admitted, app_handle);
    Ok(dto)
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

/// The one and only terminal-event emission point for a quest run.
fn emit_terminal_event(
    app_handle: &tauri::AppHandle,
    control: &quest_runtime::QuestControl,
    outcome: QuestOutcome,
) {
    match outcome {
        QuestOutcome::Completed => {
            let _ = app_handle.emit("quest-complete", ());
        }
        QuestOutcome::Stopped => {
            let _ = app_handle.emit("quest-stopped", ());
        }
        QuestOutcome::Failed(message) => {
            let label = match control.kind {
                QuestKind::Video => "Video quest: ",
                QuestKind::Stream => "Stream quest: ",
                QuestKind::Game => "Game heartbeat quest: ",
                QuestKind::PlayActivity => "PLAY_ACTIVITY quest: ",
                QuestKind::EmbeddedActivity => "CDP quest: ",
            };
            let _ = app_handle.emit("quest-error", format!("{label}{message}"));
        }
    }
}

fn ensure_no_active_quest(state: &AppState) -> Result<(), String> {
    if state.quests.has_live_runs() {
        return Err(
            "Stop the active quest before starting a manual CDP game simulation".to_string(),
        );
    }
    Ok(())
}

async fn stop_manual_cdp_game_simulation_internal(
    state: &State<'_, AppState>,
) -> Result<(), String> {
    // Keep the lock for the full verified cleanup so a concurrent start cannot
    // install a new spoof between cleanup and clearing the saved session.
    let mut sessions = state.manual_cdp_game.lock().await;
    let Some(session) = sessions.active() else {
        return Ok(());
    };

    let cleanup_result = cdp_quest::stop_manual_game_spoof(session.cdp_port)
        .await
        .map_err(|error| {
            format!(
                "Failed to stop manual CDP game simulation: {error}. Restart Discord if the simulated game remains visible."
            )
        });
    // Resources are released only after verified cleanup succeeds.
    sessions.finish_cleanup(cleanup_result)
}

async fn stop_active_work_internal(state: &State<'_, AppState>) -> Result<(), String> {
    let _ = stop_all_quests_internal(state).await;
    stop_manual_cdp_game_simulation_internal(state).await
}

/// Navigate Discord client SPA to a specific path (no reload)
#[tauri::command]
async fn navigate_discord_spa(target_path: String, cdp_port: u16) -> Result<(), String> {
    cdp_quest::navigate_discord_spa(cdp_port, &target_path)
        .await
        .map_err(|e| format!("Failed to navigate Discord SPA: {}", e))
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

    // Refuse while any quest run is live so a manual spoof cannot inject a
    // second Discord activity.
    ensure_no_active_quest(&state)?;

    // Reserve account activity and the CDP port for the whole spoof lifetime.
    let required = [
        QuestResource::AccountActivity,
        QuestResource::CdpPort(cdp_port),
    ];
    let guards = state
        .resources
        .try_acquire_all(&required)
        .map_err(|error| error.to_string())?;

    let mut sessions = state.manual_cdp_game.lock().await;
    sessions.ensure_idle()?;

    let status = cdp_client::check_cdp_available(cdp_port).await;
    if !status.connected {
        return Err(status
            .error
            .unwrap_or_else(|| format!("Discord CDP is not connected on port {cdp_port}")));
    }

    ensure_cdp_account_consistency(&state, cdp_port).await?;

    cdp_quest::start_manual_game_spoof(cdp_port, &app_id, &app_name)
        .await
        .map_err(|error| format!("Failed to start manual CDP game simulation: {error}"))?;

    let session = ManualCdpGameSimulation {
        app_id,
        app_name,
        cdp_port,
    };
    sessions.activate(session.clone(), guards);
    Ok(session)
}

/// Stop and fully verify cleanup of the current manual CDP game simulation.
#[tauri::command]
async fn stop_manual_cdp_game_simulation(state: State<'_, AppState>) -> Result<(), String> {
    stop_manual_cdp_game_simulation_internal(&state).await
}

/// Return the backend-owned manual CDP game simulation, if one is active.
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
    let runtime = state.proxy.clone();
    let configured = tokio::task::spawn_blocking(move || {
        runtime.set(input, &discord_api::validate_proxy_configuration)
    })
    .await
    .map_err(|error| format!("Proxy settings task failed: {error}"))?
    .map_err(|error| error.to_string())?;

    let (dto, configuration) = configured;
    apply_proxy_to_active_client(&state, &configuration)?;
    Ok(dto)
}

/// Delete any saved proxy credential and persist the credential-free state.
#[tauri::command]
async fn clear_proxy_credentials(state: State<'_, AppState>) -> Result<ProxySettingsDto, String> {
    let runtime = state.proxy.clone();
    let configured = tokio::task::spawn_blocking(move || {
        runtime.clear_credentials(&discord_api::validate_proxy_configuration)
    })
    .await
    .map_err(|error| format!("Proxy settings task failed: {error}"))?
    .map_err(|error| error.to_string())?;

    let (dto, configuration) = configured;
    apply_proxy_to_active_client(&state, &configuration)?;
    Ok(dto)
}

/// Send one unauthenticated request through the effective policy to a fixed
/// Discord endpoint. Redirects are disabled and nothing is saved.
#[tauri::command]
async fn test_proxy_connection(state: State<'_, AppState>) -> Result<ProxyTestResult, String> {
    let runtime = state.proxy.clone();
    let configuration = tokio::task::spawn_blocking(move || runtime.resolve_current_for_login())
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
    let auth_client = {
        let guard = state.client.lock().unwrap();
        guard.as_ref().cloned()
    };

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
    let client = {
        let guard = state.client.lock().unwrap();
        guard
            .as_ref()
            .ok_or_else(|| "Not logged in".to_string())?
            .clone()
    };

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
    let client = {
        let guard = state.client.lock().unwrap();
        guard
            .as_ref()
            .ok_or_else(|| "Not logged in".to_string())?
            .clone()
    };

    client
        .get_virtual_currency_balance()
        .await
        .map_err(|e| format!("Failed to get virtual currency balance: {}", e))
}

#[tauri::command]
async fn get_billing_subscriptions(
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let client = {
        let guard = state.client.lock().unwrap();
        guard
            .as_ref()
            .ok_or_else(|| "Not logged in".to_string())?
            .clone()
    };

    client
        .get_billing_subscriptions()
        .await
        .map_err(|e| format!("Failed to get billing subscriptions: {}", e))
}

#[tauri::command]
async fn get_program_rewards(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let client = {
        let guard = state.client.lock().unwrap();
        guard
            .as_ref()
            .ok_or_else(|| "Not logged in".to_string())?
            .clone()
    };

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
    let client = {
        let guard = state.client.lock().unwrap();
        guard
            .as_ref()
            .ok_or_else(|| "Not logged in".to_string())?
            .clone()
    };

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
    let client = {
        let guard = state.client.lock().unwrap();
        guard
            .as_ref()
            .ok_or_else(|| "Not logged in".to_string())?
            .clone()
    };

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
    let client = {
        let guard = state.client.lock().unwrap();
        guard
            .as_ref()
            .ok_or_else(|| "Not logged in".to_string())?
            .clone()
    };

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
            app.manage(AppState {
                client: Mutex::new(None),
                authenticated_user: Mutex::new(None),
                quests: Arc::new(QuestRegistry::new()),
                resources: Arc::new(ResourceCoordinator::new()),
                manual_cdp_game: tokio::sync::Mutex::new(ManualCdpGameSessionState::default()),
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
            get_proxy_settings,
            set_proxy_settings,
            clear_proxy_credentials,
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

async fn prepare_active_work_and_local_cleanup(state: &State<'_, AppState>) -> Result<(), String> {
    if APP_EXIT_CLEANUP.is_prepared() {
        return Ok(());
    }

    // Manual CDP injections must be removed while the Discord targets are
    // still reachable. Preserve any error until the remaining local cleanup
    // has run so an RPC/game cleanup failure cannot strand another resource.
    // Do not mark exit prepared until this cleanup succeeds; otherwise a
    // later prepare_app_exit (or a retried close) would skip rollback.
    let active_work_error = stop_active_work_internal(state).await.err();
    cleanup_local_resources_on_exit().await;
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
async fn start_discord_normal_restore_helper(app_handle: tauri::AppHandle) -> Result<(), String> {
    let launcher = find_bundled_cdp_launcher(&app_handle)?;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    match runtime_bridge::verify_bundled_for_execution(&launcher) {
        Ok(()) => runtime_identity::record_helper_identity(Ok(())),
        Err(error) => {
            runtime_identity::record_helper_identity(Err(error.clone()));
            return Err(error);
        }
    }
    tauri::async_runtime::spawn_blocking(move || spawn_restore_helper(&launcher))
        .await
        .map_err(|error| format!("Discord restore helper task failed: {error}"))?
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

    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Failed to start Discord restore helper: {error}"))
}

/// Force update video progress (used for ensuring final progress is saved on stop)
#[tauri::command]
async fn force_video_progress(
    quest_id: String,
    timestamp: f64,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let client = {
        let guard = state.client.lock().unwrap();
        guard
            .as_ref()
            .ok_or_else(|| "Not logged in".to_string())?
            .clone()
    };

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

/// Get debug info including X-Super-Properties
#[tauri::command]
async fn get_debug_info() -> Result<super_properties::DebugInfo, String> {
    let manager = SUPER_PROPERTIES_MANAGER.lock().map_err(|e| e.to_string())?;
    Ok(manager.get_debug_info())
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

/// Fetch SuperProperties via CDP
#[tauri::command]
async fn fetch_super_properties_cdp(
    port: Option<u16>,
) -> Result<cdp_client::CdpSuperProperties, String> {
    let port = port.unwrap_or(cdp_client::DEFAULT_CDP_PORT);
    let result = cdp_client::fetch_super_properties_via_cdp(port)
        .await
        .map_err(|e| e.to_string())?;

    // Update global SuperProperties Manager
    if let Ok(mut manager) = SUPER_PROPERTIES_MANAGER.lock() {
        manager.set_from_cdp(&result.base64, &result.decoded);
    }

    Ok(result)
}

/// Read Discord's currently loaded game detector state via CDP.
#[tauri::command]
async fn fetch_running_games_cdp(
    port: Option<u16>,
) -> Result<cdp_client::CdpRunningGamesSnapshot, String> {
    let port = port.unwrap_or(cdp_client::DEFAULT_CDP_PORT);
    cdp_client::fetch_running_games_via_cdp(port)
        .await
        .map_err(|e| e.to_string())
}

/// Capture Discord API request headers via CDP Network interception
#[tauri::command]
async fn capture_discord_headers_cdp(
    port: Option<u16>,
    duration_secs: Option<u64>,
) -> Result<cdp_client::CdpCapturedHeaders, String> {
    let port = port.unwrap_or(cdp_client::DEFAULT_CDP_PORT);
    let duration = duration_secs.unwrap_or(30);
    let captured = cdp_client::capture_discord_headers_via_cdp(port, duration)
        .await
        .map_err(|e| e.to_string())?;

    let mut manager = SUPER_PROPERTIES_MANAGER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for request in &captured.requests {
        manager.update_header_profile_from_headers(&request.headers);
    }

    Ok(captured)
}

/// Get current SuperProperties source mode and build number
#[tauri::command]
fn get_super_properties_mode() -> serde_json::Value {
    let manager = SUPER_PROPERTIES_MANAGER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    serde_json::json!({
        "mode": manager.get_mode().as_str(),
        "mode_display": manager.get_mode().display_name(),
        "build_number": manager.get_build_number()
    })
}

/// Auto-fetch SuperProperties with fallback: CDP -> Default
#[tauri::command]
async fn auto_fetch_super_properties(cdp_port: Option<u16>) -> serde_json::Value {
    use crate::logger::{log, LogCategory, LogLevel};

    let port = cdp_port.unwrap_or(cdp_client::DEFAULT_CDP_PORT);

    // Priority 1: Try CDP
    log(
        LogLevel::Info,
        LogCategory::TokenExtraction,
        &format!("Auto-fetching SuperProperties, trying CDP on port {}", port),
        None,
    );

    if let Ok(cdp_result) = cdp_client::fetch_super_properties_via_cdp(port).await {
        if let Ok(mut manager) = SUPER_PROPERTIES_MANAGER.lock() {
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
            return serde_json::json!({
                "success": true,
                "mode": "cdp",
                "build_number": manager.get_build_number()
            });
        }
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

    // Priority 3: Use default values
    let build_number = if let Ok(manager) = SUPER_PROPERTIES_MANAGER.lock() {
        manager.get_build_number()
    } else {
        None
    };

    serde_json::json!({
        "success": false,
        "mode": "default",
        "build_number": build_number
    })
}

/// Retry fetching SuperProperties (resets and tries again)
#[tauri::command]
async fn retry_super_properties(cdp_port: Option<u16>) -> serde_json::Value {
    // Reset state
    if let Ok(mut manager) = SUPER_PROPERTIES_MANAGER.lock() {
        manager.reset();
    }

    // Retry fetch
    auto_fetch_super_properties(cdp_port).await
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
