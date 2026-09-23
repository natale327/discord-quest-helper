import { defineStore } from 'pinia'
import { computed, ref, watch } from 'vue'
import type {
  Quest,
  DetectableGame,
  DesktopClientArg,
  ExcludedQuest,
  GameQuestMode,
  PlatformCapabilities,
  QuestEventEnvelope,
  QuestRunDto,
  QuestRunKind,
  QuestRunPhase,
  StopQuestResult,
} from '@/api/tauri'
import { getQuestKind, playActivityProgressPercentage } from '@/utils/questTasks'
import { resolveSimulationExecutable } from '@/utils/executables'

/** Quest with optional pre-selected executable name for batch game quest processing */
interface QueueItem extends Quest {
  selectedExeName?: string
}

/**
 * A recoverable, non-fatal quest condition surfaced to the UI.
 *
 * Unlike a thrown Error, a soft error does not abort the queue or read as a
 * system failure — it drives a dialog that can steer the user to a working mode
 * (e.g. CDP) without losing quest context.
 */
export interface QuestSoftError {
  code: 'SIMULATION_EXECUTABLE_OS_UNSUPPORTED' | 'SIMULATION_EXECUTABLE_NOT_FOUND'
  /** English fallback message; the UI localizes by `code` using `gameName`. */
  message: string
  /** Detectable-game name, so the dialog can render a localized message. */
  gameName: string
  questId: string
  recommendedMode?: 'cdp'
  recoverable: true
}

/** Why the quest queue is currently paused. */
export type QueuePauseReason =
  | 'user'
  | 'simulation_incompatible'
  | 'cdp_restart_required'
  | 'authentication_required'

/**
 * A live run plus the local quest metadata the temporary scalar projection
 * needs. `runsByQuestId`/`activeRuns` are the source of truth for parallel runs;
 * the scalars below are only a compatibility projection.
 */
export interface QuestRunView {
  questId: string
  runId: string
  accountId: string
  kind: QuestRunKind
  transport: string
  phase: QuestRunPhase
  /** Latest backend-reported progress percentage (0-100). */
  progress: number
  /** Legacy UI classification for the compatibility projection. */
  questType: 'video' | 'stream' | 'game' | 'activity'
  /** Total seconds needed, for duration display. */
  targetDuration: number
  /** Seconds completed when the run was admitted. */
  initialProgressSeconds: number
  /** Local admission/observation time; used only for display ordering. */
  startedAt: number
}
import {
  getQuestsFull,
  startVideoQuestRun,
  startStreamQuestRun,
  startGameHeartbeatQuestRun,
  startPlayActivityQuestRun,
  startCdpQuestRun,
  listAllQuestRuns,
  stopAccountQuestRun,
  stopQuestRun,
  stopAllQuests,
  onQuestProgress,
  onQuestComplete,
  onQuestError,
  onQuestStopped,
  createSimulatedGame,
  runSimulatedGame,
  stopSimulatedGame,
  fetchDetectableGames,
  connectToDiscordRpc,
  acceptQuest,
  forceVideoProgress,
  checkCdpStatus,
  getVirtualCurrencyBalance,
  getPlatformCapabilities
} from '@/api/tauri'
import { documentDir, join } from '@tauri-apps/api/path'
import { emit } from '@tauri-apps/api/event'


// localStorage keys
const STORAGE_SPEED_KEY = 'questHelper_speedMultiplier'

export const useQuestsStore = defineStore('quests', () => {
  const quests = ref<Quest[]>([])
  const excludedQuests = ref<ExcludedQuest[]>([])
  const questEnrollmentBlockedUntil = ref<string | null>(null)
  const lastQuestsFetchTime = ref(0)
  const loading = ref(false)
  const stopping = ref(false)
  const error = ref<string | null>(null)
  const orbsBalance = ref<number | null>(null)
  const orbsBalanceFetchedAt = ref<string | null>(null)
  const orbsBalanceLoading = ref(false)
  const orbsBalanceError = ref<string | null>(null)

  const activeQuestId = ref<string | null>(null)
  const activeQuestType = ref<'video' | 'stream' | 'game' | 'activity' | null>(null)
  const activeQuestProgress = ref(0)
  const activeQuestTargetDuration = ref(0)

  // Local Progress Simulation State
  const localProgress = ref(0)
  const activeGameExe = ref<string | null>(null)

  // ---------------------------------------------------------------------------
  // Registry-backed run collection (Phase 4A)
  //
  // The backend registry is the source of truth for live runs, keyed by questId
  // (the registry permits only one live run per quest id). Local quest metadata
  // is retained so untouched components can still render duration/progress while
  // the designer lane builds the per-run UI.
  // ---------------------------------------------------------------------------

  /** The account whose runs the legacy `runsByQuestId` projection exposes. */
  const activeAccountId = ref<string | null>(null)
  // Monotonic epoch bumped on every account switch. In-flight account-local
  // fetches/queue work capture it and abort if it changed, so work begun for A
  // can never apply/start against newly active B.
  let accountEpoch = 0
  let queueEpoch = 0

  function setActiveAccount(accountId: string | null) {
    if (activeAccountId.value === accountId) return
    activeAccountId.value = accountId
    accountEpoch += 1
    queueEpoch += 1
    // Cancel any pending coalesced refresh begun for the previous account and
    // stop a queue run that belonged to it.
    if (refreshTimer !== null) {
      clearTimeout(refreshTimer)
      refreshTimer = null
    }
    isQueueRunning.value = false
    // Pending local queue items belong to the previous account. Clear them so
    // restarting/processing under the new account can never execute them.
    // Already-admitted active work may continue.
    clearPendingQueue()
    // Drop the previous account's cached quest list so the new account refetches
    // its own data rather than reusing the previous account's cache.
    quests.value = []
    excludedQuests.value = []
    questEnrollmentBlockedUntil.value = null
    lastQuestsFetchTime.value = 0
    syncLegacyProjection()
    // Refetch/reproject everything for the newly active account. This is the
    // explicit path the newly selected account uses; nothing is re-read
    // implicitly after an await.
    void refreshRuns()
    void fetchQuests(true, true)
  }

  /** Account-safe run key: one live run per (account, quest id). */
  function runKey(accountId: string, questId: string): string {
    return `${accountId}\u0000${questId}`
  }

  /** Source of truth for all live runs across every account. */
  const runsByKey = ref<Record<string, QuestRunView>>({})

  /**
   * Back-compat projection keyed by questId for the ACTIVE account only, so
   * unmodified components keep working without same-quest-id cross-account
   * collisions. Background accounts' runs live only in `runsByKey`.
   */
  const runsByQuestId = computed<Record<string, QuestRunView>>(() => {
    const projection: Record<string, QuestRunView> = {}
    for (const run of Object.values(runsByKey.value)) {
      if (activeAccountId.value === null || run.accountId === activeAccountId.value) {
        projection[run.questId] = run
      }
    }
    return projection
  })

  /** Newest-first list view across every account; stable read API. */
  const activeRuns = computed<QuestRunView[]>(() =>
    Object.values(runsByKey.value).sort((a, b) => b.startedAt - a.startedAt)
  )

  /** Runs belonging to one account (newest first). */
  function accountRuns(accountId: string): QuestRunView[] {
    return Object.values(runsByKey.value)
      .filter(run => run.accountId === accountId)
      .sort((a, b) => b.startedAt - a.startedAt)
  }

  // Simulate-mode game quests are not registry runs (they never call a
  // `start_*_quest_run` command), so they are kept in this compatibility slot so
  // the scalar projection still has a primary run.
  const manualSimulation = ref<{ questId: string; targetDuration: number; progressPct: number } | null>(null)

  // Bumped on every locally-admitted run. A snapshot fetch that started before a
  // local admission is discarded so it cannot delete the fresh run.
  let admissionGeneration = 0

  function getRun(questId: string, accountId?: string): QuestRunView | undefined {
    if (accountId !== undefined) {
      return runsByKey.value[runKey(accountId, questId)]
    }
    return runsByQuestId.value[questId]
  }

  function kindToQuestType(kind: QuestRunKind): QuestRunView['questType'] {
    switch (kind) {
      case 'video': return 'video'
      case 'stream': return 'stream'
      case 'game': return 'game'
      case 'playActivity':
      case 'embeddedActivity': return 'activity'
    }
  }

  function progressSecondsForQuest(quest: Quest): number {
    const progress = quest.user_status?.progress
    if (!progress || typeof progress !== 'object') return 0
    const first = Object.values(progress)[0]
    return first?.value ?? 0
  }

  function deriveRunMeta(dto: QuestRunDto): Pick<QuestRunView, 'questType' | 'targetDuration' | 'initialProgressSeconds' | 'startedAt'> {
    const quest = quests.value.find(q => q.id === dto.questId)
    let targetDuration = 0
    let initialProgressSeconds = 0
    if (quest) {
      const tasks = quest.config.task_config_v2?.tasks ?? quest.config.task_config?.tasks
      if (tasks) {
        const values = Object.values(tasks)
        if (values.length > 0) targetDuration = values[0]?.target ?? 0
      }
      initialProgressSeconds = progressSecondsForQuest(quest)
    }
    return {
      questType: kindToQuestType(dto.kind),
      targetDuration,
      initialProgressSeconds,
      startedAt: Date.now(),
    }
  }

  /**
   * Insert or update a run returned by a start command. `progressOverride`
   * preserves the legacy scalar progress (e.g. the initial percentage) until the
   * first authoritative snapshot arrives.
   */
  function registerRun(
    dto: QuestRunDto,
    meta: { questType: QuestRunView['questType']; targetDuration: number; progressOverride?: number },
  ) {
    const key = runKey(dto.accountId, dto.questId)
    const existing = runsByKey.value[key]
    const prior = existing && existing.runId === dto.runId ? existing : undefined
    runsByKey.value = {
      ...runsByKey.value,
      [key]: {
        questId: dto.questId,
        runId: dto.runId,
        accountId: dto.accountId,
        kind: dto.kind,
        transport: dto.transport,
        phase: dto.phase,
        progress: meta.progressOverride ?? dto.progress,
        questType: meta.questType,
        targetDuration: meta.targetDuration,
        initialProgressSeconds: prior ? prior.initialProgressSeconds : 0,
        startedAt: prior ? prior.startedAt : Date.now(),
      },
    }
    admissionGeneration++
    syncLegacyProjection()
  }

  /**
   * Apply an authoritative registry snapshot. The backend keeps stopping runs in
   * its registry, so a run absent from a fresh snapshot is genuinely finished;
   * the generation guard in `refreshRuns` prevents a snapshot fetched before a
   * newer local admission from deleting that new run.
   */
  function reconcileRuns(dtos: QuestRunDto[]) {
    const next: Record<string, QuestRunView> = {}
    for (const dto of dtos) {
      const key = runKey(dto.accountId, dto.questId)
      const existing = runsByKey.value[key]
      const meta = existing && existing.runId === dto.runId
        ? {
            questType: existing.questType,
            targetDuration: existing.targetDuration,
            initialProgressSeconds: existing.initialProgressSeconds,
            startedAt: existing.startedAt,
          }
        : deriveRunMeta(dto)
      next[key] = {
        questId: dto.questId,
        runId: dto.runId,
        accountId: dto.accountId,
        kind: dto.kind,
        transport: dto.transport,
        phase: dto.phase,
        progress: dto.progress,
        ...meta,
      }
    }
    runsByKey.value = next
    syncLegacyProjection()
  }

  async function refreshRuns(): Promise<void> {
    const generationAtStart = admissionGeneration
    const epochAtStart = accountEpoch
    try {
      const dtos = await listAllQuestRuns()
      // Discard a snapshot begun for a previous account (or superseded by a
      // newer local admission) so it cannot clobber the new account's state.
      if (generationAtStart !== admissionGeneration) return
      if (epochAtStart !== accountEpoch) return
      reconcileRuns(dtos)
    } catch (e) {
      console.warn('Failed to refresh quest runs:', e)
    }
  }

  // Coalesced refresh: a burst of ID-less progress/terminal events triggers a
  // single snapshot reconciliation rather than one fetch per event.
  let refreshTimer: ReturnType<typeof setTimeout> | null = null
  function scheduleRefresh(): void {
    if (refreshTimer !== null) return
    refreshTimer = setTimeout(() => {
      refreshTimer = null
      void refreshRuns()
    }, 150)
  }

  function projectPrimaryRun(): QuestRunView | null {
    const candidates = Object.values(runsByKey.value)
      .filter(run => run.phase !== 'finished')
      .filter(run => activeAccountId.value === null || run.accountId === activeAccountId.value)
    if (candidates.length === 0) return null
    candidates.sort((a, b) => {
      const rank = (phase: QuestRunPhase) => (phase === 'running' ? 0 : 1)
      if (rank(a.phase) !== rank(b.phase)) return rank(a.phase) - rank(b.phase)
      return b.startedAt - a.startedAt
    })
    return candidates[0]
  }

  /**
   * Temporary scalar compatibility projection (Phase 4A only). It represents at
   * most ONE primary run and must not be treated as a complete view of all live
   * runs — use `activeRuns`/`runsByQuestId` for that.
   */
  function syncLegacyProjection() {
    const primary = projectPrimaryRun()
    if (primary) {
      const identityChanged = activeQuestId.value !== primary.questId
      activeQuestId.value = primary.questId
      activeQuestType.value = primary.questType
      activeQuestTargetDuration.value = primary.targetDuration
      activeQuestProgress.value = primary.progress
      if (identityChanged) localProgress.value = primary.progress
      return
    }
    if (manualSimulation.value) {
      const sim = manualSimulation.value
      const identityChanged = activeQuestId.value !== sim.questId
      activeQuestId.value = sim.questId
      activeQuestType.value = 'game'
      activeQuestTargetDuration.value = sim.targetDuration
      activeQuestProgress.value = sim.progressPct
      if (identityChanged) localProgress.value = sim.progressPct
      return
    }
    activeQuestId.value = null
    activeQuestType.value = null
    activeQuestProgress.value = 0
    activeQuestTargetDuration.value = 0
    localProgress.value = 0
  }

  /**
   * Stop one run. On `stopTimeout`/`runIdMismatch` the run is deliberately kept
   * (never falsely removed); only `stopped`/`alreadyFinished` clear it, followed
   * by an authoritative refresh.
   */
  async function stopRun(
    questId: string,
    runId?: string,
    accountId?: string,
  ): Promise<StopQuestResult> {
    // Account-scoped stop when the caller knows the owning account; otherwise
    // fall back to the legacy active-account command.
    const resolvedAccountId = accountId
      ?? Object.values(runsByKey.value).find(run => run.questId === questId)?.accountId
    const key = resolvedAccountId !== undefined ? runKey(resolvedAccountId, questId) : null
    const result = resolvedAccountId !== undefined
      ? await stopAccountQuestRun(resolvedAccountId, questId, runId)
      : await stopQuestRun(questId, runId)
    if (result.status === 'stopped' || result.status === 'alreadyFinished') {
      if (key !== null) {
        const next = { ...runsByKey.value }
        delete next[key]
        runsByKey.value = next
      }
      syncLegacyProjection()
      await refreshRuns()
    } else if (result.status === 'stopTimeout') {
      const run = key !== null ? runsByKey.value[key] : undefined
      if (run && key !== null) {
        runsByKey.value = { ...runsByKey.value, [key]: { ...run, phase: 'stopping' } }
      }
      error.value = 'Stopping this quest is taking longer than expected. It is still running; try again shortly.'
      syncLegacyProjection()
      scheduleRefresh()
    } else {
      error.value = 'This quest run changed since it was listed. Refresh and try again.'
      scheduleRefresh()
    }
    return result
  }

  // Speed multiplier - read from localStorage, default 1, range 0.1 - 2.0
  const savedSpeed = localStorage.getItem(STORAGE_SPEED_KEY)
  let initialSpeed = savedSpeed ? parseFloat(savedSpeed) : 1.0
  // Validate range (0.1 to 2.0)
  if (isNaN(initialSpeed) || initialSpeed < 0.1 || initialSpeed > 2.0) {
    initialSpeed = 1.0
  }
  const speedMultiplier = ref(initialSpeed)

  // Heartbeat interval (seconds) - for Video quests API heartbeat requests
  const STORAGE_INTERVAL_KEY = 'questHelper_heartbeatInterval'
  const savedInterval = localStorage.getItem(STORAGE_INTERVAL_KEY)
  let initialInterval = savedInterval ? parseInt(savedInterval) : 15
  // Validate range (10 to 30)
  if (isNaN(initialInterval) || initialInterval < 10 || initialInterval > 30) {
    initialInterval = 15
  }
  const heartbeatInterval = ref(initialInterval)

  // Game polling interval (seconds) - for Play/Game quests progress detection
  const STORAGE_GAME_POLLING_KEY = 'questHelper_gamePollingInterval'
  const savedGamePolling = localStorage.getItem(STORAGE_GAME_POLLING_KEY)
  let initialGamePolling = savedGamePolling ? parseInt(savedGamePolling) : 120
  // Validate range (30 to 300)
  if (isNaN(initialGamePolling) || initialGamePolling < 30 || initialGamePolling > 300) {
    initialGamePolling = 120
  }
  const gamePollingInterval = ref(initialGamePolling)

  // Game Quest Mode - 'simulate' runs a fake game exe, 'heartbeat' sends direct API heartbeats, 'cdp' injects via CDP
  const STORAGE_GAME_QUEST_MODE_KEY = 'questHelper_gameQuestMode'
  const savedGameQuestMode = localStorage.getItem(STORAGE_GAME_QUEST_MODE_KEY)
  const gameQuestMode = ref<GameQuestMode>(
    savedGameQuestMode === 'heartbeat' ? 'heartbeat'
    : savedGameQuestMode === 'cdp' ? 'cdp'
    : 'simulate'
  )

  // Read-only platform capability descriptor (loaded once from the backend).
  const platformCapabilities = ref<PlatformCapabilities | null>(null)
  // True once the first load attempt has settled (success or failure), so the UI
  // can avoid rendering platform-dependent affordances against a null descriptor.
  const platformCapabilitiesReady = ref(false)
  // In-flight load, so the fire-and-forget call at store creation and the
  // awaiting callers (startPlay, the Home pre-selection flows) share one fetch.
  let platformCapabilitiesInFlight: Promise<PlatformCapabilities | null> | null = null

  /**
   * Load the platform capability descriptor. On first run (no saved mode) this
   * applies the platform's default Play-Quest mode — 'cdp' on Linux, 'simulate'
   * on Windows/macOS — without ever overriding a preference the user has set.
   * Safe to call more than once and from several callers concurrently; only the
   * first call fetches, and later ones resolve from the cached descriptor.
   */
  async function initPlatformCapabilities(): Promise<PlatformCapabilities | null> {
    if (platformCapabilities.value) return platformCapabilities.value
    if (platformCapabilitiesInFlight) return platformCapabilitiesInFlight

    platformCapabilitiesInFlight = (async () => {
      try {
        const caps = await getPlatformCapabilities()
        platformCapabilities.value = caps
        // Re-read at resolve time rather than trusting `savedGameQuestMode`,
        // which is a store-setup snapshot. If the user picked a mode from
        // Settings while this fetch was in flight, the `gameQuestMode` watcher
        // has already persisted it, and applying the platform default here
        // would silently clobber that fresh choice.
        const currentSavedMode = localStorage.getItem(STORAGE_GAME_QUEST_MODE_KEY)
        if (
          currentSavedMode === null &&
          (caps.defaultGameQuestMode === 'simulate' ||
            caps.defaultGameQuestMode === 'heartbeat' ||
            caps.defaultGameQuestMode === 'cdp')
        ) {
          gameQuestMode.value = caps.defaultGameQuestMode
        }
        return caps
      } catch (error) {
        console.warn('Failed to load platform capabilities:', error)
        return null
      } finally {
        platformCapabilitiesReady.value = true
        platformCapabilitiesInFlight = null
      }
    })()

    return platformCapabilitiesInFlight
  }

  // Recoverable soft error (e.g. win32-only game on Linux simulate mode) that
  // the UI surfaces as an actionable dialog rather than a fatal error.
  const softError = ref<QuestSoftError | null>(null)
  const queuePauseReason = ref<QueuePauseReason | null>(null)
  // Remember the last Play-Quest request so "switch to CDP and retry" can resume it.
  const lastPlayRequest = ref<{ quest: Quest; secondsNeeded: number; initialProgress: number; selectedExeName?: string } | null>(null)

  // CDP availability status
  const cdpAvailable = ref(false)

  // CDP Port - default 9223, user configurable
  const STORAGE_CDP_PORT_KEY = 'questHelper_cdpPort'
  const savedCdpPort = localStorage.getItem(STORAGE_CDP_PORT_KEY)
  const cdpPort = ref(savedCdpPort ? parseInt(savedCdpPort) : 9223)

  const STORAGE_DESKTOP_CLIENT_KEY = 'questHelper_desktopClient'
  const savedDesktopClient = localStorage.getItem(STORAGE_DESKTOP_CLIENT_KEY)
  const desktopClient = ref<DesktopClientArg>(
    savedDesktopClient === 'official' || savedDesktopClient === 'vesktop' || savedDesktopClient === 'auto'
      ? savedDesktopClient
      : 'auto',
  )

  // Optional display: account Orbs balance. Disabled by default to avoid extra requests.
  const STORAGE_SHOW_ORBS_BALANCE_KEY = 'questHelper_showOrbsBalance'
  const savedShowOrbsBalance = localStorage.getItem(STORAGE_SHOW_ORBS_BALANCE_KEY)
  const showOrbsBalance = ref(savedShowOrbsBalance === null ? true : savedShowOrbsBalance === 'true')

  // Activity quest checkpoint interval (seconds) - min/max time between checkpoints
  const STORAGE_ACTIVITY_CHECKPOINT_MIN_KEY = 'questHelper_activityCheckpointMin'
  const savedCheckpointMin = localStorage.getItem(STORAGE_ACTIVITY_CHECKPOINT_MIN_KEY)
  let initialCheckpointMin = savedCheckpointMin ? parseInt(savedCheckpointMin) : 180
  if (isNaN(initialCheckpointMin) || initialCheckpointMin < 30 || initialCheckpointMin > 600) {
    initialCheckpointMin = 180
  }
  const activityCheckpointMin = ref(initialCheckpointMin)

  const STORAGE_ACTIVITY_CHECKPOINT_MAX_KEY = 'questHelper_activityCheckpointMax'
  const savedCheckpointMax = localStorage.getItem(STORAGE_ACTIVITY_CHECKPOINT_MAX_KEY)
  let initialCheckpointMax = savedCheckpointMax ? parseInt(savedCheckpointMax) : 300
  if (isNaN(initialCheckpointMax) || initialCheckpointMax < 60 || initialCheckpointMax > 900) {
    initialCheckpointMax = 300
  }
  const activityCheckpointMax = ref(initialCheckpointMax)

  // Persist speed changes to localStorage
  watch(speedMultiplier, (newSpeed) => {
    localStorage.setItem(STORAGE_SPEED_KEY, String(newSpeed))
  })

  // Persist heartbeat interval changes
  watch(heartbeatInterval, (newInterval) => {
    localStorage.setItem(STORAGE_INTERVAL_KEY, String(newInterval))
  })

  // Persist game polling interval changes
  watch(gamePollingInterval, (newInterval) => {
    localStorage.setItem(STORAGE_GAME_POLLING_KEY, String(newInterval))
  })

  // Persist game quest mode changes
  watch(gameQuestMode, (newMode) => {
    localStorage.setItem(STORAGE_GAME_QUEST_MODE_KEY, newMode)
  })

  // Persist CDP port changes
  watch(cdpPort, (newPort) => {
    localStorage.setItem(STORAGE_CDP_PORT_KEY, String(newPort))
  })

  watch(desktopClient, (client) => {
    localStorage.setItem(STORAGE_DESKTOP_CLIENT_KEY, client)
  })

  watch(showOrbsBalance, (enabled) => {
    localStorage.setItem(STORAGE_SHOW_ORBS_BALANCE_KEY, String(enabled))
    if (enabled && orbsBalance.value == null) {
      fetchOrbsBalance().catch(err => {
        console.warn('Background Orbs balance fetch failed:', err)
      })
    }
  })

  function normalizeCheckpoint(value: number, fallback: number, min: number, max: number): number {
    if (!Number.isFinite(value)) return fallback
    const n = Math.round(value)
    return Math.min(max, Math.max(min, n))
  }

  // Persist activity checkpoint interval changes
  watch(activityCheckpointMin, (newMin) => {
    const normalizedMin = normalizeCheckpoint(newMin, 180, 30, 600)
    if (normalizedMin !== newMin) {
      activityCheckpointMin.value = normalizedMin
      return
    }
    localStorage.setItem(STORAGE_ACTIVITY_CHECKPOINT_MIN_KEY, String(normalizedMin))
    // Ensure max >= min
    if (activityCheckpointMax.value < normalizedMin) {
      activityCheckpointMax.value = normalizedMin
    }
  })

  watch(activityCheckpointMax, (newMax) => {
    const normalizedMax = normalizeCheckpoint(newMax, 300, 60, 900)
    if (normalizedMax !== newMax) {
      activityCheckpointMax.value = normalizedMax
      return
    }
    localStorage.setItem(STORAGE_ACTIVITY_CHECKPOINT_MAX_KEY, String(normalizedMax))
    // Ensure min <= max
    if (activityCheckpointMin.value > normalizedMax) {
      activityCheckpointMin.value = normalizedMax
    }
  })

  let progressUnlisten: (() => void) | null = null
  let completeUnlisten: (() => void) | null = null
  let errorUnlisten: (() => void) | null = null
  let stoppedUnlisten: (() => void) | null = null
  let listenersActive = false
  let pollingTimer: ReturnType<typeof setInterval> | null = null

  // Simulation internal vars
  let simAnimationFrame: number | null = null
  let simLastTime = 0
  let simCurrentSpeed = 1.0

  async function fetchQuests(silent = false, force = false) {
    if (!force && quests.value.length > 0) {
      const now = Date.now()
      // 30 minutes cache
      if (now - lastQuestsFetchTime.value < 30 * 60 * 1000) {
        console.log('Using cached quests list')
        return
      }
    }

    if (!silent) loading.value = true
    error.value = null
    // Capture BOTH the account identity and the account epoch BEFORE the await.
    // A late response for a previous account must never populate the new one.
    const accountIdAtStart = activeAccountId.value
    const epochAtStart = accountEpoch
    try {
      console.log('Fetching quests from API...')
      const response = await getQuestsFull()
      if (accountIdAtStart !== activeAccountId.value || epochAtStart !== accountEpoch) {
        // Switched while this fetch was in flight: discard it.
        return
      }
      quests.value = response.quests
      excludedQuests.value = response.excluded_quests || []
      questEnrollmentBlockedUntil.value = response.quest_enrollment_blocked_until || null
      lastQuestsFetchTime.value = Date.now()
    } catch (e) {
      if (accountIdAtStart !== activeAccountId.value || epochAtStart !== accountEpoch) {
        return
      }
      error.value = e instanceof Error ? e.message : String(e)
    } finally {
      if (!silent && accountIdAtStart === activeAccountId.value && epochAtStart === accountEpoch) {
        loading.value = false
      }
    }
  }

  let orbsFetchGeneration = 0

  async function fetchOrbsBalance(force = false) {
    const generationAtStart = orbsFetchGeneration
    if (!showOrbsBalance.value && !force) return
    if (orbsBalanceLoading.value) return
    if (!force && orbsBalance.value != null) return

    orbsBalanceLoading.value = true
    orbsBalanceError.value = null
    try {
      const balance = await getVirtualCurrencyBalance()
      if (generationAtStart !== orbsFetchGeneration) return
      orbsBalance.value = balance
      orbsBalanceFetchedAt.value = new Date().toISOString()
    } catch (e) {
      if (generationAtStart !== orbsFetchGeneration) return
      orbsBalanceError.value = e as string
      throw e
    } finally {
      if (generationAtStart === orbsFetchGeneration) {
        orbsBalanceLoading.value = false
      }
    }
  }

  function checkActiveQuestStatus() {
    // Polling only tracks a simulate-mode game, which is not a registry run.
    const sim = manualSimulation.value
    if (!sim) return
    const quest = quests.value.find(q => q.id === sim.questId)
    if (!quest) return

    // Check completion
    if (quest.user_status?.completed_at) {
      manualSimulation.value = null
      // If queue is running, handle transition to next quest instead of full stop
      if (isQueueRunning.value && questQueue.value.length > 0) {
        console.log('Queue item completed detected via polling.')
        const finished = questQueue.value.shift()
        console.log(`Queue item finished: ${finished?.id}. Remaining: ${questQueue.value.length}`)

        // Reset active state
        activeQuestId.value = null
        activeQuestType.value = null
        activeQuestProgress.value = 0
        activeQuestTargetDuration.value = 0
        activeGameExe.value = null
        localProgress.value = 0
        stopProgressSimulation()
        stopPolling()

        // Refresh quests to update status in UI
        fetchQuests(true, true)

        // Process next item after a short delay
        setTimeout(() => {
          processQueue()
        }, 2000)
        return
      }

      console.log('Quest completed detected via polling, stopping game.')
      stop()
      return
    }

    // Update progress
    const progressObj = quest.user_status?.progress
    let currentSeconds = 0
    if (progressObj && typeof progressObj === 'object') {
      const vals = Object.values(progressObj as Record<string, { value?: number }>)
      if (vals.length > 0 && vals[0]?.value) currentSeconds = vals[0].value
    }

    const target = activeQuestTargetDuration.value
    if (target > 0) {
      const pct = (currentSeconds / target) * 100
      activeQuestProgress.value = pct
    }
  }

  function startPolling() {
    if (pollingTimer) clearInterval(pollingTimer)
    // Use user-configurable game polling interval (in seconds, convert to ms)
    const intervalMs = gamePollingInterval.value * 1000
    pollingTimer = setInterval(async () => {
      await fetchQuests(true, true)
      checkActiveQuestStatus()
    }, intervalMs)
  }

  function stopPolling() {
    if (pollingTimer) {
      clearInterval(pollingTimer)
      pollingTimer = null
    }
  }

  // --- Local Progress Simulation ---
  function startProgressSimulation(speed: number) {
    stopProgressSimulation() // Clear any existing
    simCurrentSpeed = speed
    simLastTime = Date.now()
    localProgress.value = activeQuestProgress.value

    // Loop
    const loop = () => {
      if (!activeQuestId.value || activeQuestProgress.value >= 100) {
        stopProgressSimulation()
        return
      }

      const now = Date.now()
      const deltaSeconds = (now - simLastTime) / 1000
      simLastTime = now

      const targetSeconds = activeQuestTargetDuration.value
      if (targetSeconds > 0) {
        const addedPercent = (deltaSeconds * simCurrentSpeed / targetSeconds) * 100
        localProgress.value += addedPercent
      }

      // Clamp logic:
      // Always at least activeQuestProgress (blue bar)
      // Never more than 100
      localProgress.value = Math.max(localProgress.value, activeQuestProgress.value)
      localProgress.value = Math.min(localProgress.value, 100)

      simAnimationFrame = requestAnimationFrame(loop)
    }

    simAnimationFrame = requestAnimationFrame(loop)
  }

  function stopProgressSimulation() {
    if (simAnimationFrame !== null) {
      cancelAnimationFrame(simAnimationFrame)
      simAnimationFrame = null
    }
  }

  // Watch activeQuestProgress to re-anchor local progress
  // If backend reports new progress (blue bar jumps), update local (green bar) to ensure it's not lagging behind
  watch(activeQuestProgress, (newVal) => {
    localProgress.value = Math.max(localProgress.value, newVal)
  })

  // Update a quest's enrollment status locally (no full refresh)
  function updateQuestEnrollment(questId: string, enrolledAt: string) {
    const questIndex = quests.value.findIndex(q => q.id === questId)
    if (questIndex !== -1) {
      const quest = quests.value[questIndex]
      // Create new user_status or update existing one
      quests.value[questIndex] = {
        ...quest,
        user_status: {
          ...quest.user_status,
          enrolled_at: enrolledAt,
          completed_at: quest.user_status?.completed_at || null,
          claimed_at: quest.user_status?.claimed_at || null,
          progress: quest.user_status?.progress || {}
        }
      }
    }
  }

  async function startVideo(questId: string, secondsNeeded: number, initialProgress: number) {
    try {
      const progressPct = (secondsNeeded > 0) ? (initialProgress / secondsNeeded) * 100 : 0

      let run: QuestRunDto
      if (gameQuestMode.value === 'cdp') {
        // CDP mode: use Discord's internal api.post() for video progress
        run = await startCdpQuestRun(questId, 'video', '', '', secondsNeeded, initialProgress, cdpPort.value)
      } else {
        console.log(`[startVideo] mode=${gameQuestMode.value} speed=${speedMultiplier.value}x interval=${heartbeatInterval.value}s`)
        run = await startVideoQuestRun(questId, secondsNeeded, progressPct, speedMultiplier.value, heartbeatInterval.value)
      }

      registerRun(run, { questType: 'video', targetDuration: secondsNeeded, progressOverride: progressPct })

      // CDP video progress is server-enforced real-time; don't inflate local simulation
      startProgressSimulation(gameQuestMode.value === 'cdp' ? 1.0 : speedMultiplier.value)
      setupListeners()
      scheduleRefresh()
    } catch (e) {
      error.value = e instanceof Error ? e.message : String(e)
      throw e
    }
  }

  async function startStream(questId: string, streamKey: string, secondsNeeded: number, initialProgress: number) {
    try {
      const progressPct = (secondsNeeded > 0) ? (initialProgress / secondsNeeded) * 100 : 0
      const run = await startStreamQuestRun(questId, streamKey, secondsNeeded, progressPct)
      registerRun(run, { questType: 'stream', targetDuration: secondsNeeded, progressOverride: progressPct })

      startProgressSimulation(1.0)
      setupListeners()
      scheduleRefresh()
    } catch (e) {
      error.value = e instanceof Error ? e.message : String(e)
      throw e
    }
  }

  async function startPlay(quest: Quest, secondsNeeded: number, initialProgress: number, selectedExeName?: string) {
    loading.value = true
    error.value = null
    // Remember the request so a soft error can offer "switch to CDP and retry".
    lastPlayRequest.value = { quest, secondsNeeded, initialProgress, selectedExeName }
    try {
      // 1. Get Application ID
      const appId = quest.config.application?.id
      if (!appId) throw new Error('Quest missing application ID')

      // Check mode: 'cdp' uses CDP injection, 'heartbeat' uses direct API calls, 'simulate' runs fake game
      if (gameQuestMode.value === 'cdp') {
        // CDP mode - inject into Discord client, no game simulation needed
        console.log(`Starting game quest via CDP for AppID: ${appId}`)
        const appName = quest.config.application?.name || quest.config.messages.game_title || 'Game'

        const progressPct = (secondsNeeded > 0) ? (initialProgress / secondsNeeded) * 100 : 0
        const run = await startCdpQuestRun(
          quest.id,
          'play',
          appId,
          appName,
          secondsNeeded,
          initialProgress,
          cdpPort.value
        )
        registerRun(run, { questType: 'game', targetDuration: secondsNeeded, progressOverride: progressPct })

        startProgressSimulation(1.0)
        setupListeners()
        scheduleRefresh()

      } else if (gameQuestMode.value === 'heartbeat') {
        // [LEGACY] Direct heartbeat mode - no game simulation needed
        console.log(`Starting game quest via direct heartbeat for AppID: ${appId}`)

        const progressPct = (secondsNeeded > 0) ? (initialProgress / secondsNeeded) * 100 : 0
        const run = await startGameHeartbeatQuestRun(
          quest.id,
          appId,
          secondsNeeded,
          progressPct
        )
        registerRun(run, { questType: 'game', targetDuration: secondsNeeded, progressOverride: progressPct })

        startProgressSimulation(1.0)

        // Setup listeners for progress/complete/error events
        setupListeners()
        scheduleRefresh()

      } else {
        // Simulate mode - original behavior
        // 2. Fetch detectable games to find executable name
        // Use cached list if available
        const gamesList = await getDetectableGames()
        const game = gamesList.find(g => g.id === appId)
        if (!game) throw new Error(`Game not found in Discord's detectable list (AppID: ${appId})`)

        // Resolve a platform-compatible executable. On Windows/macOS this stays
        // win32-first; on Linux a native `linux` executable is preferred and a
        // win32-only game raises a recoverable soft error steering the user to
        // CDP mode (a Windows binary can't be process-simulated on Linux).
        // Await rather than reading the ref: capabilities load fire-and-forget
        // at store creation, and a null descriptor here would fall back to
        // 'win32' and happily accept a Windows executable on Linux — exactly
        // the case the soft error below exists to prevent. Resolves instantly
        // once loaded.
        const caps = await initPlatformCapabilities()
        if (!caps) {
          throw new Error('Unable to determine platform capabilities. Please try again.')
        }
        const hostOs = caps.os
        const resolution = resolveSimulationExecutable(game.executables, hostOs, selectedExeName)
        if (resolution.kind === 'win32_only_on_linux') {
          softError.value = {
            code: 'SIMULATION_EXECUTABLE_OS_UNSUPPORTED',
            message: `"${game.name}" only provides a Windows executable, which cannot be process-simulated on Linux. Switch to CDP mode to complete this quest.`,
            gameName: game.name,
            questId: quest.id,
            recommendedMode: 'cdp',
            recoverable: true,
          }
          if (isQueueRunning.value) queuePauseReason.value = 'simulation_incompatible'
          loading.value = false
          return
        }
        if (resolution.kind === 'not_found') {
          softError.value = {
            code: 'SIMULATION_EXECUTABLE_NOT_FOUND',
            message: `No compatible executable definition for game ${game.name}. Switch to CDP mode to complete this quest.`,
            gameName: game.name,
            questId: quest.id,
            recommendedMode: 'cdp',
            recoverable: true,
          }
          if (isQueueRunning.value) queuePauseReason.value = 'simulation_incompatible'
          loading.value = false
          return
        }
        const exeName = resolution.executable.name

        console.log(`Starting simulated game for ${game.name} (${exeName})...`)

        // 3. Setup path (use the localized Documents dir; avoids assuming ~/Documents)
        const documents = await documentDir()
        const installPath = await join(documents, 'DiscordQuestGames')

        // 4. Create simulated game executable
        await createSimulatedGame(installPath, exeName, appId)
        activeGameExe.value = exeName

        // 5. Run simulated game
        await runSimulatedGame(game.name, installPath, exeName, appId)

        // 6. Connect RPC
        const activity = {
          app_id: appId,
          state: "In Game",
          details: `Playing ${game.name}`,
          largeImageKey: "logo",
          largeImageText: game.name,
          timestamp: Date.now()
        }

        await connectToDiscordRpc(JSON.stringify(activity), 'connect')

        // 7. Update state. Simulate mode is not a registry run, so it lives in
        // the compatibility slot instead of `runsByQuestId`.
        manualSimulation.value = {
          questId: quest.id,
          targetDuration: secondsNeeded,
          progressPct: (secondsNeeded > 0) ? (initialProgress / secondsNeeded) * 100 : 0,
        }
        syncLegacyProjection()

        startProgressSimulation(1.0)

        // Start polling for Play quests (no backend events)
        setupListeners()
        startPolling()
      }
    } catch (e) {
      error.value = e instanceof Error ? e.message : String(e)
      if (manualSimulation.value?.questId === quest.id) {
        manualSimulation.value = null
        syncLegacyProjection()
      }
      // Clean up if started (only for simulate mode)
      if (activeGameExe.value) {
        try {
          await stopSimulatedGame(activeGameExe.value)
        } catch { }
        activeGameExe.value = null
      }
      throw e
    } finally {
      loading.value = false
    }
  }

  async function startActivity(quest: Quest) {
    loading.value = true
    error.value = null
    try {
      // Activity quests require CDP mode
      if (!cdpAvailable.value) {
        throw new Error('Activity quests require CDP mode. Please start Discord with --remote-debugging-port and enable CDP in Settings.')
      }

      // Get checkpoint count from task config (default 3)
      const tasks = quest.config.task_config_v2?.tasks ?? quest.config.task_config?.tasks
      const activityTaskEntry = tasks ? Object.entries(tasks).find(([key, task]) =>
        (task.type || key) === 'ACHIEVEMENT_IN_ACTIVITY'
      ) : null
      const activityTaskKey = activityTaskEntry?.[0]
      const activityTask = activityTaskEntry?.[1] ?? null
      if (!activityTaskEntry || !activityTask) {
        throw new Error('Quest does not contain a supported checkpoint Activity task')
      }
      const checkpointCount = activityTask?.target || 3
      const currentProgress = quest.user_status?.progress
      const currentCheckpointValue = activityTaskKey && currentProgress?.[activityTaskKey]?.value != null
        ? currentProgress[activityTaskKey].value ?? 0
        : Object.values(currentProgress ?? {})[0]?.value ?? 0
      const completedCheckpoints = Math.min(
        checkpointCount,
        Math.max(0, Math.floor(currentCheckpointValue))
      )
      const remainingCheckpointCount = Math.max(0, checkpointCount - completedCheckpoints)

      if (remainingCheckpointCount === 0) {
        throw new Error('Activity quest already has all checkpoints submitted. Refresh quests or claim the reward in Discord.')
      }

      // Generate random checkpoint times within [min, max] range
      const min = activityCheckpointMin.value
      const max = activityCheckpointMax.value
      const allCheckpointTimes: number[] = []
      for (let i = 0; i < checkpointCount; i++) {
        allCheckpointTimes.push(Math.floor(Math.random() * (max - min + 1)) + min)
      }
      const checkpointTimes = allCheckpointTimes.slice(completedCheckpoints)
      const totalSeconds = allCheckpointTimes.reduce((sum, t) => sum + t, 0)
      const remainingSeconds = checkpointTimes.reduce((sum, t) => sum + t, 0)
      const progressPct = checkpointCount > 0 ? (completedCheckpoints / checkpointCount) * 100 : 0

      console.log(`Starting activity quest via CDP: completed=${completedCheckpoints}/${checkpointCount}, remaining=${remainingCheckpointCount}, times=[${checkpointTimes.join(', ')}], remaining=${remainingSeconds}s, estimatedTotal=${totalSeconds}s`)

      const appId = quest.config.application?.id || ''
      const appName = quest.config.application?.name || quest.config.messages?.quest_name || 'Activity'

      const run = await startCdpQuestRun(
        quest.id,
        'activity',
        appId,
        appName,
        totalSeconds,
        completedCheckpoints,
        cdpPort.value,
        checkpointTimes
      )
      registerRun(run, { questType: 'activity', targetDuration: totalSeconds, progressOverride: progressPct })

      startProgressSimulation(1.0)
      setupListeners()
      scheduleRefresh()

    } catch (e) {
      error.value = e instanceof Error ? e.message : String(e)
      throw e
    } finally {
      loading.value = false
    }
  }

  async function startPlayActivity(quest: Quest, secondsNeeded: number, initialProgress: number) {
    loading.value = true
    error.value = null
    try {
      const appId = quest.config.application?.id
      if (!appId) throw new Error('Cloud game Activity quest is missing an application ID')

      if (gameQuestMode.value === 'cdp' && !cdpAvailable.value) {
        throw new Error('CDP mode is selected but Discord CDP is not available')
      }

      const progressPct = playActivityProgressPercentage(initialProgress, secondsNeeded)

      console.log(
        `Starting PLAY_ACTIVITY quest: mode=${gameQuestMode.value}, progress=${initialProgress}/${secondsNeeded}s, heartbeat=${heartbeatInterval.value}s, polling=${gamePollingInterval.value}s`
      )

      const run = await startPlayActivityQuestRun(
        quest.id,
        appId,
        secondsNeeded,
        initialProgress,
        gameQuestMode.value,
        cdpPort.value,
        heartbeatInterval.value,
        gamePollingInterval.value
      )
      registerRun(run, { questType: 'activity', targetDuration: secondsNeeded, progressOverride: progressPct })

      startProgressSimulation(1.0)
      setupListeners()
      scheduleRefresh()
    } catch (e) {
      // A failed admission never registered a run, and must not remove any
      // pre-existing run for this quest id.
      error.value = e instanceof Error ? e.message : String(e)
      throw e
    } finally {
      loading.value = false
    }
  }

  async function stop() {
    stopping.value = true
    console.log('questsStore.stop() called')

    stopProgressSimulation()

    try {
      // Force Save Logic for Video Quests (skip in CDP mode — progress is server-managed)
      if (activeQuestId.value && activeQuestType.value === 'video' && activeQuestTargetDuration.value > 0 && gameQuestMode.value !== 'cdp') {
        try {
          const currentSeconds = (localProgress.value / 100) * activeQuestTargetDuration.value
          // Only force if we have significant progress
          if (currentSeconds > 0) {
            console.log(`Force submitting video progress: ${currentSeconds.toFixed(1)}s (ID: ${activeQuestId.value})`)
            await forceVideoProgress(activeQuestId.value, currentSeconds)
          }
        } catch (e) {
          console.error('Failed to force submit progress on stop:', e)
        }
      }

      // If manually stopping, ensure queue is also stopped/cleared
      if (isQueueRunning.value) {
        isQueueRunning.value = false
        questQueue.value = [] // Clear queue on manual stop
      }

      let exeToStop = activeGameExe.value

      // Recovery: If activeGameExe is missing but we have a quest, try to find it
      // Only applies to simulate mode — CDP/heartbeat modes never create a real process
      if (!exeToStop && activeQuestId.value && activeQuestType.value === 'game' && gameQuestMode.value === 'simulate') {
        console.warn('activeGameExe is null, attempting to recover from activeQuestId...')
        const quest = quests.value.find(q => q.id === activeQuestId.value)
        if (quest && quest.config.application?.id) {
          try {
            const appId = quest.config.application.id
            const detectableGames = await fetchDetectableGames()
            const game = detectableGames.find(g => g.id === appId)
            if (game) {
              const winExe = game.executables.find(e => e.os === 'win32')
              if (winExe) {
                exeToStop = winExe.name
                console.log('Recovered executable name:', exeToStop)
              }
            }
          } catch (err) {
            console.error('Failed to recover executable name:', err)
          }
        }
      }

      // Stop simulated game if running (simulate mode only)
      if (exeToStop && gameQuestMode.value === 'simulate') {
        try {
          console.log(`Stopping simulated game: ${exeToStop}`)
          await stopSimulatedGame(exeToStop)
          // Disconnect RPC
          await emit('event_disconnect')
        } catch (e) {
          console.error('Failed to stop game process:', e)
        }
        activeGameExe.value = null
      }

      // Compatibility "stop all": stop every run through the registry, not the
      // legacy single-quest command. A stop timeout is resolved by the refresh
      // below, which re-adds any run the backend still reports.
      try {
        await stopAllQuests()
      } catch (e) {
        console.error('Failed to stop all quest runs:', e)
      }

      manualSimulation.value = null
      runsByKey.value = {}
      syncLegacyProjection()
      localProgress.value = 0

      cleanupListeners()

      // Refresh quests/runs to get latest status
      await fetchQuests(true, true)
      await refreshRuns()

    } finally {
      stopping.value = false
    }
  }

  /**
   * Register the global, ID-less quest event listeners once. Progress/terminal
   * events never carry a run id, so they only trigger a coalesced snapshot
   * reconciliation — never a guessed per-run mutation.
   */
  function setupListeners() {
    if (listenersActive) return
    listenersActive = true

    console.log('Setting up quest progress listeners...')

    onQuestProgress((event) => {
      applyRunProgressEvent(event)
      scheduleRefresh()
    }).then((unlisten) => {
      progressUnlisten = unlisten
      console.log('Quest progress listener ready')
    })

    onQuestComplete((event) => {
      console.log('Received quest-complete event')
      applyRunTerminalEvent(event)
      scheduleRefresh()
    }).then((unlisten) => {
      completeUnlisten = unlisten
      console.log('Quest complete listener ready')
    })

    onQuestError((event) => {
      console.log('Received quest-error event:', event.message)
      // Validate the envelope identity before mutating UI state: an error from a
      // background account must not surface in the active account's UI.
      const belongsToActiveAccount =
        activeAccountId.value === null || event.accountId === activeAccountId.value
      if (belongsToActiveAccount && event.message) error.value = event.message
      applyRunTerminalEvent(event)
      scheduleRefresh()
    }).then((unlisten) => {
      errorUnlisten = unlisten
      console.log('Quest error listener ready')
    })

    onQuestStopped((event) => {
      applyRunTerminalEvent(event)
      scheduleRefresh()
    }).then((unlisten) => {
      stoppedUnlisten = unlisten
    })
  }

  /**
   * Apply a progress envelope to EXACTLY its matching run (account+quest+run).
   * An envelope for account A can never update account B's run with the same
   * quest id, and an id-less/unknown envelope is ignored.
   */
  function applyRunProgressEvent(event: QuestEventEnvelope) {
    if (typeof event.progress !== 'number') return
    const key = runKey(event.accountId, event.questId)
    const run = runsByKey.value[key]
    if (!run || run.runId !== event.runId) return
    runsByKey.value = {
      ...runsByKey.value,
      [key]: { ...run, progress: event.progress },
    }
    syncLegacyProjection()
  }

  /** Remove EXACTLY the terminal run identified by the envelope. */
  function applyRunTerminalEvent(event: QuestEventEnvelope) {
    const key = runKey(event.accountId, event.questId)
    const run = runsByKey.value[key]
    if (!run || run.runId !== event.runId) return
    const next = { ...runsByKey.value }
    delete next[key]
    runsByKey.value = next
    syncLegacyProjection()
  }

  function cleanupListeners() {
    stopPolling()
    listenersActive = false
    if (progressUnlisten) {
      progressUnlisten()
      progressUnlisten = null
    }
    if (completeUnlisten) {
      completeUnlisten()
      completeUnlisten = null
    }
    if (errorUnlisten) {
      errorUnlisten()
      errorUnlisten = null
    }
    if (stoppedUnlisten) {
      stoppedUnlisten()
      stoppedUnlisten = null
    }
  }

  function setSpeedMultiplier(speed: number) {
    speedMultiplier.value = speed
  }

  async function acceptQuestWrapper(questId: string) {
    try {
      await acceptQuest(questId)
      // Optimistic update
      updateQuestEnrollment(questId, new Date().toISOString())
    } catch (e) {
      error.value = e instanceof Error ? e.message : String(e)
      throw e
    }
  }

  async function acceptAllQuests(questIds: string[]) {
    loading.value = true
    error.value = null
    let successCount = 0
    let failCount = 0
    try {
      for (const id of questIds) {
        try {
          await acceptQuest(id)
          updateQuestEnrollment(id, new Date().toISOString())
          successCount++
          // Small delay to be nice to API
          await new Promise(r => setTimeout(r, 500))
        } catch (e) {
          console.error(`Failed to accept quest ${id}:`, e)
          failCount++
        }
      }
    } finally {
      loading.value = false
      if (failCount > 0) {
        error.value = `Accepted ${successCount} quests, failed ${failCount}`
      }
    }
  }


  // Refined Complete All Video:
  // We can't blocking-wait in the UI thread for 15 mins x N quests.
  // But we can start a "Queue Mode".
  const questQueue = ref<QueueItem[]>([])

  /**
   * Drop all pending local queue items. Used on an account switch: pending jobs
   * belong to the previous account and must never run under the new one.
   */
  function clearPendingQueue() {
    questQueue.value = []
  }
  const isQueueRunning = ref(false)

  let queueProcessing = false

  /**
   * Launch queued quests non-preemptively and continue to the next pending item
   * after each accepted start, so distinct REST video quests can overlap. The
   * queue no longer waits for ID-less terminal events. On a resource-busy or
   * failed admission the item is retained (never silently dropped) and the queue
   * pauses with the exact backend error.
   */
  async function processQueue() {
    if (queueProcessing) return
    queueProcessing = true
    isQueueRunning.value = true
    const epochAtStart = queueEpoch
    try {
      while (questQueue.value.length > 0) {
        // A queue run begun for account A must never start a quest against
        // newly active account B.
        if (epochAtStart !== queueEpoch) {
          isQueueRunning.value = false
          return
        }
        const queueItem = questQueue.value[0]
        console.log(`Queue processing: ${queueItem.id}`)

        // If completed, skip
        if (queueItem.user_status?.completed_at) {
          questQueue.value.shift()
          continue
        }

        // Calculate duration needed
        let seconds = 0
        const queueTasks = queueItem.config.task_config_v2?.tasks ?? queueItem.config.task_config?.tasks
        if (queueTasks) {
          const taskValues = Object.values(queueTasks)
          if (taskValues.length > 0) seconds = taskValues[0].target || 0
        }

        // Check if already partial
        let progress = 0
        if (queueItem.user_status?.progress) {
          const vals = Object.values(queueItem.user_status.progress)
          if (vals.length > 0) progress = vals[0].value || 0
        }

        // Route by quest type
        const questKind = getQuestKind(queueItem)
        console.log(`Queue item type: ${questKind}`)

        // Registry-backed game/stream/activity runs are stopped by the backend
        // with `resource_busy` when another owns account activity. Simulate mode
        // is not a registry run, so guard it locally to avoid overlapping
        // simulations. REST videos intentionally continue to overlap.
        if (questKind !== 'video' && manualSimulation.value) {
          error.value = 'resource_busy: a simulated game is already active'
          return
        }

        try {
          if (questKind === 'video') {
            await startVideo(queueItem.id, seconds, progress)
          } else {
            // Game (stream/play) quests — use startPlay with optional pre-selected exe
            await startPlay(queueItem, seconds, progress, queueItem.selectedExeName)
          }
        } catch (e) {
          console.error('Queue admission failed:', e)
          error.value = e instanceof Error ? e.message : String(e)
          // Retain the unstarted item; do not drop it silently.
          return
        }

        const admitted = !!getRun(queueItem.id, activeAccountId.value ?? undefined)
          || manualSimulation.value?.questId === queueItem.id
        if (!admitted) {
          // Soft-paused (e.g. simulation-incompatible); keep the item for retry.
          return
        }

        questQueue.value.shift()
        scheduleRefresh()
      }
      isQueueRunning.value = false
    } finally {
      queueProcessing = false
    }
  }

  // We need to modify `onQuestComplete` to trigger next in queue.
  // See `setupListeners`.

  // --- Detectable Games Caching ---
  const detectableGames = ref<DetectableGame[]>([])
  const fetchingGames = ref(false)

  async function getDetectableGames(force = false): Promise<DetectableGame[]> {
    if (!force && detectableGames.value.length > 0) {
      console.log('Returning cached detectable games')
      return detectableGames.value
    }

    if (fetchingGames.value) {
      // If already fetching, wait for it (simple poll)
      while (fetchingGames.value) {
        await new Promise(r => setTimeout(r, 100))
      }
      return detectableGames.value
    }

    fetchingGames.value = true
    try {
      console.log('Fetching detectable games from API...')
      detectableGames.value = await fetchDetectableGames()
      console.log(`Fetched ${detectableGames.value.length} detectable games successfully.`)
      return detectableGames.value
    } catch (e) {
      console.error('Failed to fetch detectable games:', e)
      throw e
    } finally {
      fetchingGames.value = false
    }
  }

  function resetForLogout() {
    orbsFetchGeneration++
    quests.value = []
    excludedQuests.value = []
    questEnrollmentBlockedUntil.value = null
    lastQuestsFetchTime.value = 0
    loading.value = false
    error.value = null
    orbsBalance.value = null
    orbsBalanceFetchedAt.value = null
    orbsBalanceLoading.value = false
    orbsBalanceError.value = null
    activeQuestId.value = null
    activeQuestType.value = null
    activeQuestProgress.value = 0
    activeQuestTargetDuration.value = 0
    localProgress.value = 0
    activeGameExe.value = null
    runsByKey.value = {}
    manualSimulation.value = null
    if (refreshTimer !== null) {
      clearTimeout(refreshTimer)
      refreshTimer = null
    }
    questQueue.value = []
    isQueueRunning.value = false
    stopping.value = false
    detectableGames.value = []
    fetchingGames.value = false
    cdpAvailable.value = false
    // Recoverable-error state is per-session: a dialog left open (or a queued
    // retry) must not reappear against the next account's quests.
    softError.value = null
    queuePauseReason.value = null
    lastPlayRequest.value = null
    stopProgressSimulation()
    cleanupListeners()
    stopPolling()
  }

  // Check CDP availability and auto-fallback if mode is 'cdp' but CDP isn't reachable
  async function initCdpMode() {
    try {
      const status = await checkCdpStatus(cdpPort.value)
      cdpAvailable.value = status.connected
      if (gameQuestMode.value === 'cdp' && !status.connected) {
        console.warn('CDP mode selected but CDP not available — falling back to simulate mode')
        gameQuestMode.value = 'simulate'
      }
    } catch {
      cdpAvailable.value = false
      if (gameQuestMode.value === 'cdp') {
        console.warn('CDP check failed — falling back to simulate mode')
        gameQuestMode.value = 'simulate'
      }
    }
  }

  /**
   * Recover from a recoverable soft error (e.g. a win32-only game on Linux) by
   * switching to CDP mode and retrying the same quest / resuming the queue.
   * Falls back to a clear error if CDP can't be reached.
   */
  async function switchToCdpAndRetry(): Promise<void> {
    const req = lastPlayRequest.value
    gameQuestMode.value = 'cdp'

    // Confirm the CDP port is actually reachable; initCdpMode flips back to
    // simulate when it isn't, so re-check availability afterward. The soft
    // error and pause reason are only cleared once CDP is confirmed: dropping
    // them on a failed check would leave a paused queue reporting "running"
    // with no active quest and no way to retry from the dialog.
    await initCdpMode()
    if (!cdpAvailable.value) {
      error.value =
        'CDP mode is not available. Start Discord with CDP enabled (Settings → Discord integration), then try again.'
      return
    }

    error.value = null
    softError.value = null
    queuePauseReason.value = null

    if (isQueueRunning.value) {
      await processQueue()
    } else if (req) {
      try {
        await startPlay(req.quest, req.secondsNeeded, req.initialProgress, req.selectedExeName)
      } catch (error) {
        // startPlay already records the user-facing error; do not let the
        // dialog action become an unhandled rejection.
        console.warn('CDP retry failed:', error)
      }
    }
  }

  /**
   * Dismiss a soft error without switching modes. If a queue was paused because
   * the current item is simulation-incompatible, the user's cancel means
   * "skip it" — drop the head item and continue the queue.
   */
  function dismissSoftError(): void {
    const wasSimIncompatible = queuePauseReason.value === 'simulation_incompatible'
    softError.value = null
    queuePauseReason.value = null

    if (wasSimIncompatible && isQueueRunning.value && questQueue.value.length > 0) {
      questQueue.value.shift()
      void processQueue()
    }
  }

  // Load platform capabilities on store creation so the Linux CDP-first default
  // and platform-aware executable resolution are ready before any quest runs.
  // Fire-and-forget; failure is handled inside the action.
  void initPlatformCapabilities()

  // Phase 4A: register the global ID-less quest listeners and reconcile the
  // registry snapshot once at startup.
  setupListeners()
  void refreshRuns()

  return {
    quests,
    excludedQuests,
    questEnrollmentBlockedUntil,
    loading,
    error,
    orbsBalance,
    orbsBalanceFetchedAt,
    orbsBalanceLoading,
    orbsBalanceError,
    showOrbsBalance,
    activityCheckpointMin,
    activityCheckpointMax,
    activeQuestId,
    activeQuestType,
    activeQuestProgress,
    activeQuestTargetDuration,
    localProgress, // Export local progress
    speedMultiplier,
    heartbeatInterval,
    gamePollingInterval,
    gameQuestMode,
    cdpPort,
    desktopClient,
    cdpAvailable,
    stopping,
    activeGameExe,
    // Registry-backed run collection (Phase 4A designer read API)
    runsByQuestId,
    runsByKey,
    activeAccountId,
    setActiveAccount,
    accountRuns,
    activeRuns,
    refreshRuns,
    stopRun,
    getRun,
    questQueue, // Export queue
    isQueueRunning,
    fetchQuests,
    fetchOrbsBalance,
    updateQuestEnrollment,
    startVideo,
    startStream,
    startPlay,
    startActivity,
    startPlayActivity,
    stop,
    setSpeedMultiplier,
    acceptQuest: acceptQuestWrapper,
    acceptAllQuests,
    // Add to queue logic needs integration with listeners
    addToQueue: (q: Quest, selectedExeName?: string) => {
      if (!questQueue.value.find(x => x.id === q.id)) {
        const item: QueueItem = { ...q, selectedExeName }
        questQueue.value.push(item)
      }
    },
    startQueue: processQueue,
    clearQueue: () => {
      questQueue.value = []
      isQueueRunning.value = false
      stop()
    },
    // Game Process Caching
    detectableGames,
    getDetectableGames,
    resetForLogout,
    initCdpMode,
    // Platform capabilities + Linux soft-error recovery
    platformCapabilities,
    platformCapabilitiesReady,
    initPlatformCapabilities,
    softError,
    queuePauseReason,
    lastPlayRequest,
    switchToCdpAndRetry,
    dismissSoftError
  }
})
