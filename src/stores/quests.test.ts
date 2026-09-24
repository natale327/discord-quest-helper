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
  listAllQuestRuns: vi.fn(),
  stopQuestRun: vi.fn(),
  stopAccountQuestRun: vi.fn(),
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
  listAllQuestRuns: mocks.listAllQuestRuns,
  stopQuestRun: mocks.stopQuestRun,
  stopAccountQuestRun: mocks.stopAccountQuestRun,
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
  mocks.listAllQuestRuns.mockResolvedValue(initialRuns)
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
    mocks.listAllQuestRuns.mockResolvedValue([])
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
    mocks.listAllQuestRuns.mockResolvedValue([
      dto({ questId: 'q1', runId: 'r1' }),
      dto({ questId: 'q2', runId: 'r2' }),
    ])

    await store.refreshRuns()

    expect(Object.keys(store.runsByQuestId).sort()).toEqual(['q1', 'q2'])
    expect(store.activeRuns.map(run => run.questId).sort()).toEqual(['q1', 'q2'])
    expect(store.runsByQuestId.q1.runId).toBe('r1')
    expect(store.runsByQuestId.q2.runId).toBe('r2')
  })

  it('keeps same quest ids on two accounts in separate account-safe slots', async () => {
    const store = await createStore()
    mocks.listAllQuestRuns.mockResolvedValue([
      dto({ accountId: 'acct-a', questId: 'q1', runId: 'ra' }),
      dto({ accountId: 'acct-b', questId: 'q1', runId: 'rb' }),
    ])

    await store.refreshRuns()

    // No collision: both live runs are retained under distinct keys.
    expect(store.activeRuns).toHaveLength(2)
    expect(store.getRun('q1', 'acct-a')?.runId).toBe('ra')
    expect(store.getRun('q1', 'acct-b')?.runId).toBe('rb')
    // The legacy projection exposes the active account only.
    store.setActiveAccount('acct-b')
    expect(Object.keys(store.runsByQuestId)).toEqual(['q1'])
    expect(store.runsByQuestId.q1.runId).toBe('rb')
  })

  it('routes progress envelopes only to their matching run and account', async () => {
    vi.useFakeTimers()
    const store = await createStore([
      dto({ accountId: 'acct-a', questId: 'q1', runId: 'ra', progress: 0 }),
      dto({ accountId: 'acct-b', questId: 'q1', runId: 'rb', progress: 0 }),
    ])
    mocks.listAllQuestRuns.mockResolvedValue([
      dto({ accountId: 'acct-a', questId: 'q1', runId: 'ra', progress: 10 }),
      dto({ accountId: 'acct-b', questId: 'q1', runId: 'rb', progress: 10 }),
    ])

    const progress = mocks.callbacks['quest-progress']
    expect(progress).toBeTypeOf('function')
    // An envelope for A must not touch B's run with the same quest id.
    progress({ accountId: 'acct-a', questId: 'q1', runId: 'ra', progress: 42 })

    expect(store.getRun('q1', 'acct-a')?.progress).toBe(42)
    expect(store.getRun('q1', 'acct-b')?.progress).toBe(0)

    await vi.advanceTimersByTimeAsync(200)
    await flush()
    expect(store.getRun('q1', 'acct-a')?.progress).toBe(10)
    expect(store.getRun('q1', 'acct-b')?.progress).toBe(10)
  })

  it('preserves a run on stopTimeout and runIdMismatch, and clears it on stopped', async () => {
    vi.useFakeTimers()
    const store = await createStore([dto({ accountId: 'acct', questId: 'q1', runId: 'r1' })])

    mocks.stopAccountQuestRun.mockResolvedValue({ questId: 'q1', runId: 'r1', status: 'stopTimeout' })
    mocks.listAllQuestRuns.mockResolvedValue([
      dto({ accountId: 'acct', questId: 'q1', runId: 'r1', phase: 'stopping' }),
    ])

    await store.stopRun('q1', 'r1')
    expect(store.runsByQuestId.q1).toBeDefined()
    expect(store.runsByQuestId.q1.phase).toBe('stopping')
    expect(store.error).toContain('taking longer')
    // Account-scoped stop sends the target account id.
    expect(mocks.stopAccountQuestRun).toHaveBeenCalledWith('acct', 'q1', 'r1')

    await vi.advanceTimersByTimeAsync(200)
    await flush()
    expect(store.runsByQuestId.q1).toBeDefined()

    mocks.stopAccountQuestRun.mockResolvedValue({ questId: 'q1', runId: 'r1', status: 'runIdMismatch' })
    await store.stopRun('q1', 'stale-run')
    expect(store.runsByQuestId.q1).toBeDefined()
    expect(store.error).toContain('changed')

    mocks.stopAccountQuestRun.mockResolvedValue({ questId: 'q1', runId: 'r1', status: 'stopped' })
    mocks.listAllQuestRuns.mockResolvedValue([])
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

  it('uses the active account CDP port for CDP starts and keeps the global default', async () => {
    const store = await createStore()
    store.gameQuestMode = 'cdp'

    store.setActiveAccount('acct-a', 9223)
    expect(store.activeCdpPort).toBe(9223)
    mocks.startCdpQuestRun.mockResolvedValue(
      dto({ accountId: 'acct-a', questId: 'q1', runId: 'r1' }),
    )
    await store.startVideo('q1', 900, 0)
    expect(mocks.startCdpQuestRun).toHaveBeenCalledWith('q1', 'video', '', '', 900, 0, 9223)

    // Switching accounts changes the port used; the global default is untouched.
    store.setActiveAccount('acct-b', 9333)
    expect(store.activeCdpPort).toBe(9333)
    expect(store.cdpPort).toBe(9223)
    mocks.startCdpQuestRun.mockResolvedValue(
      dto({ accountId: 'acct-b', questId: 'q2', runId: 'r2' }),
    )
    await store.startVideo('q2', 900, 0)
    expect(mocks.startCdpQuestRun).toHaveBeenLastCalledWith('q2', 'video', '', '', 900, 0, 9333)
  })

  it('rejects CDP video admission before IPC for an unassigned account port', async () => {
    const store = await createStore()
    store.gameQuestMode = 'cdp'
    store.setActiveAccount('acct-b', 0)

    await expect(store.startVideo('blocked-video', 900, 0)).rejects.toThrow('cdp_port_conflict')
    expect(mocks.startCdpQuestRun).not.toHaveBeenCalled()
    expect(mocks.startVideoQuestRun).not.toHaveBeenCalled()

    // Reassigning the same account to a valid port makes the same production
    // start path issue its CDP IPC on that account port, not the global default.
    store.setActiveAccount('acct-b', 9224)
    mocks.startCdpQuestRun.mockResolvedValue(
      dto({ accountId: 'acct-b', questId: 'valid-video', runId: 'r-valid' }),
    )
    await store.startVideo('valid-video', 900, 0)

    expect(mocks.startCdpQuestRun).toHaveBeenCalledWith('valid-video', 'video', '', '', 900, 0, 9224)
    expect(store.cdpPort).toBe(9223)
  })

  it('marks CDP unavailable without checking an unassigned active port', async () => {
    const store = await createStore()
    store.setActiveAccount('blocked-account', 0)

    await store.initCdpMode()

    expect(store.cdpAvailable).toBe(false)
    expect(mocks.checkCdpStatus).not.toHaveBeenCalled()
  })

  it('checks CDP status on the active account port', async () => {
    const store = await createStore()
    store.setActiveAccount('acct-a', 9555)

    await store.initCdpMode()

    expect(mocks.checkCdpStatus).toHaveBeenCalledWith(9555)
  })

  it('an account switch stops an in-flight queue from starting the next item', async () => {
    const store = await createStore()
    store.addToQueue(videoQuest('q1'))
    store.addToQueue(videoQuest('q2'))

    let resolveFirst: (value: QuestRunDto) => void = () => {}
    mocks.startVideoQuestRun.mockReturnValue(
      new Promise<QuestRunDto>(resolve => {
        resolveFirst = resolve
      }),
    )

    const running = store.startQueue()
    await flush()

    // Switch accounts while q1's start is in flight.
    store.setActiveAccount('acct-b')
    resolveFirst(dto({ accountId: 'acct-a', questId: 'q1', runId: 'r-a1' }))
    await running
    await flush()

    // q1 was started under A; q2 (a pending local A job) is cleared, never
    // started against B.
    expect(mocks.startVideoQuestRun).toHaveBeenCalledTimes(1)
    expect(store.questQueue).toHaveLength(0)
    expect(store.isQueueRunning).toBe(false)
  })

  it('discards a late quest fetch for the previous account', async () => {
    const store = await createStore()
    // Ignore any fetch triggered during store creation and control the two
    // account fetches explicitly.
    mocks.getQuestsFull.mockReset()
    let resolveA: (value: unknown) => void = () => {}
    let resolveB: (value: unknown) => void = () => {}
    mocks.getQuestsFull
      .mockImplementationOnce(() => new Promise(resolve => { resolveA = resolve }))
      .mockImplementationOnce(() => new Promise(resolve => { resolveB = resolve }))

    store.setActiveAccount('acct-a')
    await flush()
    store.setActiveAccount('acct-b')
    await flush()

    // A's late response arrives after the switch to B.
    resolveA({ quests: [videoQuest('a-only')], excluded_quests: [] })
    await flush()
    expect(store.quests.map(q => q.id)).not.toContain('a-only')

    // B's own fetch still populates B.
    resolveB({ quests: [videoQuest('b-only')], excluded_quests: [] })
    await flush()
    expect(store.quests.map(q => q.id)).toEqual(['b-only'])
  })

  it('an error envelope from a background account never surfaces in the active UI', async () => {
    const store = await createStore([
      dto({ accountId: 'acct-a', questId: 'q1', runId: 'ra' }),
      dto({ accountId: 'acct-b', questId: 'q1', runId: 'rb' }),
    ])
    store.setActiveAccount('acct-b')

    const error = mocks.callbacks['quest-error']
    expect(error).toBeTypeOf('function')
    error({ accountId: 'acct-a', questId: 'q1', runId: 'ra', message: 'background boom' })
    expect(store.error).toBeNull()

    error({ accountId: 'acct-b', questId: 'q1', runId: 'rb', message: 'active boom' })
    expect(store.error).toBe('active boom')
  })
})
