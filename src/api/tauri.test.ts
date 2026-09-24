import { beforeEach, describe, expect, it, vi } from 'vitest'
import {
  activateAccount,
  activateOnlineAccount,
  autoAddAccountViaCdp,
  autoLoginViaCdp,
  clearProxyCredentials,
  confirmAddCdpAccount,
  getProgramRewards,
  getProxySettings,
  listAccounts,
  listAllQuestRuns,
  listQuestRuns,
  onQuestStopped,
  removeAccount,
  reconnectCdpAccount,
  setProxySettings,
  startCdpQuestRun,
  startGameHeartbeatQuestRun,
  startPlayActivityQuestRun,
  startStreamQuestRun,
  startVideoQuestRun,
  stopAccountQuestRun,
  stopAccountQuests,
  stopAllQuests,
  stopQuestRun,
  testProxyConnection,
  previewCdpIdentity,
  type AddCdpResult,
  type AuthProgress,
  type ConfirmAddCdpResult,
  type CdpIdentityPreview,
  type ProxySettingsDto,
  type ProxySettingsInput,
  type QuestEventEnvelope,
  type QuestRunDto,
} from './tauri'

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  listen: vi.fn(),
  channelCallback: null as ((progress: unknown) => void) | null,
}))

vi.mock('@tauri-apps/api/core', () => ({
  Channel: vi.fn(function (this: unknown, callback: (progress: unknown) => void) {
    mocks.channelCallback = callback
  }),
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

describe('autoAddAccountViaCdp', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    mocks.channelCallback = null
  })

  const user = {
    id: '123',
    username: 'quest-user',
    discriminator: '0',
    avatar: null,
    global_name: 'Quest User',
  }

  it('invokes auto_add_account_via_cdp with the port and progress channel', async () => {
    const result: AddCdpResult = { user, alreadyKnown: false }
    mocks.invoke.mockResolvedValue(result)

    await expect(autoAddAccountViaCdp(9224)).resolves.toBe(result)

    expect(mocks.invoke).toHaveBeenCalledWith('auto_add_account_via_cdp', {
      port: 9224,
      onProgress: expect.anything(),
    })
    expect(result.user).not.toHaveProperty('token')
    expect(typeof result.alreadyKnown).toBe('boolean')
  })

  it('reports the backend alreadyKnown flag for a duplicate capture', async () => {
    mocks.invoke.mockResolvedValue({ user, alreadyKnown: true })

    const result = await autoAddAccountViaCdp(9223)

    expect(result.alreadyKnown).toBe(true)
    expect(result.user).toEqual(user)
  })

  it('omits an unspecified port so the backend default applies', async () => {
    const result: AddCdpResult = { user, alreadyKnown: false }
    mocks.invoke.mockResolvedValue(result)

    await autoAddAccountViaCdp()

    expect(mocks.invoke).toHaveBeenCalledWith('auto_add_account_via_cdp', {
      port: undefined,
      onProgress: expect.anything(),
    })
  })

  it('forwards backend auth progress through the Channel callback', async () => {
    mocks.invoke.mockResolvedValue({ user, alreadyKnown: false })
    const onProgress = vi.fn()

    await autoAddAccountViaCdp(9224, onProgress)

    const progress: AuthProgress = {
      phase: 'capturing_cdp_session',
      current: null,
      total: null,
      valid_accounts: null,
    }
    expect(mocks.channelCallback).toBeTypeOf('function')
    mocks.channelCallback?.(progress)
    expect(onProgress).toHaveBeenCalledWith(progress)
  })
})

describe('client-first CDP identity commands', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    mocks.channelCallback = null
  })

  const user = {
    id: '123',
    username: 'quest-user',
    discriminator: '0',
    avatar: null,
    global_name: 'Quest User',
  }

  it('previews the selected port without exposing secrets', async () => {
    const preview: CdpIdentityPreview = { port: 9224, user }
    mocks.invoke.mockResolvedValue(preview)

    await expect(previewCdpIdentity(9224)).resolves.toBe(preview)

    expect(mocks.invoke).toHaveBeenCalledWith('preview_cdp_identity', { port: 9224 })
    expect(preview.user).not.toHaveProperty('token')
    expect(preview).not.toHaveProperty('secret')
  })

  it('confirms Add with expectedUserId and reports the backend status through a typed result', async () => {
    const result: ConfirmAddCdpResult = { status: 'added', user, port: 9224 }
    mocks.invoke.mockResolvedValue(result)
    const onProgress = vi.fn()

    await expect(confirmAddCdpAccount(9224, '123', onProgress)).resolves.toBe(result)

    expect(mocks.invoke).toHaveBeenCalledWith('confirm_add_cdp_account', {
      port: 9224,
      expectedUserId: '123',
      onProgress: expect.anything(),
    })
    expect(result.status).toBe('added')
    expect(result.user).not.toHaveProperty('token')

    const progress: AuthProgress = {
      phase: 'capturing_cdp_session',
      current: null,
      total: null,
      valid_accounts: null,
    }
    mocks.channelCallback?.(progress)
    expect(onProgress).toHaveBeenCalledWith(progress)
  })

  it('reconnects one explicit account and returns mismatch status without account secrets', async () => {
    const result = { status: 'identityChanged' as const, user: { ...user, id: 'A' }, port: 9225 }
    mocks.invoke.mockResolvedValue(result)
    const onProgress = vi.fn()

    await expect(reconnectCdpAccount('B', 9225, onProgress)).resolves.toBe(result)

    expect(mocks.invoke).toHaveBeenCalledWith('reconnect_cdp_account', {
      accountId: 'B',
      port: 9225,
      onProgress: expect.anything(),
    })
    expect(result.user).not.toHaveProperty('token')
    const progress: AuthProgress = {
      phase: 'validating_cdp_session',
      current: null,
      total: null,
      valid_accounts: null,
    }
    mocks.channelCallback?.(progress)
    expect(onProgress).toHaveBeenCalledWith(progress)
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

  it('delivers a typed quest-stopped envelope to the listener', async () => {
    const callback = vi.fn()
    await onQuestStopped(callback)

    expect(mocks.listen).toHaveBeenCalledWith('quest-stopped', expect.any(Function))

    const calls = mocks.listen.mock.calls
    const handler = calls[calls.length - 1]?.[1] as (event: { payload: QuestEventEnvelope }) => void
    const payload: QuestEventEnvelope = {
      accountId: '111111111111111111',
      questId: 'q1',
      runId: 'r1',
      kind: 'stopped',
    }
    handler({ payload })
    expect(callback).toHaveBeenCalledWith(payload)
  })
})

describe('account IPC wrappers', () => {
  beforeEach(() => {
    vi.clearAllMocks()
  })

  it('calls the typed account commands with camelCase arguments', async () => {
    mocks.invoke.mockResolvedValue({ accounts: [], activeAccountId: undefined })
    await listAccounts()
    expect(mocks.invoke).toHaveBeenCalledWith('list_accounts')

    mocks.invoke.mockResolvedValue({ id: '1', username: 'a', isAuthenticated: false })
    await activateAccount('1')
    expect(mocks.invoke).toHaveBeenCalledWith('activate_account', { accountId: '1' })

    mocks.invoke.mockResolvedValue({ accounts: [], activeAccountId: undefined })
    await removeAccount('1')
    expect(mocks.invoke).toHaveBeenCalledWith('remove_account', { accountId: '1' })
  })

  it('invokes the online-only activation command with the account id', async () => {
    const account = { id: 'B', username: 'b', isAuthenticated: true }
    mocks.invoke.mockResolvedValue(account)

    await expect(activateOnlineAccount('B')).resolves.toBe(account)

    expect(mocks.invoke).toHaveBeenCalledWith('activate_online_account', { accountId: 'B' })
  })

  it('calls the account-scoped run commands with the explicit account id', async () => {
    mocks.invoke.mockResolvedValue([])
    await listAllQuestRuns()
    expect(mocks.invoke).toHaveBeenCalledWith('list_all_quest_runs')

    mocks.invoke.mockResolvedValue({ questId: 'q1', runId: 'r1', status: 'stopped' })
    await stopAccountQuestRun('acct-a', 'q1', 'r1')
    expect(mocks.invoke).toHaveBeenCalledWith('stop_account_quest_run', {
      accountId: 'acct-a',
      questId: 'q1',
      runId: 'r1',
    })

    mocks.invoke.mockResolvedValue({ completed: [], timedOut: [], cleanupFailed: [] })
    await stopAccountQuests('acct-a')
    expect(mocks.invoke).toHaveBeenCalledWith('stop_account_quests', { accountId: 'acct-a' })
  })
})

// Deliberately fake values — never real endpoints or credentials.
const proxyDto: ProxySettingsDto = {
  mode: 'custom',
  endpoint: 'http://127.0.0.1:9',
  noProxy: 'localhost',
  hasCredentials: true,
}

function expectNoCredentialFields(value: unknown): void {
  const record = value as Record<string, unknown>
  expect(record).not.toHaveProperty('username')
  expect(record).not.toHaveProperty('password')
  expect(record).not.toHaveProperty('credentialRef')
  expect(record).not.toHaveProperty('credential_ref')
}

describe('proxy settings commands', () => {
  beforeEach(() => {
    vi.clearAllMocks()
  })

  it('invokes get_proxy_settings and returns only the public DTO', async () => {
    mocks.invoke.mockResolvedValue(proxyDto)

    await expect(getProxySettings()).resolves.toEqual(proxyDto)
    expect(mocks.invoke).toHaveBeenCalledWith('get_proxy_settings')

    const result = await getProxySettings()
    expectNoCredentialFields(result)
    expect(typeof result.hasCredentials).toBe('boolean')
  })

  it('forwards one-way credentials to set_proxy_settings and echoes no secrets', async () => {
    const input: ProxySettingsInput = {
      mode: 'custom',
      endpoint: 'http://127.0.0.1:9',
      noProxy: 'localhost',
      username: 'dummy-user-not-real',
      password: 'dummy-password-do-not-use',
    }
    mocks.invoke.mockResolvedValue(proxyDto)

    const result = await setProxySettings(input)

    expect(mocks.invoke).toHaveBeenCalledWith('set_proxy_settings', { input })
    expect(result).toEqual(proxyDto)
    expectNoCredentialFields(result)
  })

  it('invokes clear_proxy_credentials and reports the credential-free DTO', async () => {
    const cleared: ProxySettingsDto = {
      mode: 'custom',
      endpoint: 'http://127.0.0.1:9',
      noProxy: null,
      hasCredentials: false,
    }
    mocks.invoke.mockResolvedValue(cleared)

    await expect(clearProxyCredentials()).resolves.toEqual(cleared)
    expect(mocks.invoke).toHaveBeenCalledWith('clear_proxy_credentials')
    expectNoCredentialFields(cleared)
  })

  it('invokes test_proxy_connection and returns only status metadata', async () => {
    const probe = { ok: true, status: 200, message: 'Connected through the configured policy.' }
    mocks.invoke.mockResolvedValue(probe)

    await expect(testProxyConnection()).resolves.toEqual(probe)
    expect(mocks.invoke).toHaveBeenCalledWith('test_proxy_connection')
    expectNoCredentialFields(probe)
  })
})

describe('account proxy wrappers', () => {
  beforeEach(() => {
    vi.clearAllMocks()
  })

  function expectNoCredentialFields(dto: any) {
    expect(dto).not.toHaveProperty('username')
    expect(dto).not.toHaveProperty('password')
    expect(dto).not.toHaveProperty('overrideUsername')
    expect(dto).not.toHaveProperty('overridePassword')
  }

  it('invokes get_account_proxy_settings with the account id', async () => {
    const dto = {
      accountId: 'acct-1',
      hasOverride: false,
      overrideMode: null,
      overrideEndpoint: null,
      overrideNoProxy: null,
      overrideHasCredentials: false,
      effectiveMode: 'system',
      effectiveEndpoint: null,
      effectiveNoProxy: null,
      effectiveHasCredentials: false,
    }
    mocks.invoke.mockResolvedValue(dto)

    const { getAccountProxySettings } = await import('./tauri')
    await expect(getAccountProxySettings('acct-1')).resolves.toEqual(dto)
    expect(mocks.invoke).toHaveBeenCalledWith('get_account_proxy_settings', { accountId: 'acct-1' })
    expectNoCredentialFields(dto)
  })

  it('invokes set_account_proxy_override with account id and input', async () => {
    const dto = {
      accountId: 'acct-2',
      hasOverride: true,
      overrideMode: 'custom',
      overrideEndpoint: 'http://proxy:8080',
      overrideNoProxy: 'localhost',
      overrideHasCredentials: true,
      effectiveMode: 'custom',
      effectiveEndpoint: 'http://proxy:8080',
      effectiveNoProxy: 'localhost',
      effectiveHasCredentials: true,
    }
    mocks.invoke.mockResolvedValue(dto)

    const { setAccountProxyOverride } = await import('./tauri')
    const input = {
      mode: 'custom' as const,
      endpoint: 'http://proxy:8080',
      noProxy: 'localhost',
      username: 'user',
      password: 'pass',
    }

    await expect(setAccountProxyOverride('acct-2', input)).resolves.toEqual(dto)
    expect(mocks.invoke).toHaveBeenCalledWith('set_account_proxy_override', {
      accountId: 'acct-2',
      input,
    })
    expectNoCredentialFields(dto)
  })

  it('invokes clear_account_proxy_override with the account id', async () => {
    const dto = {
      accountId: 'acct-3',
      hasOverride: false,
      overrideMode: null,
      overrideEndpoint: null,
      overrideNoProxy: null,
      overrideHasCredentials: false,
      effectiveMode: 'system',
      effectiveEndpoint: null,
      effectiveNoProxy: null,
      effectiveHasCredentials: false,
    }
    mocks.invoke.mockResolvedValue(dto)

    const { clearAccountProxyOverride } = await import('./tauri')
    await expect(clearAccountProxyOverride('acct-3')).resolves.toEqual(dto)
    expect(mocks.invoke).toHaveBeenCalledWith('clear_account_proxy_override', { accountId: 'acct-3' })
    expectNoCredentialFields(dto)
  })
})
