import { beforeEach, describe, expect, it, vi } from 'vitest'
import {
  autoLoginViaCdp,
  getProgramRewards,
  listQuestRuns,
  onQuestStopped,
  startCdpQuestRun,
  startGameHeartbeatQuestRun,
  startPlayActivityQuestRun,
  startStreamQuestRun,
  startVideoQuestRun,
  stopAllQuests,
  stopQuestRun,
  type QuestRunDto,
} from './tauri'

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  listen: vi.fn(),
}))

vi.mock('@tauri-apps/api/core', () => ({
  Channel: vi.fn(),
  invoke: mocks.invoke,
}))

vi.mock('@tauri-apps/api/event', () => ({
  listen: mocks.listen,
}))

describe('getProgramRewards', () => {
  beforeEach(() => {
    vi.clearAllMocks()
  })

  it('normalizes Discord’s keyed rewards response', async () => {
    mocks.invoke.mockResolvedValue({
      rewards: {
        NITRO: {
          next_reward_date: '2026-09-22T00:21:08.745Z',
          program_current_state: 'active',
        },
        '2': {
          reward_program: 'XBOX',
          next_reward_date: null,
        },
      },
    })

    await expect(getProgramRewards()).resolves.toEqual(expect.arrayContaining([
      {
        reward_program: 'NITRO',
        next_reward_date: '2026-09-22T00:21:08.745Z',
        program_current_state: 'active',
      },
      {
        reward_program: 'XBOX',
        next_reward_date: null,
      },
    ]))
    expect(mocks.invoke).toHaveBeenCalledWith('get_program_rewards')
  })

  it('preserves the official array response and Nitro enum value', async () => {
    mocks.invoke.mockResolvedValue({
      rewards: [
        {
          reward_program: 0,
          next_reward_date: '2026-09-22T00:21:08.745Z',
          program_current_state: 'active',
          total_countdown_duration_ms: 2592000000,
        },
      ],
    })

    await expect(getProgramRewards()).resolves.toEqual([
      {
        reward_program: 0,
        next_reward_date: '2026-09-22T00:21:08.745Z',
        program_current_state: 'active',
        total_countdown_duration_ms: 2592000000,
      },
    ])
  })
})

describe('autoLoginViaCdp', () => {
  beforeEach(() => {
    vi.clearAllMocks()
  })

  it('is the only login command and returns the user without a raw token', async () => {
    const user = {
      id: '123',
      username: 'quest-user',
      discriminator: '0',
      avatar: null,
      global_name: 'Quest User',
    }
    mocks.invoke.mockResolvedValue(user)

    await expect(autoLoginViaCdp(9223)).resolves.toBe(user)

    expect(mocks.invoke).toHaveBeenCalledWith('auto_login_via_cdp', {
      port: 9223,
      onProgress: expect.anything(),
    })
    expect(user).not.toHaveProperty('token')
  })

  it('omits the port when none is supplied so the backend default applies', async () => {
    mocks.invoke.mockResolvedValue(null)

    await autoLoginViaCdp()

    expect(mocks.invoke).toHaveBeenCalledWith('auto_login_via_cdp', {
      port: undefined,
      onProgress: expect.anything(),
    })
  })
})

const run: QuestRunDto = {
  accountId: '1',
  questId: 'quest-1',
  runId: 'run-1',
  kind: 'video',
  transport: 'rest',
  phase: 'running',
  progress: 0,
}

describe('registry-backed quest run commands', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    mocks.listen.mockResolvedValue(() => {})
  })

  it('invokes the non-preemptive video/stream/game start commands with the legacy argument shape', async () => {
    mocks.invoke.mockResolvedValue(run)

    await startVideoQuestRun('q1', 900, 30, 1.5, 15)
    expect(mocks.invoke).toHaveBeenCalledWith('start_video_quest_run', {
      questId: 'q1',
      secondsNeeded: 900,
      initialProgress: 30,
      speedMultiplier: 1.5,
      heartbeatInterval: 15,
    })

    await startStreamQuestRun('q2', 'stream-key', 600, 0)
    expect(mocks.invoke).toHaveBeenCalledWith('start_stream_quest_run', {
      questId: 'q2',
      streamKey: 'stream-key',
      secondsNeeded: 600,
      initialProgress: 0,
    })

    await startGameHeartbeatQuestRun('q3', 'app-1', 300, 12)
    expect(mocks.invoke).toHaveBeenCalledWith('start_game_heartbeat_quest_run', {
      questId: 'q3',
      applicationId: 'app-1',
      secondsNeeded: 300,
      initialProgress: 12,
    })
  })

  it('invokes the PLAY_ACTIVITY and CDP run commands and forwards optional checkpoint times', async () => {
    mocks.invoke.mockResolvedValue(run)

    await startPlayActivityQuestRun('q4', 'app-2', 900, 0, 'cdp', 9223, 15, 120)
    expect(mocks.invoke).toHaveBeenCalledWith('start_play_activity_quest_run', {
      questId: 'q4',
      applicationId: 'app-2',
      secondsNeeded: 900,
      initialProgress: 0,
      mode: 'cdp',
      cdpPort: 9223,
      heartbeatInterval: 15,
      progressPollingInterval: 120,
    })

    await startCdpQuestRun('q5', 'activity', 'app-3', 'Name', 540, 1, 9223, [180, 180])
    expect(mocks.invoke).toHaveBeenCalledWith('start_cdp_quest_run', {
      questId: 'q5',
      questType: 'activity',
      applicationId: 'app-3',
      applicationName: 'Name',
      secondsNeeded: 540,
      initialProgress: 1,
      cdpPort: 9223,
      checkpointTimes: [180, 180],
    })

    await startCdpQuestRun('q6', 'video', '', '', 100, 0, 9223)
    expect(mocks.invoke).toHaveBeenCalledWith(
      'start_cdp_quest_run',
      expect.objectContaining({ checkpointTimes: [] }),
    )
  })

  it('lists runs and normalizes a null response to an empty array', async () => {
    mocks.invoke.mockResolvedValue(null)
    await expect(listQuestRuns()).resolves.toEqual([])
    expect(mocks.invoke).toHaveBeenCalledWith('list_quest_runs')

    mocks.invoke.mockResolvedValue([run])
    await expect(listQuestRuns()).resolves.toEqual([run])
  })

  it('stops a single run with and without a run id, and stops all runs', async () => {
    mocks.invoke.mockResolvedValue({ questId: 'q1', runId: 'run-1', status: 'stopped' })

    await stopQuestRun('q1', 'run-1')
    expect(mocks.invoke).toHaveBeenCalledWith('stop_quest_run', {
      questId: 'q1',
      runId: 'run-1',
    })

    await stopQuestRun('q1')
    expect(mocks.invoke).toHaveBeenCalledWith('stop_quest_run', {
      questId: 'q1',
      runId: undefined,
    })

    mocks.invoke.mockResolvedValue({ completed: ['q1'], timedOut: [], cleanupFailed: [] })
    await expect(stopAllQuests()).resolves.toEqual({
      completed: ['q1'],
      timedOut: [],
      cleanupFailed: [],
    })
    expect(mocks.invoke).toHaveBeenCalledWith('stop_all_quests')
  })

  it('exposes an id-less quest-stopped listener', async () => {
    const callback = vi.fn()
    await onQuestStopped(callback)

    expect(mocks.listen).toHaveBeenCalledWith('quest-stopped', expect.any(Function))

    const calls = mocks.listen.mock.calls
    const handler = calls[calls.length - 1]?.[1] as () => void
    handler()
    expect(callback).toHaveBeenCalledOnce()
  })
})
