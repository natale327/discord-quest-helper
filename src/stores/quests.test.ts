import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'
import type { Quest, QuestRunDto } from '@/api/tauri'
import { useQuestsStore } from './quests'

type Listener = (...args: unknown[]) => void

const mocks = vi.hoisted(() => ({
  getQuestsFull: vi.fn(),
  startVideoQuestRun: vi.fn(),
  startStreamQuestRun: vi.fn(),
  startGameHeartbeatQuestRun: vi.fn(),
  startPlayActivityQuestRun: vi.fn(),
  startCdpQuestRun: vi.fn(),
  listQuestRuns: vi.fn(),
  stopQuestRun: vi.fn(),
  stopAllQuests: vi.fn(),
  onQuestProgress: vi.fn(),
  onQuestComplete: vi.fn(),
  onQuestError: vi.fn(),
  onQuestStopped: vi.fn(),
  createSimulatedGame: vi.fn(),
  runSimulatedGame: vi.fn(),
  stopSimulatedGame: vi.fn(),
  fetchDetectableGames: vi.fn(),
  connectToDiscordRpc: vi.fn(),
  acceptQuest: vi.fn(),
  forceVideoProgress: vi.fn(),
  checkCdpStatus: vi.fn(),
  getVirtualCurrencyBalance: vi.fn(),
  getPlatformCapabilities: vi.fn(),
  emit: vi.fn(),
  documentDir: vi.fn(),
  join: vi.fn(),
  callbacks: {} as Record<string, Listener>,
}))

vi.mock('@/api/tauri', () => ({
  getQuestsFull: mocks.getQuestsFull,
  startVideoQuestRun: mocks.startVideoQuestRun,
  startStreamQuestRun: mocks.startStreamQuestRun,
  startGameHeartbeatQuestRun: mocks.startGameHeartbeatQuestRun,
  startPlayActivityQuestRun: mocks.startPlayActivityQuestRun,
  startCdpQuestRun: mocks.startCdpQuestRun,
  listQuestRuns: mocks.listQuestRuns,
  stopQuestRun: mocks.stopQuestRun,
  stopAllQuests: mocks.stopAllQuests,
  onQuestProgress: mocks.onQuestProgress,
  onQuestComplete: mocks.onQuestComplete,
  onQuestError: mocks.onQuestError,
  onQuestStopped: mocks.onQuestStopped,
  createSimulatedGame: mocks.createSimulatedGame,
  runSimulatedGame: mocks.runSimulatedGame,
  stopSimulatedGame: mocks.stopSimulatedGame,
  fetchDetectableGames: mocks.fetchDetectableGames,
  connectToDiscordRpc: mocks.connectToDiscordRpc,
  acceptQuest: mocks.acceptQuest,
  forceVideoProgress: mocks.forceVideoProgress,
  checkCdpStatus: mocks.checkCdpStatus,
  getVirtualCurrencyBalance: mocks.getVirtualCurrencyBalance,
  getPlatformCapabilities: mocks.getPlatformCapabilities,
}))

vi.mock('@tauri-apps/api/path', () => ({
  documentDir: mocks.documentDir,
  join: mocks.join,
}))

vi.mock('@tauri-apps/api/event', () => ({
  emit: mocks.emit,
}))

function dto(overrides: Partial<QuestRunDto> & { questId: string }): QuestRunDto {
  return {
    accountId: 'acct',
    runId: `run-${overrides.questId}`,
    kind: 'video',
    transport: 'rest',
    phase: 'running',
    progress: 0,
    ...overrides,
  }
}

function videoQuest(id: string): Quest {
  return {
    id,
    config: {
      messages: { quest_name: `Quest ${id}` },
      task_config_v2: { tasks: { WATCH_VIDEO: { type: 'WATCH_VIDEO', target: 900 } } },
      application: { id: `app-${id}`, name: 'App', link: '' },
    },
    user_status: null,
  }
}

async function flush(): Promise<void> {
  for (let i = 0; i < 6; i += 1) {
    await Promise.resolve()
  }
}

async function createStore(initialRuns: QuestRunDto[] = []) {
  mocks.listQuestRuns.mockResolvedValue(initialRuns)
  const store = useQuestsStore()
  await flush()
  return store
}

function installListener(event: string) {
  return (callback: Listener) => {
    mocks.callbacks[event] = callback
    return Promise.resolve(() => {})
  }
}

describe('quests store run registry', () => {
  beforeEach(() => {
    const storage = new Map<string, string>()
    vi.stubGlobal('localStorage', {
      getItem: (key: string) => storage.get(key) ?? null,
      setItem: (key: string, value: string) => {
        storage.set(key, String(value))
      },
      removeItem: (key: string) => {
        storage.delete(key)
      },
      clear: () => storage.clear(),
    })
    vi.stubGlobal('requestAnimationFrame', () => 1)
    vi.stubGlobal('cancelAnimationFrame', () => {})

    setActivePinia(createPinia())
    vi.clearAllMocks()
    mocks.callbacks = {}

    mocks.getQuestsFull.mockResolvedValue({ quests: [], excluded_quests: [] })
    mocks.listQuestRuns.mockResolvedValue([])
    mocks.getVirtualCurrencyBalance.mockResolvedValue(0)
    mocks.getPlatformCapabilities.mockResolvedValue({
      os: 'linux',
      arch: 'x64',
      cdpLauncher: true,
      launcherEntry: true,
      gameSimulation: true,
      executableOsPriority: ['linux', 'win32'],
      defaultGameQuestMode: 'simulate',
    })
    mocks.checkCdpStatus.mockResolvedValue({
      available: true,
      connected: true,
      target_title: null,
      error: null,
    })
    mocks.stopAllQuests.mockResolvedValue({ completed: [], timedOut: [], cleanupFailed: [] })

    mocks.onQuestProgress.mockImplementation(installListener('quest-progress'))
    mocks.onQuestComplete.mockImplementation(installListener('quest-complete'))
    mocks.onQuestError.mockImplementation(installListener('quest-error'))
    mocks.onQuestStopped.mockImplementation(installListener('quest-stopped'))
  })

  afterEach(() => {
    vi.useRealTimers()
    vi.unstubAllGlobals()
  })

  it('keeps two distinct video runs after a registry snapshot', async () => {
    const store = await createStore()
    mocks.listQuestRuns.mockResolvedValue([
      dto({ questId: 'q1', runId: 'r1' }),
      dto({ questId: 'q2', runId: 'r2' }),
    ])

    await store.refreshRuns()

    expect(Object.keys(store.runsByQuestId).sort()).toEqual(['q1', 'q2'])
    expect(store.activeRuns.map(run => run.questId).sort()).toEqual(['q1', 'q2'])
    expect(store.runsByQuestId.q1.runId).toBe('r1')
    expect(store.runsByQuestId.q2.runId).toBe('r2')
  })

  it('treats ID-less progress events as a reconciliation trigger only', async () => {
    vi.useFakeTimers()
    const store = await createStore([dto({ questId: 'q1', runId: 'r1', progress: 0 })])
    mocks.listQuestRuns.mockResolvedValue([dto({ questId: 'q1', runId: 'r1', progress: 10 })])

    const progress = mocks.callbacks['quest-progress']
    expect(progress).toBeTypeOf('function')
    progress(42)

    // The bare payload must never be attributed to a run.
    expect(store.activeQuestProgress).toBe(0)
    expect(store.runsByQuestId.q1.progress).toBe(0)

    await vi.advanceTimersByTimeAsync(200)
    await flush()

    expect(store.activeQuestProgress).toBe(10)
    expect(store.runsByQuestId.q1.progress).toBe(10)
  })

  it('preserves a run on stopTimeout and runIdMismatch, and clears it on stopped', async () => {
    vi.useFakeTimers()
    const store = await createStore([dto({ questId: 'q1', runId: 'r1' })])

    mocks.stopQuestRun.mockResolvedValue({ questId: 'q1', runId: 'r1', status: 'stopTimeout' })
    mocks.listQuestRuns.mockResolvedValue([dto({ questId: 'q1', runId: 'r1', phase: 'stopping' })])

    await store.stopRun('q1', 'r1')
    expect(store.runsByQuestId.q1).toBeDefined()
    expect(store.runsByQuestId.q1.phase).toBe('stopping')
    expect(store.error).toContain('taking longer')

    await vi.advanceTimersByTimeAsync(200)
    await flush()
    expect(store.runsByQuestId.q1).toBeDefined()

    mocks.stopQuestRun.mockResolvedValue({ questId: 'q1', runId: 'r1', status: 'runIdMismatch' })
    await store.stopRun('q1', 'stale-run')
    expect(store.runsByQuestId.q1).toBeDefined()
    expect(store.error).toContain('changed')

    mocks.stopQuestRun.mockResolvedValue({ questId: 'q1', runId: 'r1', status: 'stopped' })
    mocks.listQuestRuns.mockResolvedValue([])
    await store.stopRun('q1', 'r1')
    expect(store.runsByQuestId.q1).toBeUndefined()
  })

  it('retains queued items when admission fails and drains them once admission succeeds', async () => {
    const store = await createStore()
    store.addToQueue(videoQuest('q1'))
    store.addToQueue(videoQuest('q2'))

    mocks.startVideoQuestRun.mockRejectedValue(
      new Error('resource_busy: account_activity is already in use'),
    )

    await store.startQueue()

    expect(store.questQueue.map(item => item.id)).toEqual(['q1', 'q2'])
    expect(store.error).toContain('resource_busy')
    expect(store.isQueueRunning).toBe(true)
    expect(Object.keys(store.runsByQuestId)).toHaveLength(0)

    mocks.startVideoQuestRun.mockImplementation((questId: string) =>
      Promise.resolve(dto({ questId, runId: `run-${questId}` })),
    )

    await store.startQueue()

    expect(store.questQueue).toHaveLength(0)
    expect(store.isQueueRunning).toBe(false)
    expect(Object.keys(store.runsByQuestId).sort()).toEqual(['q1', 'q2'])
  })

  it('starts a video quest through the non-preemptive command and registers the returned run', async () => {
    const store = await createStore()
    mocks.startVideoQuestRun.mockResolvedValue(dto({ questId: 'q1', runId: 'r1' }))

    await store.startVideo('q1', 900, 0)

    expect(mocks.startVideoQuestRun).toHaveBeenCalledWith('q1', 900, 0, 1, 15)
    expect(store.runsByQuestId.q1.runId).toBe('r1')
    expect(store.activeQuestId).toBe('q1')
  })
})
