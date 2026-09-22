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
//! - CDP-mutating runs hold [`QuestResource::CdpPort`]: at most one per port.
//! - [`QuestResource::ProcessSimulation`] and [`QuestResource::DiscordRpc`] are
//!   globally exclusive and reserved for the process-simulation and RPC paths.

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
    pub fn required_resources(self, transport: QuestTransport) -> Vec<QuestResource> {
        let mut resources = Vec::new();
        if let QuestTransport::Cdp { port } = transport {
            resources.push(QuestResource::CdpPort(port));
        }
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
    CdpPort(u16),
    #[allow(dead_code)]
    ProcessSimulation,
    #[allow(dead_code)]
    DiscordRpc,
}

impl QuestResource {
    fn rank(self) -> u8 {
        match self {
            QuestResource::AccountActivity => 0,
            QuestResource::CdpPort(_) => 1,
            QuestResource::ProcessSimulation => 2,
            QuestResource::DiscordRpc => 3,
        }
    }
}

impl fmt::Display for QuestResource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QuestResource::AccountActivity => write!(formatter, "account_activity"),
            QuestResource::CdpPort(port) => write!(formatter, "cdp_port_{port}"),
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
    CdpPort(OwnedMutexGuard<()>),
    ProcessSimulation(OwnedMutexGuard<()>),
    DiscordRpc(OwnedMutexGuard<()>),
}

/// Account-wide rate-limit coordination: a small in-flight concurrency limit
/// plus an account-wide 429 `Retry-After` backoff. Deliberately does not retry
/// on its own; callers decide whether to wait and retry.
pub struct RateLimitCoordinator {
    in_flight: Arc<Semaphore>,
    retry_after_until: Mutex<Option<Instant>>,
}

impl RateLimitCoordinator {
    pub fn new() -> Self {
        Self {
            in_flight: Arc::new(Semaphore::new(2)),
            retry_after_until: Mutex::new(None),
        }
    }

    /// Record a Discord 429 `Retry-After` that applies to the whole account.
    pub fn note_retry_after(&self, retry_after: Duration) {
        let until = Instant::now() + retry_after;
        let mut guard = self
            .retry_after_until
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(until);
    }

    /// Wait for any account-wide backoff to elapse before issuing a request.
    pub async fn wait_for_clearance(&self) {
        let wait = {
            let guard = self
                .retry_after_until
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard
                .map(|until| until.saturating_duration_since(Instant::now()))
                .unwrap_or_default()
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    /// Acquire an in-flight slot, honoring any account-wide backoff first.
    #[allow(dead_code)]
    pub async fn acquire(&self) -> OwnedSemaphorePermit {
        self.wait_for_clearance().await;
        self.in_flight
            .clone()
            .acquire_owned()
            .await
            .expect("rate-limit semaphore is never closed")
    }
}

impl Default for RateLimitCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

/// Owns every exclusive resource a quest can require.
pub struct ResourceCoordinator {
    account_activity: Arc<AsyncMutex<()>>,
    cdp_ports: Mutex<HashMap<u16, Arc<AsyncMutex<()>>>>,
    process_simulation: Arc<AsyncMutex<()>>,
    discord_rpc: Arc<AsyncMutex<()>>,
    api_requests: Arc<Semaphore>,
    rate_limits: Arc<RateLimitCoordinator>,
}

impl ResourceCoordinator {
    pub fn new() -> Self {
        Self {
            account_activity: Arc::new(AsyncMutex::new(())),
            cdp_ports: Mutex::new(HashMap::new()),
            process_simulation: Arc::new(AsyncMutex::new(())),
            discord_rpc: Arc::new(AsyncMutex::new(())),
            api_requests: Arc::new(Semaphore::new(2)),
            rate_limits: Arc::new(RateLimitCoordinator::new()),
        }
    }

    #[allow(dead_code)]
    pub fn rate_limits(&self) -> &Arc<RateLimitCoordinator> {
        &self.rate_limits
    }

    /// Reserve an in-flight API request slot. Exposed so the authenticated API
    /// request path can throttle account-wide traffic.
    #[allow(dead_code)]
    pub async fn acquire_api_permit(&self) -> OwnedSemaphorePermit {
        self.api_requests
            .clone()
            .acquire_owned()
            .await
            .expect("api request semaphore is never closed")
    }

    /// Try to take all `required` resources without waiting. On failure any
    /// already-acquired guards are dropped automatically, releasing them.
    pub fn try_acquire_all(
        &self,
        required: &[QuestResource],
    ) -> Result<Vec<ResourceGuard>, ResourceBusyError> {
        let mut ordered = required.to_vec();
        ordered.sort_by_key(|resource| resource.rank());
        ordered.dedup();

        let mut guards = Vec::with_capacity(ordered.len());
        for resource in ordered {
            guards.push(self.try_acquire(resource)?);
        }
        Ok(guards)
    }

    fn try_acquire(&self, resource: QuestResource) -> Result<ResourceGuard, ResourceBusyError> {
        match resource {
            QuestResource::AccountActivity => self
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
            QuestResource::CdpPort(port) => {
                // Never hold the port map while awaiting: clone the per-port
                // mutex Arc out, then try it without blocking.
                let port_mutex = {
                    let mut ports = self
                        .cdp_ports
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    ports
                        .entry(port)
                        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
                        .clone()
                };
                port_mutex
                    .try_lock_owned()
                    .map(ResourceGuard::CdpPort)
                    .map_err(|_| ResourceBusyError(resource))
            }
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

/// Registry of live runs, keyed by quest id.
pub struct QuestRegistry {
    entries: Mutex<HashMap<QuestId, Arc<QuestControl>>>,
    admission: AsyncMutex<()>,
}

impl QuestRegistry {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            admission: AsyncMutex::new(()),
        }
    }

    fn lock_entries(&self) -> std::sync::MutexGuard<'_, HashMap<QuestId, Arc<QuestControl>>> {
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

    pub fn has_live_runs(&self) -> bool {
        self.lock_entries()
            .values()
            .any(|control| !is_finished(control))
    }

    /// Remove a run only when the `run_id` still matches. A monitor that lost a
    /// race with a newer run of the same quest must not evict it.
    pub fn finish(&self, run_id: &RunId) {
        self.lock_entries()
            .retain(|_, control| control.run_id != *run_id);
    }

    /// Request cancellation. A stale `run_id` is rejected; an unknown or already
    /// finished quest id is reported as not found so the caller can answer
    /// idempotently.
    pub fn signal_stop(&self, quest_id: &QuestId, run_id: Option<&str>) -> StopSignal {
        let control = self.lock_entries().get(quest_id).cloned();
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

/// Admit a run under the short admission mutex: reap finished entries, reject a
/// duplicate live quest id, try-acquire resources without waiting, then spawn
/// the worker that owns those guards for its whole lifetime.
///
/// The admission mutex is dropped before the worker future is polled, so it is
/// never held for the life of a quest.
pub async fn admit_run<F, Fut>(
    registry: &QuestRegistry,
    resources: &ResourceCoordinator,
    quest_id: QuestId,
    kind: QuestKind,
    transport: QuestTransport,
    required: Vec<QuestResource>,
    make_worker: F,
) -> Result<AdmittedRun, AdmitError>
where
    F: FnOnce(Vec<ResourceGuard>, watch::Receiver<bool>, Arc<AtomicU64>) -> Fut + Send + 'static,
    Fut: Future<Output = QuestOutcome> + Send + 'static,
{
    let admission = registry.admission.lock().await;
    registry.reap_finished_locked();

    if let Some(existing) = registry.lock_entries().get(&quest_id).cloned() {
        if !is_finished(&existing) {
            return Err(AdmitError::Duplicate(quest_id));
        }
    }

    let guards = resources
        .try_acquire_all(&required)
        .map_err(AdmitError::ResourceBusy)?;

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let (done_tx, done_rx) = watch::channel(None);
    let progress = Arc::new(AtomicU64::new(0));
    let run_id = RunId::new_v4();

    let worker = tokio::spawn(make_worker(guards, cancel_rx, progress.clone()));

    let control = Arc::new(QuestControl {
        run_id,
        quest_id: quest_id.clone(),
        kind,
        transport,
        resources: required,
        started_at: Instant::now(),
        cancel: cancel_tx,
        done: done_rx,
        abort: worker.abort_handle(),
        progress,
    });

    registry.lock_entries().insert(quest_id, control.clone());
    drop(admission);

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
    quest_id: QuestId,
}

impl QuestEventSink {
    pub fn new(app: tauri::AppHandle, progress: Arc<AtomicU64>, quest_id: QuestId) -> Self {
        Self {
            app,
            progress,
            quest_id,
        }
    }

    pub fn quest_id(&self) -> &str {
        &self.quest_id
    }

    pub fn progress(&self, value: f64) {
        self.progress.store(value.to_bits(), Ordering::Relaxed);
        let _ = self.app.emit("quest-progress", value);
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

    #[tokio::test]
    async fn two_cdp_runs_on_the_same_port_reject_the_second() {
        let registry = Arc::new(QuestRegistry::new());
        let resources = ResourceCoordinator::new();
        // A CDP video transport requires only the port, isolating the port
        // conflict from the account-activity lock.
        let transport = QuestTransport::Cdp { port: 9223 };

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
        .await;
        match second {
            Err(AdmitError::ResourceBusy(error)) => {
                assert_eq!(error.0, QuestResource::CdpPort(9223));
            }
            Ok(_) => panic!("expected cdp port resource_busy"),
            Err(AdmitError::Duplicate(_)) => {
                panic!("expected cdp port resource_busy, got duplicate")
            }
        }

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

        let StopSignal::Signalled(control_a) = registry.signal_stop(&"quest-a".to_string(), None)
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

        let StopSignal::Signalled(control_b) = registry.signal_stop(&"quest-b".to_string(), None)
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

        let StopSignal::Signalled(control) = registry.signal_stop(&"stream".to_string(), None)
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

        let StopSignal::Signalled(control) = registry.signal_stop(&"slow".to_string(), None) else {
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

        let StopSignal::Signalled(control) = registry.signal_stop(&"quest".to_string(), None)
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

        let signal = registry.signal_stop(&"quest".to_string(), Some(&stale_run_id));
        assert!(matches!(signal, StopSignal::RunIdMismatch { .. }));

        let live = registry.snapshot();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].phase(), QuestPhase::Running);

        let StopSignal::Signalled(control) = registry.signal_stop(&"quest".to_string(), None)
        else {
            panic!("expected newer run to be signalled");
        };
        wait_for_done(&control, Duration::from_secs(2)).await;
        second_monitor.await.unwrap();
    }

    #[tokio::test]
    async fn rate_limit_backoff_is_account_wide() {
        let coordinator = RateLimitCoordinator::new();
        coordinator.note_retry_after(Duration::from_millis(30));
        let started = Instant::now();
        coordinator.wait_for_clearance().await;
        assert!(started.elapsed() >= Duration::from_millis(20));
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
}
