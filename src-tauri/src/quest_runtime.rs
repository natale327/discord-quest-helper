//! Quest run registry plus cross-quest resource coordination.
//!
//! This module replaces the previous single-quest `Option<QuestState>` slot with
//! a registry of live runs and a set of typed, non-blocking resource locks. It
//! is intentionally independent of any specific quest loop: callers admit a run
//! by handing `admit_run` a worker factory, and observe completion through
//! [`monitor_run`].
//!
//! Concurrency policy encoded here:
//! - REST video quests hold no exclusive resource and may run concurrently for
//!   different quest ids.
//! - Stream / game-heartbeat / PLAY_ACTIVITY / embedded-activity / manual CDP
//!   game spoof runs hold [`QuestResource::AccountActivity`]: at most one owner
//!   per account.
//! - CDP mutation is authorized by the unified `cdp_port_lease::CdpPortLease`,
//!   not by this coordinator; a run holds its lease in addition to these guards.
//! - [`QuestResource::ProcessSimulation`] and [`QuestResource::DiscordRpc`] are
//!   globally exclusive and reserved for the process-simulation and RPC paths.

use crate::models::AccountId;
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::Emitter;
use tokio::sync::{watch, Mutex as AsyncMutex, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};
use tokio::task::{AbortHandle, JoinHandle};

pub type QuestId = String;
pub type RunId = uuid::Uuid;
/// Registry key: one live run per (account, quest id).
pub type QuestKey = (AccountId, QuestId);

/// Logical category of a quest run. Used for reporting and error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QuestKind {
    Video,
    Stream,
    Game,
    PlayActivity,
    EmbeddedActivity,
}

impl QuestKind {
    pub fn as_str(self) -> &'static str {
        match self {
            QuestKind::Video => "video",
            QuestKind::Stream => "stream",
            QuestKind::Game => "game",
            QuestKind::PlayActivity => "playActivity",
            QuestKind::EmbeddedActivity => "embeddedActivity",
        }
    }

    /// The exclusive resources a run of this kind requires for its whole
    /// lifetime. REST video runs deliberately require nothing so they can run
    /// concurrently for different quest ids.
    ///
    /// CDP port exclusion is deliberately NOT a `QuestResource`: the unified
    /// `CdpPortLease` is the sole authority for a port and is held separately (and
    /// for the whole worker lifetime) by CDP paths. The transport argument is
    /// retained for call-site compatibility.
    pub fn required_resources(self, _transport: QuestTransport) -> Vec<QuestResource> {
        let mut resources = Vec::new();
        match self {
            QuestKind::Video => {}
            QuestKind::Stream
            | QuestKind::Game
            | QuestKind::PlayActivity
            | QuestKind::EmbeddedActivity => resources.push(QuestResource::AccountActivity),
        }
        resources
    }
}

/// How a run talks to Discord.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestTransport {
    Rest,
    Cdp {
        port: u16,
    },
    #[allow(dead_code)]
    ProcessSimulation,
}

impl QuestTransport {
    pub fn as_str(self) -> String {
        match self {
            QuestTransport::Rest => "rest".to_string(),
            QuestTransport::Cdp { port } => format!("cdp:{port}"),
            QuestTransport::ProcessSimulation => "processSimulation".to_string(),
        }
    }
}

/// A typed, exclusive resource. Acquired without waiting; a busy resource is
/// reported as an error rather than queued so no run appears active while
/// silently waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QuestResource {
    AccountActivity,
    #[allow(dead_code)]
    ProcessSimulation,
    #[allow(dead_code)]
    DiscordRpc,
}

impl QuestResource {
    fn rank(self) -> u8 {
        match self {
            QuestResource::AccountActivity => 0,
            QuestResource::ProcessSimulation => 2,
            QuestResource::DiscordRpc => 3,
        }
    }
}

impl fmt::Display for QuestResource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QuestResource::AccountActivity => write!(formatter, "account_activity"),
            QuestResource::ProcessSimulation => write!(formatter, "process_simulation"),
            QuestResource::DiscordRpc => write!(formatter, "discord_rpc"),
        }
    }
}

/// A non-blocking resource acquisition failure naming the busy resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceBusyError(pub QuestResource);

impl fmt::Display for ResourceBusyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "resource_busy: {} is already in use", self.0)
    }
}

/// Admission failures. `Duplicate` is a live `quest_id`; `ResourceBusy` means a
/// required resource lock could not be taken without waiting.
#[derive(Debug)]
pub enum AdmitError {
    Duplicate(QuestId),
    ResourceBusy(ResourceBusyError),
}

impl fmt::Display for AdmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdmitError::Duplicate(quest_id) => {
                write!(formatter, "quest {quest_id} is already running")
            }
            AdmitError::ResourceBusy(error) => write!(formatter, "{error}"),
        }
    }
}

/// Terminal state of a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestOutcome {
    Completed,
    Stopped,
    Failed(String),
}

/// RAII guard for one acquired resource. Dropping it releases the lock, which
/// is why it must be held by the worker until cleanup has fully finished.
#[derive(Debug)]
pub enum ResourceGuard {
    AccountActivity(OwnedMutexGuard<()>),
    ProcessSimulation(OwnedMutexGuard<()>),
    DiscordRpc(OwnedMutexGuard<()>),
}

/// Account-wide 429 `Retry-After` backoff. Deliberately does not retry on its
/// own; callers decide whether to wait and retry. Only the *longest* reported
/// deadline is retained so a later, shorter 429 cannot release a request early.
pub struct RateLimitCoordinator {
    retry_after_until: Mutex<Option<Instant>>,
}

impl RateLimitCoordinator {
    pub fn new() -> Self {
        Self {
            retry_after_until: Mutex::new(None),
        }
    }

    /// Record a Discord 429 `Retry-After` that applies to the whole account.
    /// Never shortens an existing deadline: the later of the two instants wins.
    pub fn note_retry_after(&self, retry_after: Duration) {
        let now = Instant::now();
        // `checked_add` only fails for absurd durations; fall back to a far but
        // finite deadline rather than panicking.
        let until = now
            .checked_add(retry_after)
            .unwrap_or_else(|| now + Duration::from_secs(60 * 60));

        let mut guard = self
            .retry_after_until
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match *guard {
            Some(existing) if existing >= until => {}
            _ => *guard = Some(until),
        }
    }

    /// Wait for any account-wide backoff to elapse before issuing a request.
    ///
    /// Re-checks after every sleep because a concurrent 429 may push the
    /// deadline further out while this caller is waiting.
    pub async fn wait_for_clearance(&self) {
        loop {
            let wait = {
                let guard = self
                    .retry_after_until
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard
                    .map(|until| until.saturating_duration_since(Instant::now()))
                    .unwrap_or_default()
            };
            if wait.is_zero() {
                return;
            }
            tokio::time::sleep(wait).await;
        }
    }
}

impl Default for RateLimitCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

/// Cloneable, account-local request gate: at most two in-flight API requests
/// plus the account-wide 429 backoff.
///
/// The gate is shared by every client/run of one account (via
/// [`ResourceCoordinator::request_gate`]) so capacity and backoff are account
/// scoped. [`AccountRequestGate::standalone`] creates an isolated gate for
/// short-lived callers and tests.
#[derive(Clone)]
pub struct AccountRequestGate {
    in_flight: Arc<Semaphore>,
    rate_limits: Arc<RateLimitCoordinator>,
}

impl AccountRequestGate {
    /// An isolated gate with its own capacity and backoff state.
    pub fn standalone() -> Self {
        Self {
            in_flight: Arc::new(Semaphore::new(2)),
            rate_limits: Arc::new(RateLimitCoordinator::new()),
        }
    }

    /// Acquire one in-flight slot, then wait out any reported backoff *while
    /// holding it*. Acquiring the slot first means a request queued behind
    /// capacity cannot miss a 429 reported while it waits for a permit.
    pub async fn acquire(&self) -> AccountRequestPermit {
        let permit = self
            .in_flight
            .clone()
            .acquire_owned()
            .await
            .expect("request gate semaphore is never closed");
        self.rate_limits.wait_for_clearance().await;
        AccountRequestPermit { _permit: permit }
    }

    /// Record a Discord 429 `Retry-After` for this account.
    pub fn note_retry_after(&self, retry_after: Duration) {
        self.rate_limits.note_retry_after(retry_after);
    }
}

impl Default for AccountRequestGate {
    fn default() -> Self {
        Self::standalone()
    }
}

/// Opaque RAII permit for one account-local in-flight request slot. Dropping it
/// returns the slot to the gate.
#[derive(Debug)]
pub struct AccountRequestPermit {
    _permit: OwnedSemaphorePermit,
}

/// Account-local throttling state: account-activity exclusivity plus the shared
/// request gate (capacity and 429 backoff), all scoped to one account.
struct AccountResources {
    account_activity: Arc<AsyncMutex<()>>,
    request_gate: AccountRequestGate,
}

impl AccountResources {
    fn new() -> Self {
        Self {
            account_activity: Arc::new(AsyncMutex::new(())),
            request_gate: AccountRequestGate::standalone(),
        }
    }
}

/// Owns every exclusive resource a quest can require.
///
/// Account-local resources (`AccountActivity`, request-gate capacity/429
/// state) are separated per account. `ProcessSimulation` and `DiscordRpc`
/// remain process-global. CDP port exclusion lives in the unified
/// `cdp_port_lease::CdpPortLease`, not here.
pub struct ResourceCoordinator {
    accounts: Mutex<HashMap<AccountId, Arc<AccountResources>>>,
    process_simulation: Arc<AsyncMutex<()>>,
    discord_rpc: Arc<AsyncMutex<()>>,
}

impl ResourceCoordinator {
    pub fn new() -> Self {
        Self {
            accounts: Mutex::new(HashMap::new()),
            process_simulation: Arc::new(AsyncMutex::new(())),
            discord_rpc: Arc::new(AsyncMutex::new(())),
        }
    }

    /// This account's shared request gate. Repeated calls for one account return
    /// handles to the same gate (shared capacity and backoff); different account
    /// ids get independent gates.
    pub fn request_gate(&self, account_id: &AccountId) -> AccountRequestGate {
        self.account_resources(account_id).request_gate.clone()
    }

    fn account_resources(&self, account_id: &AccountId) -> Arc<AccountResources> {
        let mut accounts = self
            .accounts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        accounts
            .entry(account_id.clone())
            .or_insert_with(|| Arc::new(AccountResources::new()))
            .clone()
    }

    /// Try to take all `required` resources for `account_id` without waiting. On
    /// failure any already-acquired guards are dropped automatically, releasing
    /// them for the correct account.
    ///
    /// Canonical acquisition order (enforced by `QuestResource::rank`):
    /// account-activity (account-local) -> process simulation (global) -> Discord
    /// RPC (global). No map mutex is held across an await; the per-account mutex
    /// Arc is cloned out first.
    pub fn try_acquire_all(
        &self,
        account_id: &AccountId,
        required: &[QuestResource],
    ) -> Result<Vec<ResourceGuard>, ResourceBusyError> {
        let account_resources = self.account_resources(account_id);

        let mut ordered = required.to_vec();
        ordered.sort_by_key(|resource| resource.rank());
        ordered.dedup();

        let mut guards = Vec::with_capacity(ordered.len());
        for resource in ordered {
            guards.push(self.try_acquire(resource, &account_resources)?);
        }
        Ok(guards)
    }

    fn try_acquire(
        &self,
        resource: QuestResource,
        account_resources: &Arc<AccountResources>,
    ) -> Result<ResourceGuard, ResourceBusyError> {
        match resource {
            QuestResource::AccountActivity => account_resources
                .account_activity
                .clone()
                .try_lock_owned()
                .map(ResourceGuard::AccountActivity)
                .map_err(|_| ResourceBusyError(resource)),
            QuestResource::ProcessSimulation => self
                .process_simulation
                .clone()
                .try_lock_owned()
                .map(ResourceGuard::ProcessSimulation)
                .map_err(|_| ResourceBusyError(resource)),
            QuestResource::DiscordRpc => self
                .discord_rpc
                .clone()
                .try_lock_owned()
                .map(ResourceGuard::DiscordRpc)
                .map_err(|_| ResourceBusyError(resource)),
        }
    }
}

impl Default for ResourceCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

/// Live phase of a run, derived from its cancel flag and terminal outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestPhase {
    Running,
    Stopping,
    Finished,
}

impl QuestPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            QuestPhase::Running => "running",
            QuestPhase::Stopping => "stopping",
            QuestPhase::Finished => "finished",
        }
    }
}

/// Everything the registry needs to describe and control one run.
#[derive(Debug)]
pub struct QuestControl {
    pub account_id: AccountId,
    pub run_id: RunId,
    pub quest_id: QuestId,
    pub kind: QuestKind,
    pub transport: QuestTransport,
    #[allow(dead_code)]
    pub resources: Vec<QuestResource>,
    #[allow(dead_code)]
    pub started_at: Instant,
    pub cancel: watch::Sender<bool>,
    pub done: watch::Receiver<Option<QuestOutcome>>,
    pub abort: AbortHandle,
    progress: Arc<AtomicU64>,
}

impl QuestControl {
    pub fn phase(&self) -> QuestPhase {
        if self.done.borrow().is_some() {
            QuestPhase::Finished
        } else if *self.cancel.borrow() {
            QuestPhase::Stopping
        } else {
            QuestPhase::Running
        }
    }

    pub fn progress(&self) -> f64 {
        f64::from_bits(self.progress.load(Ordering::Relaxed))
    }
}

/// Registry of live runs, keyed by (account id, quest id).
pub struct QuestRegistry {
    entries: Mutex<HashMap<QuestKey, Arc<QuestControl>>>,
    admission: AsyncMutex<()>,
}

impl QuestRegistry {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            admission: AsyncMutex::new(()),
        }
    }

    fn lock_entries(&self) -> std::sync::MutexGuard<'_, HashMap<QuestKey, Arc<QuestControl>>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Remove runs that already produced an outcome. Callers must hold
    /// `admission`.
    fn reap_finished_locked(&self) {
        self.lock_entries()
            .retain(|_, control| !is_finished(control));
    }

    pub fn snapshot(&self) -> Vec<Arc<QuestControl>> {
        self.lock_entries().values().cloned().collect()
    }

    /// Live runs owned by one account. Account-scoped so the active-account
    /// command surface cannot observe or collide with another account's runs.
    pub fn snapshot_for_account(&self, account_id: &AccountId) -> Vec<Arc<QuestControl>> {
        self.lock_entries()
            .iter()
            .filter(|((owner, _), _)| owner == account_id)
            .map(|(_, control)| control.clone())
            .collect()
    }

    /// Whether one account has any live run. Account-scoped so a guard for the
    /// active account is never coupled to another account's runs.
    pub fn has_live_runs_for_account(&self, account_id: &AccountId) -> bool {
        self.lock_entries()
            .iter()
            .any(|((owner, _), control)| owner == account_id && !is_finished(control))
    }

    /// Remove a run only when the `run_id` still matches. A monitor that lost a
    /// race with a newer run of the same quest must not evict it. Keyed by
    /// run_id (globally unique) so an account switch cannot evict the wrong run.
    pub fn finish(&self, run_id: &RunId) {
        self.lock_entries()
            .retain(|_, control| control.run_id != *run_id);
    }

    /// Request cancellation of one account's run. A stale `run_id` is rejected;
    /// an unknown or already finished (account, quest) is reported as not found
    /// so the caller can answer idempotently.
    pub fn signal_stop(
        &self,
        account_id: &AccountId,
        quest_id: &QuestId,
        run_id: Option<&str>,
    ) -> StopSignal {
        let key = (account_id.clone(), quest_id.clone());
        let control = self.lock_entries().get(&key).cloned();
        match control {
            None => StopSignal::NotFound,
            Some(control) if is_finished(&control) => StopSignal::NotFound,
            Some(control) => {
                if let Some(expected) = run_id {
                    if control.run_id.to_string() != expected {
                        return StopSignal::RunIdMismatch {
                            current: control.run_id,
                        };
                    }
                }
                let _ = control.cancel.send(true);
                StopSignal::Signalled(control)
            }
        }
    }
}

impl Default for QuestRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn is_finished(control: &QuestControl) -> bool {
    control.done.borrow().is_some() || control.abort.is_finished()
}

/// Result of a `signal_stop` request.
#[derive(Debug)]
pub enum StopSignal {
    Signalled(Arc<QuestControl>),
    NotFound,
    RunIdMismatch { current: RunId },
}

/// Result of waiting for a run's terminal outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoneWait {
    Finished(QuestOutcome),
    TimedOut,
}

/// How `stop_all_runs` classified a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopClass {
    Completed,
    TimedOut,
    CleanupFailed,
}

/// An admitted run: its control record, the worker task, and the sender used to
/// publish the terminal outcome. Consumed by [`monitor_run`].
pub struct AdmittedRun {
    pub control: Arc<QuestControl>,
    pub worker: JoinHandle<QuestOutcome>,
    done_tx: watch::Sender<Option<QuestOutcome>>,
}

/// A fully-specified quest admission request.
///
/// Grouping these fields keeps [`admit_run`] within the argument-count limit and
/// gives every call site one place to declare the run's account scope and the
/// exclusive resources it needs for its whole lifetime.
#[derive(Debug)]
pub struct QuestAdmission {
    pub account_id: AccountId,
    pub quest_id: QuestId,
    pub kind: QuestKind,
    pub transport: QuestTransport,
    pub required: Vec<QuestResource>,
}

/// Admit a run under the short admission mutex: reap finished entries, reject a
/// duplicate live (account, quest id), try-acquire resources without waiting,
/// then spawn the worker that owns those guards for its whole lifetime.
///
/// The admission mutex is dropped before the worker future is polled, so it is
/// never held for the life of a quest.
pub async fn admit_run<F, Fut>(
    registry: &QuestRegistry,
    resources: &ResourceCoordinator,
    admission: QuestAdmission,
    make_worker: F,
) -> Result<AdmittedRun, AdmitError>
where
    F: FnOnce(Vec<ResourceGuard>, watch::Receiver<bool>, Arc<AtomicU64>, RunId) -> Fut
        + Send
        + 'static,
    Fut: Future<Output = QuestOutcome> + Send + 'static,
{
    let QuestAdmission {
        account_id,
        quest_id,
        kind,
        transport,
        required,
    } = admission;

    let admission_guard = registry.admission.lock().await;
    registry.reap_finished_locked();

    // Duplicate rejection is per (account, quest): the same quest id may run
    // concurrently for two different accounts.
    let key = (account_id.clone(), quest_id.clone());
    if let Some(existing) = registry.lock_entries().get(&key).cloned() {
        if !is_finished(&existing) {
            return Err(AdmitError::Duplicate(quest_id));
        }
    }

    let guards = resources
        .try_acquire_all(&account_id, &required)
        .map_err(AdmitError::ResourceBusy)?;

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let (done_tx, done_rx) = watch::channel(None);
    let progress = Arc::new(AtomicU64::new(0));
    let run_id = RunId::new_v4();

    let worker = tokio::spawn(make_worker(guards, cancel_rx, progress.clone(), run_id));

    let control = Arc::new(QuestControl {
        account_id,
        run_id,
        quest_id,
        kind,
        transport,
        resources: required,
        started_at: Instant::now(),
        cancel: cancel_tx,
        done: done_rx,
        abort: worker.abort_handle(),
        progress,
    });

    registry.lock_entries().insert(key, control.clone());
    drop(admission_guard);

    Ok(AdmittedRun {
        control,
        worker,
        done_tx,
    })
}

/// Await a run's worker and emit exactly one terminal event through
/// `on_terminal`. A panic or task cancellation still produces a terminal
/// outcome, and the registry entry is removed only if the `run_id` still
/// matches (so a stale monitor cannot evict a newer run).
pub async fn monitor_run<F>(registry: &QuestRegistry, admitted: AdmittedRun, on_terminal: F)
where
    F: FnOnce(&QuestControl, QuestOutcome),
{
    let AdmittedRun {
        control,
        worker,
        done_tx,
    } = admitted;
    let run_id = control.run_id;

    let outcome = match worker.await {
        Ok(outcome) => outcome,
        Err(error) if error.is_cancelled() => {
            QuestOutcome::Failed("quest task was cancelled".to_string())
        }
        Err(error) => QuestOutcome::Failed(format!("quest task ended unexpectedly: {error}")),
    };

    // A user-requested stop wins over a worker that reported completion in the
    // same instant.
    let outcome = if *control.cancel.borrow() && outcome == QuestOutcome::Completed {
        QuestOutcome::Stopped
    } else {
        outcome
    };

    let _ = done_tx.send(Some(outcome.clone()));
    registry.finish(&run_id);
    on_terminal(&control, outcome);
}

/// Wait (bounded) for a run's terminal outcome. Cancel-safe: `watch::changed`
/// can be dropped and retried at any await point.
pub async fn wait_for_done(control: &QuestControl, timeout: Duration) -> DoneWait {
    let mut done = control.done.clone();
    if let Some(outcome) = (*done.borrow()).clone() {
        return DoneWait::Finished(outcome);
    }

    match tokio::time::timeout(timeout, done.changed()).await {
        Ok(Ok(())) | Ok(Err(_)) => match (*done.borrow()).clone() {
            Some(outcome) => DoneWait::Finished(outcome),
            None => DoneWait::Finished(QuestOutcome::Stopped),
        },
        Err(_) => DoneWait::TimedOut,
    }
}

/// Signal every run first, then await them concurrently. Runs that never finish
/// within `timeout` remain in the registry in the `Stopping` phase; they are
/// never aborted here.
pub async fn stop_all_runs(
    registry: &QuestRegistry,
    timeout: Duration,
) -> Vec<(QuestId, StopClass)> {
    let controls = registry.snapshot();
    signal_and_await(controls, timeout).await
}

/// Signal one account's runs first, then await them concurrently. Another
/// account's runs (even with identical quest ids) are never touched.
pub async fn stop_account_runs(
    registry: &QuestRegistry,
    account_id: &AccountId,
    timeout: Duration,
) -> Vec<(QuestId, StopClass)> {
    let controls = registry.snapshot_for_account(account_id);
    signal_and_await(controls, timeout).await
}

async fn signal_and_await(
    controls: Vec<Arc<QuestControl>>,
    timeout: Duration,
) -> Vec<(QuestId, StopClass)> {
    for control in &controls {
        let _ = control.cancel.send(true);
    }

    let futures = controls.into_iter().map(|control| async move {
        let class = match wait_for_done(&control, timeout).await {
            DoneWait::Finished(QuestOutcome::Failed(_)) => StopClass::CleanupFailed,
            DoneWait::Finished(_) => StopClass::Completed,
            DoneWait::TimedOut => StopClass::TimedOut,
        };
        (control.quest_id.clone(), class)
    });

    join_all(futures).await
}

/// Emits `quest-progress` events for a run and records the latest value for
/// `list_quest_runs`. Only progress events are emitted here; terminal events
/// belong exclusively to the run monitor.
#[derive(Clone)]
pub struct QuestEventSink {
    app: tauri::AppHandle,
    progress: Arc<AtomicU64>,
    account_id: AccountId,
    quest_id: QuestId,
    run_id: RunId,
}

impl QuestEventSink {
    /// Build a sink bound to the exact account/quest/run identity snapped at
    /// start time. Every emitted event carries those ids.
    pub fn new(
        app: tauri::AppHandle,
        progress: Arc<AtomicU64>,
        account_id: AccountId,
        quest_id: QuestId,
        run_id: RunId,
    ) -> Self {
        Self {
            app,
            progress,
            account_id,
            quest_id,
            run_id,
        }
    }

    pub fn quest_id(&self) -> &str {
        &self.quest_id
    }

    pub fn progress(&self, value: f64) {
        self.progress.store(value.to_bits(), Ordering::Relaxed);
        let envelope = crate::models::QuestEventEnvelope {
            account_id: self.account_id.as_str().to_string(),
            quest_id: self.quest_id.clone(),
            run_id: self.run_id.to_string(),
            progress: Some(value),
            message: None,
            kind: None,
        };
        let _ = self.app.emit("quest-progress", envelope);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex as StdMutex;
    use tokio::sync::oneshot;

    type TerminalLog = Arc<StdMutex<Vec<(QuestId, QuestOutcome)>>>;

    fn terminal_log() -> TerminalLog {
        Arc::new(StdMutex::new(Vec::new()))
    }

    /// Spawn a monitor that records the terminal outcome, mirroring the
    /// production call site.
    fn spawn_monitor(
        registry: Arc<QuestRegistry>,
        admitted: AdmittedRun,
        log: TerminalLog,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            monitor_run(&registry, admitted, move |control, outcome| {
                log.lock()
                    .unwrap()
                    .push((control.quest_id.clone(), outcome));
            })
            .await;
        })
    }

    type WorkerFuture = std::pin::Pin<Box<dyn Future<Output = QuestOutcome> + Send>>;
    type WorkerFactory = Box<
        dyn FnOnce(Vec<ResourceGuard>, watch::Receiver<bool>, Arc<AtomicU64>) -> WorkerFuture
            + Send,
    >;

    /// Worker that completes after `hold`, or reports `Stopped` on cancel.
    fn completing_worker(hold: Duration, cleaned: Option<Arc<AtomicBool>>) -> WorkerFactory {
        Box::new(move |guards, mut cancel, _progress| {
            Box::pin(async move {
                // Keep guards owned for the whole worker lifetime, even on cancel.
                let _guards = guards;
                tokio::select! {
                    _ = tokio::time::sleep(hold) => {
                        if let Some(flag) = cleaned.as_ref() {
                            flag.store(true, Ordering::SeqCst);
                        }
                        QuestOutcome::Completed
                    }
                    _ = cancel.changed() => {
                        // Simulate rollback/cleanup that must finish before the
                        // resource can be reused.
                        tokio::time::sleep(Duration::from_millis(40)).await;
                        if let Some(flag) = cleaned.as_ref() {
                            flag.store(true, Ordering::SeqCst);
                        }
                        QuestOutcome::Stopped
                    }
                }
            })
        })
    }

    fn required(kind: QuestKind, transport: QuestTransport) -> Vec<QuestResource> {
        kind.required_resources(transport)
    }

    const ACCOUNT_A_ID: &str = "111111111111111111";
    const ACCOUNT_B_ID: &str = "222222222222222222";

    fn account(id: &str) -> AccountId {
        AccountId::parse(id).expect("valid test account id")
    }

    fn account_a() -> AccountId {
        account(ACCOUNT_A_ID)
    }

    fn account_b() -> AccountId {
        account(ACCOUNT_B_ID)
    }

    /// Build an admission request whose resources are exactly what the kind and
    /// transport require.
    fn admission(
        account_id: AccountId,
        quest_id: &str,
        kind: QuestKind,
        transport: QuestTransport,
    ) -> QuestAdmission {
        QuestAdmission {
            account_id,
            quest_id: quest_id.to_string(),
            kind,
            transport,
            required: required(kind, transport),
        }
    }

    /// Account-parameterised admission used by the account-scoped tests.
    async fn admit_for<F, Fut>(
        admission: QuestAdmission,
        registry: &QuestRegistry,
        resources: &ResourceCoordinator,
        make_worker: F,
    ) -> Result<AdmittedRun, AdmitError>
    where
        F: FnOnce(Vec<ResourceGuard>, watch::Receiver<bool>, Arc<AtomicU64>) -> Fut
            + Send
            + 'static,
        Fut: Future<Output = QuestOutcome> + Send + 'static,
    {
        super::admit_run(
            registry,
            resources,
            admission,
            move |guards, cancel, progress, _run_id| make_worker(guards, cancel, progress),
        )
        .await
    }

    /// Test-local wrapper that shadows the glob-imported `admit_run` and always
    /// admits for account A, so the existing single-account tests stay terse.
    async fn admit_run<F, Fut>(
        registry: &QuestRegistry,
        resources: &ResourceCoordinator,
        quest_id: QuestId,
        kind: QuestKind,
        transport: QuestTransport,
        required: Vec<QuestResource>,
        make_worker: F,
    ) -> Result<AdmittedRun, AdmitError>
    where
        F: FnOnce(Vec<ResourceGuard>, watch::Receiver<bool>, Arc<AtomicU64>) -> Fut
            + Send
            + 'static,
        Fut: Future<Output = QuestOutcome> + Send + 'static,
    {
        admit_for(
            QuestAdmission {
                account_id: account_a(),
                quest_id,
                kind,
                transport,
                required,
            },
            registry,
            resources,
            make_worker,
        )
        .await
    }

    #[tokio::test]
    async fn two_rest_video_runs_admit_concurrently_and_duplicate_is_rejected() {
        let registry = Arc::new(QuestRegistry::new());
        let resources = ResourceCoordinator::new();

        let first = admit_run(
            &registry,
            &resources,
            "quest-a".to_string(),
            QuestKind::Video,
            QuestTransport::Rest,
            required(QuestKind::Video, QuestTransport::Rest),
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .expect("first video run admits");
        let second = admit_run(
            &registry,
            &resources,
            "quest-b".to_string(),
            QuestKind::Video,
            QuestTransport::Rest,
            required(QuestKind::Video, QuestTransport::Rest),
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .expect("second video run admits concurrently");
        assert_eq!(registry.snapshot().len(), 2);

        let duplicate = admit_run(
            &registry,
            &resources,
            "quest-a".to_string(),
            QuestKind::Video,
            QuestTransport::Rest,
            required(QuestKind::Video, QuestTransport::Rest),
            completing_worker(Duration::from_secs(5), None),
        )
        .await;
        assert!(matches!(duplicate, Err(AdmitError::Duplicate(_))));

        drop(first);
        drop(second);
    }

    #[tokio::test]
    async fn non_preemptive_admission_does_not_stop_an_existing_video_run() {
        // The new `start_*_run` APIs call `admit_run` directly, without the
        // legacy stop-before-start step. Admitting a second, distinct video run
        // must leave the first one live and uncancelled.
        let registry = Arc::new(QuestRegistry::new());
        let resources = ResourceCoordinator::new();

        let first = admit_run(
            &registry,
            &resources,
            "quest-a".to_string(),
            QuestKind::Video,
            QuestTransport::Rest,
            required(QuestKind::Video, QuestTransport::Rest),
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .expect("first video run admits");
        let second = admit_run(
            &registry,
            &resources,
            "quest-b".to_string(),
            QuestKind::Video,
            QuestTransport::Rest,
            required(QuestKind::Video, QuestTransport::Rest),
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .expect("second distinct video run admits without preempting");

        assert_ne!(first.control.run_id, second.control.run_id);
        assert_eq!(first.control.phase(), QuestPhase::Running);
        assert!(!*first.control.cancel.borrow());
        assert_eq!(second.control.phase(), QuestPhase::Running);
        assert_eq!(registry.snapshot().len(), 2);

        drop(first);
        drop(second);
    }

    #[tokio::test]
    async fn resource_busy_rejection_leaves_the_existing_run_untouched() {
        // A rejected admission must not disturb the run that holds the resource.
        let registry = Arc::new(QuestRegistry::new());
        let resources = ResourceCoordinator::new();

        let owner = admit_run(
            &registry,
            &resources,
            "stream".to_string(),
            QuestKind::Stream,
            QuestTransport::Rest,
            required(QuestKind::Stream, QuestTransport::Rest),
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .expect("account-activity owner admits");

        let rejected = admit_run(
            &registry,
            &resources,
            "game".to_string(),
            QuestKind::Game,
            QuestTransport::Rest,
            required(QuestKind::Game, QuestTransport::Rest),
            completing_worker(Duration::from_secs(5), None),
        )
        .await;
        assert!(matches!(
            rejected,
            Err(AdmitError::ResourceBusy(ref error)) if error.0 == QuestResource::AccountActivity
        ));

        assert_eq!(owner.control.phase(), QuestPhase::Running);
        assert!(!*owner.control.cancel.borrow());
        assert_eq!(registry.snapshot().len(), 1);

        drop(owner);
    }

    #[tokio::test]
    async fn two_account_activities_reject_the_second_with_resource_busy() {
        let registry = Arc::new(QuestRegistry::new());
        let resources = ResourceCoordinator::new();

        let video = admit_run(
            &registry,
            &resources,
            "video".to_string(),
            QuestKind::Video,
            QuestTransport::Rest,
            required(QuestKind::Video, QuestTransport::Rest),
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .expect("video run admits");

        let stream = admit_run(
            &registry,
            &resources,
            "stream".to_string(),
            QuestKind::Stream,
            QuestTransport::Rest,
            required(QuestKind::Stream, QuestTransport::Rest),
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .expect("one account activity admits alongside video");

        let game = admit_run(
            &registry,
            &resources,
            "game".to_string(),
            QuestKind::Game,
            QuestTransport::Rest,
            required(QuestKind::Game, QuestTransport::Rest),
            completing_worker(Duration::from_secs(5), None),
        )
        .await;
        match game {
            Err(AdmitError::ResourceBusy(error)) => {
                assert_eq!(error.0, QuestResource::AccountActivity);
            }
            Ok(_) => panic!("expected resource_busy"),
            Err(AdmitError::Duplicate(_)) => panic!("expected resource_busy, got duplicate"),
        }

        drop(video);
        drop(stream);
    }

    // CDP port exclusion moved to the unified `CdpPortLease`. The coordinator no
    // longer serializes CDP transports: `required_resources` is empty for a CDP
    // video run and two such runs may admit concurrently.
    #[tokio::test]
    async fn cdp_transport_no_longer_consumes_a_coordinator_resource() {
        let registry = Arc::new(QuestRegistry::new());
        let resources = ResourceCoordinator::new();
        let transport = QuestTransport::Cdp { port: 9223 };
        assert!(required(QuestKind::Video, transport).is_empty());

        let first = admit_run(
            &registry,
            &resources,
            "cdp-a".to_string(),
            QuestKind::Video,
            transport,
            required(QuestKind::Video, transport),
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .expect("first CDP run admits");

        let second = admit_run(
            &registry,
            &resources,
            "cdp-b".to_string(),
            QuestKind::Video,
            transport,
            required(QuestKind::Video, transport),
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .expect("port exclusion is the lease's responsibility, not the coordinator's");

        drop(second);
        drop(first);
    }

    #[tokio::test]
    async fn stopping_one_rest_video_run_does_not_affect_another() {
        let registry = Arc::new(QuestRegistry::new());
        let resources = ResourceCoordinator::new();
        let log = terminal_log();

        let first = admit_run(
            &registry,
            &resources,
            "quest-a".to_string(),
            QuestKind::Video,
            QuestTransport::Rest,
            required(QuestKind::Video, QuestTransport::Rest),
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .unwrap();
        let second = admit_run(
            &registry,
            &resources,
            "quest-b".to_string(),
            QuestKind::Video,
            QuestTransport::Rest,
            required(QuestKind::Video, QuestTransport::Rest),
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .unwrap();

        let monitor_a = spawn_monitor(registry.clone(), first, log.clone());
        let monitor_b = spawn_monitor(registry.clone(), second, log.clone());

        let StopSignal::Signalled(control_a) =
            registry.signal_stop(&account_a(), &"quest-a".to_string(), None)
        else {
            panic!("expected quest-a to be signalled");
        };
        assert_eq!(
            wait_for_done(&control_a, Duration::from_secs(2)).await,
            DoneWait::Finished(QuestOutcome::Stopped)
        );
        monitor_a.await.unwrap();

        let live = registry.snapshot();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].quest_id, "quest-b");
        assert_eq!(live[0].phase(), QuestPhase::Running);

        let StopSignal::Signalled(control_b) =
            registry.signal_stop(&account_a(), &"quest-b".to_string(), None)
        else {
            panic!("expected quest-b to be signalled");
        };
        assert_eq!(
            wait_for_done(&control_b, Duration::from_secs(2)).await,
            DoneWait::Finished(QuestOutcome::Stopped)
        );
        monitor_b.await.unwrap();
        assert!(registry.snapshot().is_empty());
    }

    #[tokio::test]
    async fn stop_all_signals_every_run_and_awaits() {
        let registry = Arc::new(QuestRegistry::new());
        let resources = ResourceCoordinator::new();
        let log = terminal_log();

        let first = admit_run(
            &registry,
            &resources,
            "quest-a".to_string(),
            QuestKind::Video,
            QuestTransport::Rest,
            required(QuestKind::Video, QuestTransport::Rest),
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .unwrap();
        let second = admit_run(
            &registry,
            &resources,
            "quest-b".to_string(),
            QuestKind::Video,
            QuestTransport::Rest,
            required(QuestKind::Video, QuestTransport::Rest),
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .unwrap();

        let monitor_a = spawn_monitor(registry.clone(), first, log.clone());
        let monitor_b = spawn_monitor(registry.clone(), second, log.clone());

        let results = stop_all_runs(&registry, Duration::from_secs(2)).await;
        assert_eq!(results.len(), 2);
        assert!(results
            .iter()
            .all(|(_, class)| *class == StopClass::Completed));

        monitor_a.await.unwrap();
        monitor_b.await.unwrap();
        assert!(registry.snapshot().is_empty());
        assert_eq!(
            log.lock()
                .unwrap()
                .iter()
                .filter(|(_, o)| o == &QuestOutcome::Stopped)
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn cancellation_cleanup_completes_before_the_resource_is_reusable() {
        let registry = Arc::new(QuestRegistry::new());
        let resources = Arc::new(ResourceCoordinator::new());
        let log = terminal_log();
        let cleaned = Arc::new(AtomicBool::new(false));

        let run = admit_run(
            &registry,
            &resources,
            "stream".to_string(),
            QuestKind::Stream,
            QuestTransport::Rest,
            required(QuestKind::Stream, QuestTransport::Rest),
            completing_worker(Duration::from_secs(5), Some(cleaned.clone())),
        )
        .await
        .unwrap();
        let monitor = spawn_monitor(registry.clone(), run, log.clone());

        let StopSignal::Signalled(control) =
            registry.signal_stop(&account_a(), &"stream".to_string(), None)
        else {
            panic!("expected stream run to be signalled");
        };

        // During rollback the account activity lock must still be held.
        let during_cleanup = admit_run(
            &registry,
            &resources,
            "stream-2".to_string(),
            QuestKind::Stream,
            QuestTransport::Rest,
            required(QuestKind::Stream, QuestTransport::Rest),
            completing_worker(Duration::from_secs(5), None),
        )
        .await;
        assert!(matches!(during_cleanup, Err(AdmitError::ResourceBusy(_))));

        assert_eq!(
            wait_for_done(&control, Duration::from_secs(2)).await,
            DoneWait::Finished(QuestOutcome::Stopped)
        );
        monitor.await.unwrap();
        assert!(cleaned.load(Ordering::SeqCst));

        // Once cleanup finished and the entry is gone, the resource is reusable.
        let after_cleanup = admit_run(
            &registry,
            &resources,
            "stream-3".to_string(),
            QuestKind::Stream,
            QuestTransport::Rest,
            required(QuestKind::Stream, QuestTransport::Rest),
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .expect("account activity is reusable after cleanup");
        drop(after_cleanup);
    }

    #[tokio::test]
    async fn panicking_worker_still_yields_a_failed_terminal_outcome() {
        let registry = Arc::new(QuestRegistry::new());
        let resources = ResourceCoordinator::new();
        let log = terminal_log();

        let run = admit_run(
            &registry,
            &resources,
            "boom".to_string(),
            QuestKind::Video,
            QuestTransport::Rest,
            required(QuestKind::Video, QuestTransport::Rest),
            |_guards, _cancel, _progress| {
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    panic!("simulated worker panic");
                })
            },
        )
        .await
        .unwrap();
        let monitor = spawn_monitor(registry.clone(), run, log.clone());
        monitor.await.unwrap();

        let recorded = log.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert!(matches!(recorded[0].1, QuestOutcome::Failed(_)));
        drop(recorded);
        assert!(registry.snapshot().is_empty());
    }

    #[tokio::test]
    async fn stop_timeout_retains_stopping_and_blocks_conflicting_work() {
        let registry = Arc::new(QuestRegistry::new());
        let resources = ResourceCoordinator::new();
        let log = terminal_log();
        let (release_tx, release_rx) = oneshot::channel::<()>();

        let run = admit_run(
            &registry,
            &resources,
            "slow".to_string(),
            QuestKind::Stream,
            QuestTransport::Rest,
            required(QuestKind::Stream, QuestTransport::Rest),
            move |guards, cancel, _progress| {
                Box::pin(async move {
                    // Hold the guards and the cancel receiver for the whole
                    // worker lifetime so `signal_stop` can reach this run and
                    // its resources stay reserved.
                    let _guards = guards;
                    let _cancel = cancel;
                    let _ = release_rx.await;
                    QuestOutcome::Completed
                })
            },
        )
        .await
        .unwrap();
        let monitor = spawn_monitor(registry.clone(), run, log.clone());

        let StopSignal::Signalled(control) =
            registry.signal_stop(&account_a(), &"slow".to_string(), None)
        else {
            panic!("expected slow run to be signalled");
        };
        assert_eq!(
            wait_for_done(&control, Duration::from_millis(20)).await,
            DoneWait::TimedOut
        );
        assert_eq!(control.phase(), QuestPhase::Stopping);

        // The worker still owns account activity, so conflicting work must be
        // rejected rather than silently queued.
        let conflicting = admit_run(
            &registry,
            &resources,
            "other".to_string(),
            QuestKind::Game,
            QuestTransport::Rest,
            required(QuestKind::Game, QuestTransport::Rest),
            completing_worker(Duration::from_secs(5), None),
        )
        .await;
        assert!(matches!(conflicting, Err(AdmitError::ResourceBusy(_))));

        let _ = release_tx.send(());
        monitor.await.unwrap();
        assert!(registry.snapshot().is_empty());
    }

    #[tokio::test]
    async fn stale_run_id_cannot_stop_a_newer_run() {
        let registry = Arc::new(QuestRegistry::new());
        let resources = ResourceCoordinator::new();
        let log = terminal_log();

        let first = admit_run(
            &registry,
            &resources,
            "quest".to_string(),
            QuestKind::Video,
            QuestTransport::Rest,
            required(QuestKind::Video, QuestTransport::Rest),
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .unwrap();
        let stale_run_id = first.control.run_id.to_string();
        let first_monitor = spawn_monitor(registry.clone(), first, log.clone());

        let StopSignal::Signalled(control) =
            registry.signal_stop(&account_a(), &"quest".to_string(), None)
        else {
            panic!("expected first run to be signalled");
        };
        wait_for_done(&control, Duration::from_secs(2)).await;
        first_monitor.await.unwrap();

        let second = admit_run(
            &registry,
            &resources,
            "quest".to_string(),
            QuestKind::Video,
            QuestTransport::Rest,
            required(QuestKind::Video, QuestTransport::Rest),
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .expect("newer run admits once the previous one finished");
        let second_monitor = spawn_monitor(registry.clone(), second, log.clone());

        let signal = registry.signal_stop(&account_a(), &"quest".to_string(), Some(&stale_run_id));
        assert!(matches!(signal, StopSignal::RunIdMismatch { .. }));

        let live = registry.snapshot();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].phase(), QuestPhase::Running);

        let StopSignal::Signalled(control) =
            registry.signal_stop(&account_a(), &"quest".to_string(), None)
        else {
            panic!("expected newer run to be signalled");
        };
        wait_for_done(&control, Duration::from_secs(2)).await;
        second_monitor.await.unwrap();
    }

    #[tokio::test]
    async fn rate_limit_backoff_is_account_wide() {
        use futures_util::poll;
        use std::task::Poll;

        let coordinator = RateLimitCoordinator::new();
        coordinator.note_retry_after(Duration::from_secs(300));
        let mut clearance = Box::pin(coordinator.wait_for_clearance());
        assert!(matches!(poll!(clearance.as_mut()), Poll::Pending));
    }

    #[test]
    fn quest_run_dto_uses_camel_case_contract() {
        let dto = crate::models::QuestRunDto {
            account_id: "1".to_string(),
            quest_id: "q".to_string(),
            run_id: "r".to_string(),
            kind: QuestKind::PlayActivity.as_str().to_string(),
            transport: QuestTransport::Cdp { port: 9223 }.as_str(),
            phase: QuestPhase::Stopping.as_str().to_string(),
            progress: 12.5,
        };
        let value = serde_json::to_value(dto).unwrap();
        assert_eq!(value["accountId"], "1");
        assert_eq!(value["questId"], "q");
        assert_eq!(value["runId"], "r");
        assert_eq!(value["kind"], "playActivity");
        assert_eq!(value["transport"], "cdp:9223");
        assert_eq!(value["phase"], "stopping");
        assert_eq!(value["progress"], 12.5);
        assert!(value.get("account_id").is_none());
    }

    // B1: the same quest id admits concurrently for two accounts; a duplicate
    // within one account is rejected.
    #[tokio::test]
    async fn same_quest_id_admits_per_account_and_duplicate_is_rejected() {
        let registry = Arc::new(QuestRegistry::new());
        let resources = ResourceCoordinator::new();

        let a = admit_for(
            admission(
                account_a(),
                "shared",
                QuestKind::Video,
                QuestTransport::Rest,
            ),
            &registry,
            &resources,
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .expect("account A admits the shared quest");
        let b = admit_for(
            admission(
                account_b(),
                "shared",
                QuestKind::Video,
                QuestTransport::Rest,
            ),
            &registry,
            &resources,
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .expect("account B admits the same quest id concurrently");

        assert_eq!(a.control.account_id, account_a());
        assert_eq!(b.control.account_id, account_b());
        assert_eq!(registry.snapshot().len(), 2);

        let duplicate = admit_for(
            admission(
                account_a(),
                "shared",
                QuestKind::Video,
                QuestTransport::Rest,
            ),
            &registry,
            &resources,
            completing_worker(Duration::from_secs(5), None),
        )
        .await;
        assert!(matches!(duplicate, Err(AdmitError::Duplicate(_))));

        drop(a);
        drop(b);
    }

    // B2: account activity is exclusive per account but does not serialize across
    // accounts.
    #[tokio::test]
    async fn account_activity_is_exclusive_per_account_but_not_across() {
        let registry = Arc::new(QuestRegistry::new());
        let resources = ResourceCoordinator::new();

        let a = admit_for(
            admission(
                account_a(),
                "a-stream",
                QuestKind::Stream,
                QuestTransport::Rest,
            ),
            &registry,
            &resources,
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .expect("account A holds its activity");

        let a2 = admit_for(
            admission(account_a(), "a-game", QuestKind::Game, QuestTransport::Rest),
            &registry,
            &resources,
            completing_worker(Duration::from_secs(5), None),
        )
        .await;
        assert!(matches!(
            a2,
            Err(AdmitError::ResourceBusy(ref error))
                if error.0 == QuestResource::AccountActivity
        ));

        let b = admit_for(
            admission(account_b(), "b-game", QuestKind::Game, QuestTransport::Rest),
            &registry,
            &resources,
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .expect("account B is not serialized by account A's activity");
        assert_eq!(registry.snapshot().len(), 2);

        drop(a);
        drop(b);
    }

    // B2: request capacity and 429 backoff are account-local and shared across
    // repeated handles for one account. Fully deterministic: driven with
    // `futures_util::poll!`, never a real wait.
    #[tokio::test]
    async fn request_gate_capacity_and_backoff_are_per_account() {
        use futures_util::poll;
        use std::task::Poll;

        let resources = ResourceCoordinator::new();
        let a = account_a();
        let b = account_b();
        let gate_a = resources.request_gate(&a);
        let gate_a_again = resources.request_gate(&a);
        let gate_b = resources.request_gate(&b);

        // Repeated calls for one account share state; another account does not.
        assert!(Arc::ptr_eq(&gate_a.rate_limits, &gate_a_again.rate_limits));
        assert!(!Arc::ptr_eq(&gate_a.rate_limits, &gate_b.rate_limits));

        // Two A permits are immediately available; a third A acquire is Pending.
        let mut a1 = Box::pin(gate_a.acquire());
        let permit_a1 = match poll!(a1.as_mut()) {
            Poll::Ready(permit) => permit,
            Poll::Pending => panic!("first A permit should be immediately available"),
        };
        let mut a2 = Box::pin(gate_a.acquire());
        let permit_a2 = match poll!(a2.as_mut()) {
            Poll::Ready(permit) => permit,
            Poll::Pending => panic!("second A permit should be immediately available"),
        };
        let mut a3 = Box::pin(gate_a.acquire());
        assert!(
            matches!(poll!(a3.as_mut()), Poll::Pending),
            "third A acquire must wait for capacity"
        );

        // B's independent capacity completes while A is saturated.
        let mut b1 = Box::pin(gate_b.acquire());
        assert!(
            matches!(poll!(b1.as_mut()), Poll::Ready(_)),
            "B request must not be blocked by A saturation"
        );

        drop((permit_a1, permit_a2));
    }

    // A backoff reported on A blocks only A; B still acquires immediately.
    #[tokio::test]
    async fn request_gate_backoff_blocks_only_its_own_account() {
        use futures_util::poll;
        use std::task::Poll;

        let resources = ResourceCoordinator::new();
        let gate_a = resources.request_gate(&account_a());
        let gate_b = resources.request_gate(&account_b());

        gate_a.note_retry_after(Duration::from_secs(300));
        let mut a = Box::pin(gate_a.acquire());
        assert!(matches!(poll!(a.as_mut()), Poll::Pending));

        let mut b = Box::pin(gate_b.acquire());
        assert!(matches!(poll!(b.as_mut()), Poll::Ready(_)));
    }

    // A shorter subsequent 429 must never shorten the retained deadline.
    #[tokio::test]
    async fn request_gate_keeps_the_longer_backoff() {
        use futures_util::poll;
        use std::task::Poll;

        let resources = ResourceCoordinator::new();
        let gate = resources.request_gate(&account_a());

        gate.note_retry_after(Duration::from_secs(300));
        gate.note_retry_after(Duration::from_millis(1));

        let deadline = *gate
            .rate_limits
            .retry_after_until
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            deadline
                .expect("backoff deadline recorded")
                .saturating_duration_since(Instant::now())
                >= Duration::from_secs(200),
            "a shorter subsequent Retry-After must not shorten the deadline"
        );

        let mut acquire = Box::pin(gate.acquire());
        assert!(
            matches!(poll!(acquire.as_mut()), Poll::Pending),
            "the retained longer backoff still holds the account"
        );
    }

    // B2/B4: process simulation and Discord RPC stay globally exclusive across
    // accounts. CDP port exclusion lives in the unified lease, not here.
    #[tokio::test]
    async fn global_resources_are_exclusive_across_accounts() {
        let registry = Arc::new(QuestRegistry::new());
        let resources = ResourceCoordinator::new();

        let sim_a = admit_for(
            QuestAdmission {
                required: vec![QuestResource::ProcessSimulation],
                ..admission(account_a(), "sim-a", QuestKind::Video, QuestTransport::Rest)
            },
            &registry,
            &resources,
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .expect("account A holds process simulation");
        let sim_b = admit_for(
            QuestAdmission {
                required: vec![QuestResource::ProcessSimulation],
                ..admission(account_b(), "sim-b", QuestKind::Video, QuestTransport::Rest)
            },
            &registry,
            &resources,
            completing_worker(Duration::from_secs(5), None),
        )
        .await;
        assert!(matches!(
            sim_b,
            Err(AdmitError::ResourceBusy(ref error))
                if error.0 == QuestResource::ProcessSimulation
        ));
        drop(sim_a);

        let rpc_a = admit_for(
            QuestAdmission {
                required: vec![QuestResource::DiscordRpc],
                ..admission(account_a(), "rpc-a", QuestKind::Video, QuestTransport::Rest)
            },
            &registry,
            &resources,
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .expect("account A holds Discord RPC");
        let rpc_b = admit_for(
            QuestAdmission {
                required: vec![QuestResource::DiscordRpc],
                ..admission(account_b(), "rpc-b", QuestKind::Video, QuestTransport::Rest)
            },
            &registry,
            &resources,
            completing_worker(Duration::from_secs(5), None),
        )
        .await;
        assert!(matches!(
            rpc_b,
            Err(AdmitError::ResourceBusy(ref error))
                if error.0 == QuestResource::DiscordRpc
        ));
        drop(rpc_a);
    }

    // B3/B6: stopping account A's run cannot remove, stop, or mutate account B's
    // run, even with an identical quest id.
    #[tokio::test]
    async fn stopping_account_a_cannot_affect_account_b_same_quest_id() {
        let registry = Arc::new(QuestRegistry::new());
        let resources = ResourceCoordinator::new();
        let log = terminal_log();

        let a = admit_for(
            admission(
                account_a(),
                "shared",
                QuestKind::Video,
                QuestTransport::Rest,
            ),
            &registry,
            &resources,
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .unwrap();
        let b = admit_for(
            admission(
                account_b(),
                "shared",
                QuestKind::Video,
                QuestTransport::Rest,
            ),
            &registry,
            &resources,
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .unwrap();
        let a_run_id = a.control.run_id;
        let a_monitor = spawn_monitor(registry.clone(), a, log.clone());
        let b_monitor = spawn_monitor(registry.clone(), b, log.clone());

        let StopSignal::Signalled(control) =
            registry.signal_stop(&account_a(), &"shared".to_string(), None)
        else {
            panic!("expected account A run to be signalled");
        };
        assert_eq!(control.run_id, a_run_id);
        wait_for_done(&control, Duration::from_secs(2)).await;
        a_monitor.await.unwrap();

        let live = registry.snapshot();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].account_id, account_b());
        assert_eq!(live[0].quest_id, "shared");
        assert_eq!(live[0].phase(), QuestPhase::Running);
        assert!(!*live[0].cancel.borrow());

        // An account-A stop is now a no-op and never touches account B.
        let results = stop_account_runs(&registry, &account_a(), Duration::from_millis(50)).await;
        assert!(results.is_empty());
        assert_eq!(registry.snapshot().len(), 1);

        let StopSignal::Signalled(control) =
            registry.signal_stop(&account_b(), &"shared".to_string(), None)
        else {
            panic!("expected account B run to be signalled");
        };
        wait_for_done(&control, Duration::from_secs(2)).await;
        b_monitor.await.unwrap();
        assert!(registry.snapshot().is_empty());
    }

    // B3: stop_account_runs only stops the addressed account's runs.
    #[tokio::test]
    async fn stop_account_runs_is_scoped_to_one_account() {
        let registry = Arc::new(QuestRegistry::new());
        let resources = ResourceCoordinator::new();
        let log = terminal_log();

        let a = admit_for(
            admission(account_a(), "a-q", QuestKind::Video, QuestTransport::Rest),
            &registry,
            &resources,
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .unwrap();
        let b = admit_for(
            admission(account_b(), "b-q", QuestKind::Video, QuestTransport::Rest),
            &registry,
            &resources,
            completing_worker(Duration::from_secs(5), None),
        )
        .await
        .unwrap();
        let a_monitor = spawn_monitor(registry.clone(), a, log.clone());
        let b_monitor = spawn_monitor(registry.clone(), b, log.clone());

        let results = stop_account_runs(&registry, &account_a(), Duration::from_secs(2)).await;
        assert_eq!(results.len(), 1);
        a_monitor.await.unwrap();

        let live = registry.snapshot();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].account_id, account_b());
        assert_eq!(live[0].phase(), QuestPhase::Running);

        stop_account_runs(&registry, &account_b(), Duration::from_secs(2)).await;
        b_monitor.await.unwrap();
        assert!(registry.snapshot().is_empty());
    }
}
