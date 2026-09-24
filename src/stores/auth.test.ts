import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'
import type {
  AccountsSnapshot,
  ConfirmAddCdpResult,
  CdpIdentityPreview,
  DiscordUser,
  ReconnectCdpResult,
} from '@/api/tauri'
import { useAuthStore } from './auth'

const mocks = vi.hoisted(() => ({
  autoLoginViaCdp: vi.fn(),
  autoAddAccountViaCdp: vi.fn(),
  previewCdpIdentity: vi.fn(),
  confirmAddCdpAccount: vi.fn(),
  reconnectCdpAccount: vi.fn(),
  getProgramRewards: vi.fn(),
  listAccounts: vi.fn(),
  activateAccount: vi.fn(),
  activateOnlineAccount: vi.fn(),
  removeAccount: vi.fn(),
  questsStore: {
    cdpPort: 9223,
    activeCdpPort: 9223,
    cdpAvailable: false,
    gameQuestMode: 'simulate',
    quests: [] as unknown[],
    questsByAccount: {} as Record<string, unknown[]>,
    initCdpMode: vi.fn(),
    getDetectableGames: vi.fn(),
    fetchOrbsBalance: vi.fn(),
    stop: vi.fn(),
    resetForLogout: vi.fn(),
    setActiveAccount: vi.fn(),
  },
}))

vi.mock('@/api/tauri', () => ({
  autoLoginViaCdp: mocks.autoLoginViaCdp,
  autoAddAccountViaCdp: mocks.autoAddAccountViaCdp,
  previewCdpIdentity: mocks.previewCdpIdentity,
  confirmAddCdpAccount: mocks.confirmAddCdpAccount,
  reconnectCdpAccount: mocks.reconnectCdpAccount,
  getProgramRewards: mocks.getProgramRewards,
  listAccounts: mocks.listAccounts,
  activateAccount: mocks.activateAccount,
  activateOnlineAccount: mocks.activateOnlineAccount,
  removeAccount: mocks.removeAccount,
}))

vi.mock('./quests', () => ({
  useQuestsStore: () => mocks.questsStore,
}))

vi.mock('vue-i18n', () => ({
  useI18n: () => ({ t: (key: string) => key }),
}))

vi.mock('@vueuse/core', () => ({
  useNow: () => ({ value: new Date('2026-08-31T00:00:00.000Z') }),
}))

const user: DiscordUser = {
  id: '123',
  username: 'quest-user',
  discriminator: '0',
  avatar: null,
  global_name: 'Quest User',
}

function accountSummary(id: string, isAuthenticated = true, lastCdpPort?: number) {
  return {
    id,
    username: id,
    discriminator: '0',
    isAuthenticated,
    lastCdpPort,
  }
}

function createDeferred<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((res) => {
    resolve = res
  })
  return { promise, resolve }
}

function createBlockedActiveAuthStore() {
  localStorage.setItem('questHelper_accountCdpPorts', JSON.stringify({ B: null }))
  const authStore = useAuthStore()
  authStore.accounts = [accountSummary('B', true, 9444)]
  authStore.activeAccountId = 'B'
  authStore.user = { ...user, id: 'B' }
  authStore.nitroProgramReward = {
    reward_program: 0,
    next_reward_date: '2026-09-17T00:00:00.000Z',
  }
  authStore.programRewardLoading = true
  authStore.programRewardError = 'cached reward state'
  mocks.questsStore.activeCdpPort = 0
  mocks.questsStore.setActiveAccount.mockClear()
  return authStore
}

describe('auth CDP-only login', () => {
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
      clear: () => storage.clear()
    })
    setActivePinia(createPinia())
    vi.clearAllMocks()
    mocks.questsStore.cdpPort = 9223
    mocks.questsStore.cdpAvailable = false
    mocks.questsStore.activeCdpPort = 9223
    mocks.questsStore.gameQuestMode = 'simulate'
    mocks.questsStore.quests = []
    mocks.questsStore.questsByAccount = {}
    mocks.autoLoginViaCdp.mockResolvedValue(user)
    mocks.autoAddAccountViaCdp.mockResolvedValue({ user, alreadyKnown: false })
    mocks.previewCdpIdentity.mockResolvedValue({ port: 9223, user } satisfies CdpIdentityPreview)
    mocks.confirmAddCdpAccount.mockResolvedValue({ status: 'added', user, port: 9223 } satisfies ConfirmAddCdpResult)
    mocks.reconnectCdpAccount.mockResolvedValue({ status: 'reconnected', user, port: 9223 } satisfies ReconnectCdpResult)
    mocks.getProgramRewards.mockResolvedValue([])
    mocks.listAccounts.mockResolvedValue({
      accounts: [accountSummary(user.id)],
      activeAccountId: user.id,
    })
    mocks.activateOnlineAccount.mockResolvedValue(accountSummary(user.id))
    mocks.questsStore.initCdpMode.mockResolvedValue(undefined)
    mocks.questsStore.getDetectableGames.mockResolvedValue(undefined)
    mocks.questsStore.fetchOrbsBalance.mockResolvedValue(undefined)
  })

  afterEach(() => {
    vi.unstubAllGlobals()
  })

  it('selects CDP quest execution after a successful CDP login', async () => {
    const authStore = useAuthStore()

    await expect(authStore.loginViaCdp()).resolves.toBe(true)

    expect(mocks.questsStore.cdpAvailable).toBe(true)
    expect(mocks.questsStore.gameQuestMode).toBe('cdp')
    expect(mocks.questsStore.initCdpMode).toHaveBeenCalledOnce()
  })

  it('never exposes a raw token or manual/detected-account surface', async () => {
    const authStore = useAuthStore()

    expect(authStore).not.toHaveProperty('token')
    expect(authStore).not.toHaveProperty('detectedAccounts')
    expect(authStore).not.toHaveProperty('loginWithToken')
    expect(authStore).not.toHaveProperty('tryAutoDetect')

    await authStore.loginViaCdp()

    expect(authStore.user).toEqual(user)
    expect(authStore).not.toHaveProperty('token')
  })

  it('clears the user and CDP login surface on logout', async () => {
    const authStore = useAuthStore()
    await authStore.loginViaCdp()

    await authStore.logout()

    expect(authStore.user).toBeNull()
    expect(mocks.questsStore.resetForLogout).toHaveBeenCalledOnce()
  })

  it('switching to an offline account clears the authenticated projection', async () => {
    const authStore = useAuthStore()
    await authStore.loginViaCdp()
    expect(authStore.user).toEqual(user)

    mocks.activateAccount.mockResolvedValue({ id: '999', username: 'offline', isAuthenticated: false })
    mocks.listAccounts.mockResolvedValue({
      accounts: [accountSummary('999', false)],
      activeAccountId: '999',
    })
    await authStore.activateAccount('999')

    expect(authStore.activeAccountId).toBe('999')
    expect(authStore.isActiveAccountAuthenticated).toBe(false)
    expect(authStore.user).toBeNull()
    // The account's resolved port is passed alongside the id.
    expect(mocks.questsStore.setActiveAccount).toHaveBeenCalledWith('999', 9223)
  })

  it('removing the authenticated active account clears its projection', async () => {
    const authStore = useAuthStore()
    await authStore.loginViaCdp()

    mocks.removeAccount.mockResolvedValue({ accounts: [], activeAccountId: undefined })
    mocks.listAccounts.mockResolvedValue({ accounts: [], activeAccountId: undefined })
    await authStore.removeAccount('123')

    expect(authStore.user).toBeNull()
    expect(authStore.activeAccountId).toBeNull()
    expect(mocks.questsStore.resetForLogout).toHaveBeenCalled()
  })

  it('frees and persists a removed account port so it is reusable after store recreation', async () => {
    const authStore = useAuthStore()
    const remaining = accountSummary('B', true, 9224)
    authStore.accounts = [
      accountSummary('A'),
      remaining,
    ]
    expect(authStore.setAccountPort('A', 9223)).toBe(true)
    expect(authStore.setAccountPort('B', 9224)).toBe(true)
    mocks.removeAccount.mockResolvedValue({
      accounts: [remaining],
      activeAccountId: 'B',
    })
    mocks.listAccounts.mockResolvedValue({
      accounts: [remaining],
      activeAccountId: 'B',
    })

    await authStore.removeAccount('A')

    expect(authStore.accountPorts).toEqual({ B: 9224 })
    expect(JSON.parse(localStorage.getItem('questHelper_accountCdpPorts') ?? '{}')).toEqual({ B: 9224 })

    setActivePinia(createPinia())
    const reloaded = useAuthStore()
    await reloaded.loadAccounts()
    expect(reloaded.accountPorts).toEqual({ B: 9224 })
    expect(reloaded.suggestAccountPort()).toBe(9223)
  })

  it('keeps a saved account port when backend removal fails', async () => {
    const authStore = useAuthStore()
    authStore.accounts = [accountSummary('A')]
    expect(authStore.setAccountPort('A', 9223)).toBe(true)
    const storedBefore = localStorage.getItem('questHelper_accountCdpPorts')
    mocks.removeAccount.mockRejectedValue(new Error('remove failed'))

    await expect(authStore.removeAccount('A')).rejects.toThrow('remove failed')

    expect(authStore.accountPorts).toEqual({ A: 9223 })
    expect(localStorage.getItem('questHelper_accountCdpPorts')).toBe(storedBefore)
  })

  it('hydrates the projection when switching to an authenticated account', async () => {
    const authStore = useAuthStore()
    mocks.activateAccount.mockResolvedValue({
      id: '555',
      username: 'other',
      discriminator: '1',
      globalName: 'Other Display',
      isAuthenticated: true,
    })
    mocks.listAccounts.mockResolvedValue({
      accounts: [{
        id: '555',
        username: 'other',
        discriminator: '1',
        globalName: 'Other Display',
        isAuthenticated: true,
      }],
      activeAccountId: '555',
    })

    await authStore.activateAccount('555')

    expect(authStore.isActiveAccountAuthenticated).toBe(true)
    expect(authStore.user).toEqual({
      id: '555',
      username: 'other',
      discriminator: '1',
      avatar: null,
      global_name: 'Other Display',
    })
  })

  it('keeps A unchanged when online-only activation rejects stale-online B', async () => {
    const authStore = useAuthStore()
    authStore.accounts = [
      accountSummary('A', true, 9223),
      accountSummary('B', true, 9224),
    ]
    authStore.activeAccountId = 'A'
    authStore.user = { ...user, id: 'A' }
    expect(authStore.setAccountPort('A', 9223)).toBe(true)
    expect(authStore.setAccountPort('B', 9224)).toBe(true)
    mocks.questsStore.activeCdpPort = 9223
    mocks.questsStore.setActiveAccount.mockClear()
    mocks.activateOnlineAccount.mockRejectedValue(new Error('account_offline'))
    const accountsBefore = [...authStore.accounts]
    const portsBefore = { ...authStore.accountPorts }
    const userBefore = { ...authStore.user! }
    const storageBefore = localStorage.getItem('questHelper_accountCdpPorts')

    await expect(authStore.switchOnlineAccount('B')).rejects.toThrow('account_offline')

    expect(mocks.activateOnlineAccount).toHaveBeenCalledOnce()
    expect(mocks.activateOnlineAccount).toHaveBeenCalledWith('B')
    expect(mocks.activateAccount).not.toHaveBeenCalled()
    expect(mocks.listAccounts).not.toHaveBeenCalled()
    expect(authStore.accounts).toEqual(accountsBefore)
    expect(authStore.accountPorts).toEqual(portsBefore)
    expect(localStorage.getItem('questHelper_accountCdpPorts')).toBe(storageBefore)
    expect(authStore.activeAccountId).toBe('A')
    expect(authStore.user).toEqual(userBefore)
    expect(mocks.questsStore.activeCdpPort).toBe(9223)
    expect(mocks.questsStore.setActiveAccount).not.toHaveBeenCalled()
  })

  it('switches online B through online-only IPC and reconciles its active port', async () => {
    const authStore = useAuthStore()
    const profileA = accountSummary('A', true, 9223)
    const profileB = accountSummary('B', true, 9224)
    authStore.accounts = [profileA, profileB]
    authStore.activeAccountId = 'A'
    authStore.user = { ...user, id: 'A' }
    expect(authStore.setAccountPort('A', 9223)).toBe(true)
    expect(authStore.setAccountPort('B', 9224)).toBe(true)
    mocks.activateOnlineAccount.mockResolvedValue(profileB)
    mocks.listAccounts.mockResolvedValue({
      accounts: [profileA, profileB],
      activeAccountId: 'B',
    })

    await expect(authStore.switchOnlineAccount('B')).resolves.toEqual(profileB)

    expect(mocks.activateOnlineAccount).toHaveBeenCalledWith('B')
    expect(mocks.activateAccount).not.toHaveBeenCalled()
    expect(authStore.activeAccountId).toBe('B')
    expect(authStore.user).toMatchObject({ id: 'B' })
    expect(authStore.portForAccount('B')).toBe(9224)
    expect(mocks.questsStore.setActiveAccount).toHaveBeenLastCalledWith('B', 9224)
  })

  it('uses Discord program reward timestamps for the Orbs countdown', async () => {
    const authStore = useAuthStore()
    authStore.user = user
    mocks.getProgramRewards.mockResolvedValue([
      {
        // Discord's live enum is NITRO=0 (XBOX=1).
        reward_program: 0,
        next_reward_date: '2026-09-17T00:00:00.000Z',
      },
    ])

    await authStore.fetchNitroProgramReward()

    expect(authStore.nextOrbsClaim).toEqual({ value: 17, unit: 'days' })
  })

  describe('per-account CDP ports', () => {
    it('resolves explicit > accountPorts > lastCdpPort > global', async () => {
      const authStore = useAuthStore()
      authStore.accounts = [
        { id: 'acct-a', username: 'a', isAuthenticated: true, lastCdpPort: 9555 },
      ]

      // Profile `lastCdpPort` fallback.
      expect(authStore.portForAccount('acct-a')).toBe(9555)
      // A saved account port beats the profile metadata.
      authStore.setAccountPort('acct-a', 9666)
      expect(authStore.portForAccount('acct-a')).toBe(9666)
      // Unknown account falls back to the global default.
      expect(authStore.portForAccount('unknown')).toBe(9223)

      mocks.listAccounts.mockResolvedValue({
        accounts: [
          { id: 'acct-a', username: 'a', isAuthenticated: true, lastCdpPort: 9555 },
          accountSummary(user.id),
        ],
        activeAccountId: user.id,
      })

      // Explicit port wins for a login attempt.
      await authStore.loginViaCdp(undefined, { port: 9777 })
      expect(mocks.autoLoginViaCdp).toHaveBeenCalledWith(9777, undefined)

      // No explicit port: the active account's saved port is used.
      authStore.activeAccountId = 'acct-a'
      await authStore.loginViaCdp()
      expect(mocks.autoLoginViaCdp).toHaveBeenLastCalledWith(9666, undefined)
    })

    it('suggests the next port after an explicit account assignment', () => {
      const authStore = useAuthStore()
      authStore.accounts = [{ id: 'A', username: 'a', isAuthenticated: true }]

      expect(authStore.setAccountPort('A', 9223)).toBe(true)
      expect(authStore.suggestAccountPort()).toBe(9224)
      expect(authStore.suggestAccountPort('A')).toBe(9223)
    })

    it('suggests the next free port after multiple explicit assignments', () => {
      const authStore = useAuthStore()
      authStore.accounts = [
        { id: 'A', username: 'a', isAuthenticated: true },
        { id: 'B', username: 'b', isAuthenticated: true, lastCdpPort: 9224 },
      ]

      expect(authStore.setAccountPort('A', 9223)).toBe(true)
      expect(authStore.setAccountPort('B', 9224)).toBe(true)
      expect(authStore.suggestAccountPort()).toBe(9225)
    })

    it('reserves profile lastCdpPort values when there is no local override', () => {
      const authStore = useAuthStore()
      authStore.accounts = [
        { id: 'A', username: 'a', isAuthenticated: false, lastCdpPort: 9223 },
        { id: 'B', username: 'b', isAuthenticated: false, lastCdpPort: 9224 },
      ]

      expect(authStore.suggestAccountPort()).toBe(9225)
      expect(authStore.isAccountPortAvailable(9223)).toBe(false)
    })

    it('blocks every saved profile sharing the implicit global fallback', async () => {
      const authStore = useAuthStore()
      mocks.listAccounts.mockResolvedValue({
        accounts: [accountSummary('A', false), accountSummary('B', false)],
        activeAccountId: 'B',
      })

      await authStore.loadAccounts()

      expect(authStore.accountPorts).toEqual({ A: null, B: null })
      expect(authStore.portForAccount('A')).toBe(0)
      expect(authStore.portForAccount('B')).toBe(0)
      expect(mocks.questsStore.setActiveAccount).toHaveBeenLastCalledWith('B', 0)
      expect(JSON.parse(localStorage.getItem('questHelper_accountCdpPorts') ?? '{}')).toEqual({
        A: null,
        B: null,
      })
    })

    it('keeps an explicit owner and blocks the profile using its implicit fallback', async () => {
      const authStore = useAuthStore()
      expect(authStore.setAccountPort('explicit', 9223)).toBe(true)
      mocks.listAccounts.mockResolvedValue({
        accounts: [accountSummary('explicit', false), accountSummary('implicit', false)],
        activeAccountId: 'implicit',
      })

      await authStore.loadAccounts()

      expect(authStore.accountPorts).toEqual({ explicit: 9223, implicit: null })
      expect(authStore.portForAccount('explicit')).toBe(9223)
      expect(authStore.portForAccount('implicit')).toBe(0)
      expect(mocks.questsStore.setActiveAccount).toHaveBeenLastCalledWith('implicit', 0)
    })

    it('fails closed when legacy explicit assignments already collide', async () => {
      localStorage.setItem('questHelper_accountCdpPorts', JSON.stringify({ A: 9223, B: 9223 }))
      const authStore = useAuthStore()
      mocks.listAccounts.mockResolvedValue({
        accounts: [accountSummary('A', false), accountSummary('B', false)],
        activeAccountId: 'A',
      })

      await authStore.loadAccounts()

      expect(authStore.accountPorts).toEqual({ A: null, B: null })
      expect(authStore.portForAccount('A')).toBe(0)
      expect(authStore.portForAccount('B')).toBe(0)
    })

    it('prunes an orphaned persisted override after an accepted snapshot and frees it after reload', async () => {
      localStorage.setItem('questHelper_accountCdpPorts', JSON.stringify({ removed: 9224 }))
      const authStore = useAuthStore()
      mocks.listAccounts.mockResolvedValue({
        accounts: [accountSummary('kept', false, 9223)],
        activeAccountId: 'kept',
      })

      await authStore.loadAccounts()

      expect(authStore.accountPorts).toEqual({})
      expect(localStorage.getItem('questHelper_accountCdpPorts')).toBe('{}')
      expect(authStore.suggestAccountPort()).toBe(9224)

      setActivePinia(createPinia())
      const reloaded = useAuthStore()
      mocks.listAccounts.mockResolvedValue({
        accounts: [accountSummary('kept', false, 9223)],
        activeAccountId: 'kept',
      })
      await reloaded.loadAccounts()
      expect(reloaded.accountPorts).toEqual({})
      expect(reloaded.suggestAccountPort()).toBe(9224)
    })

    it('does not prune ports from stale or rejected account-list snapshots', async () => {
      localStorage.setItem('questHelper_accountCdpPorts', JSON.stringify({ A: 9224 }))
      const authStore = useAuthStore()
      const staleEmpty = createDeferred<AccountsSnapshot>()
      const currentSnapshot = createDeferred<AccountsSnapshot>()
      mocks.listAccounts
        .mockReturnValueOnce(staleEmpty.promise)
        .mockReturnValueOnce(currentSnapshot.promise)

      const staleLoad = authStore.loadAccounts()
      const currentLoad = authStore.loadAccounts()
      currentSnapshot.resolve({
        accounts: [accountSummary('A', false, 9224)],
        activeAccountId: 'A',
      })
      await currentLoad
      const afterAcceptedSnapshot = localStorage.getItem('questHelper_accountCdpPorts')
      staleEmpty.resolve({ accounts: [], activeAccountId: undefined })
      await staleLoad

      expect(authStore.accountPorts).toEqual({ A: 9224 })
      expect(localStorage.getItem('questHelper_accountCdpPorts')).toBe(afterAcceptedSnapshot)

      mocks.listAccounts.mockRejectedValueOnce(new Error('list failed'))
      await expect(authStore.loadAccounts()).rejects.toThrow('list failed')
      expect(authStore.accountPorts).toEqual({ A: 9224 })
      expect(localStorage.getItem('questHelper_accountCdpPorts')).toBe(afterAcceptedSnapshot)
    })

    it('prunes orphaned persisted overrides only after an accepted authoritative snapshot', async () => {
      localStorage.setItem('questHelper_accountCdpPorts', JSON.stringify({ removed: 9224 }))
      const authStore = useAuthStore()
      const staleEmpty = createDeferred<AccountsSnapshot>()
      const accepted = createDeferred<AccountsSnapshot>()
      mocks.listAccounts
        .mockReturnValueOnce(staleEmpty.promise)
        .mockReturnValueOnce(accepted.promise)

      const staleLoad = authStore.loadAccounts()
      const currentLoad = authStore.loadAccounts()
      accepted.resolve({
        accounts: [accountSummary('kept', false, 9223)],
        activeAccountId: 'kept',
      })
      await currentLoad
      expect(authStore.accountPorts).toEqual({})
      expect(localStorage.getItem('questHelper_accountCdpPorts')).toBe('{}')

      // The older empty result is stale and cannot perform additional pruning.
      staleEmpty.resolve({ accounts: [], activeAccountId: undefined })
      await staleLoad
      expect(localStorage.getItem('questHelper_accountCdpPorts')).toBe('{}')

      mocks.listAccounts.mockRejectedValueOnce(new Error('list failed'))
      await expect(authStore.loadAccounts()).rejects.toThrow('list failed')
      expect(localStorage.getItem('questHelper_accountCdpPorts')).toBe('{}')

      setActivePinia(createPinia())
      const reloaded = useAuthStore()
      mocks.listAccounts.mockResolvedValue({
        accounts: [accountSummary('kept', false, 9223)],
        activeAccountId: 'kept',
      })
      await reloaded.loadAccounts()
      expect(reloaded.suggestAccountPort()).toBe(9224)
    })

    it('lets the explicit local override replace that same profile port', () => {
      const authStore = useAuthStore()
      authStore.accounts = [
        { id: 'A', username: 'a', isAuthenticated: false, lastCdpPort: 9223 },
      ]

      expect(authStore.setAccountPort('A', 9224)).toBe(true)
      // The explicit map takes priority; the stale profile port is not reserved.
      expect(authStore.suggestAccountPort()).toBe(9223)
    })

    it('reserves the global fallback for saved unassigned profiles only', () => {
      const authStore = useAuthStore()
      authStore.accounts = [{ id: 'A', username: 'a', isAuthenticated: false }]

      expect(authStore.portForAccount('A')).toBe(9223)
      expect(authStore.isAccountPortAvailable(9223)).toBe(false)
      expect(authStore.suggestAccountPort()).toBe(9224)

      authStore.accounts = []
      expect(authStore.isAccountPortAvailable(9223)).toBe(true)
      expect(authStore.suggestAccountPort()).toBe(9223)
    })

    it('refuses normal login for a blocked active account without a port option', async () => {
      const authStore = createBlockedActiveAuthStore()
      const accountsBefore = [...authStore.accounts]
      const portsBefore = { ...authStore.accountPorts }
      const userBefore = { ...authStore.user! }
      const storageBefore = localStorage.getItem('questHelper_accountCdpPorts')

      await expect(authStore.loginViaCdp()).resolves.toBe(false)

      expect(mocks.autoLoginViaCdp).not.toHaveBeenCalled()
      expect(authStore.error).toBe('auth.account_cdp_port_unassigned')
      expect(authStore.loading).toBe(false)
      expect(authStore.accounts).toEqual(accountsBefore)
      expect(authStore.activeAccountId).toBe('B')
      expect(authStore.user).toEqual(userBefore)
      expect(authStore.accountPorts).toEqual(portsBefore)
      expect(authStore.portForAccount('B')).toBe(0)
      expect(localStorage.getItem('questHelper_accountCdpPorts')).toBe(storageBefore)
      expect(authStore.nitroProgramReward).toEqual({
        reward_program: 0,
        next_reward_date: '2026-09-17T00:00:00.000Z',
      })
      expect(authStore.programRewardLoading).toBe(true)
      expect(authStore.programRewardError).toBe('cached reward state')
      expect(mocks.questsStore.activeCdpPort).toBe(0)
      expect(mocks.questsStore.setActiveAccount).not.toHaveBeenCalled()
    })

    it('does not let an explicit free port bypass a blocked active account', async () => {
      const authStore = createBlockedActiveAuthStore()
      const accountsBefore = [...authStore.accounts]
      const portsBefore = { ...authStore.accountPorts }
      const userBefore = { ...authStore.user! }
      const storageBefore = localStorage.getItem('questHelper_accountCdpPorts')

      await expect(authStore.loginViaCdp(undefined, { port: 9224 })).resolves.toBe(false)

      expect(mocks.autoLoginViaCdp).not.toHaveBeenCalled()
      expect(authStore.error).toBe('auth.account_cdp_port_unassigned')
      expect(authStore.loading).toBe(false)
      expect(authStore.accounts).toEqual(accountsBefore)
      expect(authStore.activeAccountId).toBe('B')
      expect(authStore.user).toEqual(userBefore)
      expect(authStore.accountPorts).toEqual(portsBefore)
      expect(localStorage.getItem('questHelper_accountCdpPorts')).toBe(storageBefore)
      expect(authStore.nitroProgramReward).toEqual({
        reward_program: 0,
        next_reward_date: '2026-09-17T00:00:00.000Z',
      })
      expect(authStore.programRewardLoading).toBe(true)
      expect(authStore.programRewardError).toBe('cached reward state')
      expect(mocks.questsStore.activeCdpPort).toBe(0)
      expect(mocks.questsStore.setActiveAccount).not.toHaveBeenCalled()
    })

    it('allows normal login after the blocked account is manually assigned a free port', async () => {
      const authStore = createBlockedActiveAuthStore()
      expect(authStore.setAccountPort('B', 9224)).toBe(true)
      mocks.autoLoginViaCdp.mockResolvedValue({ ...user, id: 'B' })
      mocks.listAccounts.mockResolvedValue({
        accounts: [accountSummary('B', true, 9224)],
        activeAccountId: 'B',
      })

      await expect(authStore.loginViaCdp()).resolves.toBe(true)

      expect(mocks.autoLoginViaCdp).toHaveBeenCalledWith(9224, undefined)
      expect(authStore.error).toBeNull()
      expect(authStore.activeAccountId).toBe('B')
      expect(authStore.accountPorts).toEqual({ B: 9224 })
      expect(authStore.portForAccount('B')).toBe(9224)
    })

    it('keeps Add-account independent of a blocked active profile', async () => {
      const authStore = createBlockedActiveAuthStore()
      mocks.autoAddAccountViaCdp.mockResolvedValue({
        user: { ...user, id: 'C' },
        alreadyKnown: false,
      })
      mocks.listAccounts.mockResolvedValue({
        accounts: [accountSummary('B', true, 9444), accountSummary('C', true, 9224)],
        activeAccountId: 'C',
      })

      await expect(authStore.addAccountViaCdp(undefined, { port: 9224 })).resolves.toBe(true)

      expect(mocks.autoAddAccountViaCdp).toHaveBeenCalledWith(9224, undefined)
      expect(mocks.autoLoginViaCdp).not.toHaveBeenCalled()
      expect(authStore.activeAccountId).toBe('C')
      expect(authStore.accountPorts).toEqual({ B: null, C: 9224 })
      expect(authStore.portForAccount('B')).toBe(0)
      expect(authStore.portForAccount('C')).toBe(9224)
    })

    it('fails closed with port zero when the upward range is exhausted', () => {
      const authStore = useAuthStore()
      mocks.questsStore.cdpPort = 65535
      authStore.accounts = [{ id: 'A', username: 'a', isAuthenticated: true }]
      expect(authStore.setAccountPort('A', 65535)).toBe(true)

      expect(authStore.suggestAccountPort()).toBe(0)
      expect(authStore.isAccountPortAvailable(0)).toBe(false)
      expect(authStore.setAccountPort('B', 0)).toBe(false)
    })

    it('rejects an explicit port collision without changing ports, storage, or active CDP state', () => {
      const authStore = useAuthStore()
      authStore.accounts = [
        { id: 'A', username: 'a', isAuthenticated: true },
        { id: 'B', username: 'b', isAuthenticated: true, lastCdpPort: 9224 },
      ]
      authStore.activeAccountId = 'B'
      expect(authStore.setAccountPort('A', 9223)).toBe(true)
      expect(authStore.setAccountPort('B', 9224)).toBe(true)
      mocks.questsStore.setActiveAccount.mockClear()
      mocks.questsStore.activeCdpPort = 9224
      const portsBefore = { ...authStore.accountPorts }
      const storageBefore = localStorage.getItem('questHelper_accountCdpPorts')

      expect(authStore.isAccountPortAvailable(9223, 'B')).toBe(false)
      expect(authStore.setAccountPort('B', 9223)).toBe(false)

      expect(authStore.accountPorts).toEqual(portsBefore)
      expect(localStorage.getItem('questHelper_accountCdpPorts')).toBe(storageBefore)
      expect(mocks.questsStore.activeCdpPort).toBe(9224)
      expect(mocks.questsStore.setActiveAccount).not.toHaveBeenCalled()
    })

    it('allows an account to keep or change its own non-conflicting port', () => {
      const authStore = useAuthStore()
      authStore.accounts = [
        { id: 'A', username: 'a', isAuthenticated: true },
        { id: 'B', username: 'b', isAuthenticated: true, lastCdpPort: 9224 },
      ]
      authStore.activeAccountId = 'B'
      expect(authStore.setAccountPort('A', 9223)).toBe(true)
      expect(authStore.setAccountPort('B', 9224)).toBe(true)
      mocks.questsStore.setActiveAccount.mockClear()

      expect(authStore.isAccountPortAvailable(9224, 'B')).toBe(true)
      expect(authStore.setAccountPort('B', 9224)).toBe(true)
      expect(authStore.setAccountPort('B', 9225)).toBe(true)
      expect(authStore.accountPorts).toEqual({ A: 9223, B: 9225 })
      expect(mocks.questsStore.setActiveAccount).toHaveBeenLastCalledWith('B', 9225)
    })

    it('rejects NaN, fractional, and out-of-range account ports', () => {
      const authStore = useAuthStore()
      authStore.activeAccountId = 'A'
      const portsBefore = { ...authStore.accountPorts }
      const storageBefore = localStorage.getItem('questHelper_accountCdpPorts')

      for (const port of [Number.NaN, 9223.5, 1023, 65536, 0]) {
        expect(authStore.isAccountPortAvailable(port, 'A')).toBe(false)
        expect(authStore.setAccountPort('A', port)).toBe(false)
      }

      expect(authStore.accountPorts).toEqual(portsBefore)
      expect(localStorage.getItem('questHelper_accountCdpPorts')).toBe(storageBefore)
      expect(mocks.questsStore.setActiveAccount).not.toHaveBeenCalled()
    })

    it('uses each active account own port for CDP login', async () => {
      const authStore = useAuthStore()
      authStore.accounts = [
        { id: '111', username: 'a', isAuthenticated: true },
        { id: '222', username: 'b', isAuthenticated: true, lastCdpPort: 9333 },
      ]
      authStore.setAccountPort('111', 9223)
      authStore.setAccountPort('222', 9333)
      mocks.listAccounts
        .mockResolvedValueOnce({
          accounts: [
            { id: '111', username: 'a', isAuthenticated: true },
            { id: '222', username: 'b', isAuthenticated: true, lastCdpPort: 9333 },
          ],
          activeAccountId: '111',
        })
        .mockResolvedValueOnce({
          accounts: [
            { id: '111', username: 'a', isAuthenticated: true },
            { id: '222', username: 'b', isAuthenticated: true, lastCdpPort: 9333 },
          ],
          activeAccountId: '222',
        })
      mocks.autoLoginViaCdp
        .mockResolvedValueOnce({ ...user, id: '111' })
        .mockResolvedValueOnce({ ...user, id: '222' })

      authStore.activeAccountId = '111'
      await authStore.loginViaCdp()
      expect(mocks.autoLoginViaCdp).toHaveBeenLastCalledWith(9223, undefined)

      authStore.activeAccountId = '222'
      await authStore.loginViaCdp()
      expect(mocks.autoLoginViaCdp).toHaveBeenLastCalledWith(9333, undefined)
    })

    it('an add-account duplicate leaves the active account, user, ports, and list untouched', async () => {
      const authStore = useAuthStore()
      const active = { id: 'B', username: 'b', isAuthenticated: true }
      const existing = { id: '123', username: 'quest-user', isAuthenticated: true }
      authStore.accounts = [active, existing]
      authStore.user = {
        id: 'B',
        username: 'b',
        discriminator: '0',
        avatar: null,
        global_name: 'B',
      }
      authStore.activeAccountId = 'B'
      // Pre-existing saved per-account ports: active B -> 9224, duplicate 123 -> 9444.
      authStore.setAccountPort('B', 9224)
      authStore.setAccountPort('123', 9444)

      const cachedReward = { reward_program: 0, next_reward_date: '2026-09-17T00:00:00.000Z' }
      authStore.nitroProgramReward = cachedReward
      authStore.programRewardLoading = true
      authStore.programRewardError = 'cached reward state'
      mocks.questsStore.activeCdpPort = 9224
      mocks.questsStore.quests = [{ id: 'cached-quest' }]
      mocks.questsStore.questsByAccount = { B: [{ id: 'cached-account-quest' }] }

      // The captured user IS in the local list, but only the backend flag decides.
      mocks.autoAddAccountViaCdp.mockResolvedValue({
        user: { ...user, id: '123' },
        alreadyKnown: true,
      })
      const accountsSnapshot: AccountsSnapshot = { accounts: [active, existing], activeAccountId: 'B' }
      mocks.listAccounts.mockResolvedValue(accountsSnapshot)
      // Setup above activated a port; only record calls from the add attempt.
      mocks.questsStore.setActiveAccount.mockClear()

      await expect(authStore.addAccountViaCdp(undefined, { port: 9223 })).resolves.toBe(true)

      expect(authStore.duplicateLoginAccountId).toBe('123')
      expect(authStore.error).toBeNull()
      // The prior active account/projection is preserved.
      expect(authStore.activeAccountId).toBe('B')
      expect(authStore.user?.id).toBe('B')
      expect(authStore.accounts).toEqual([active, existing])
      // Neither the duplicate's nor the active account's saved port was overwritten.
      expect(authStore.portForAccount('B')).toBe(9224)
      expect(authStore.portForAccount('123')).toBe(9444)
      expect(authStore.accountPorts).toEqual({ B: 9224, '123': 9444 })
      expect(authStore.nitroProgramReward).toEqual(cachedReward)
      expect(authStore.programRewardLoading).toBe(true)
      expect(authStore.programRewardError).toBe('cached reward state')
      expect(mocks.autoAddAccountViaCdp).toHaveBeenCalledWith(9223, undefined)
      // The account list was not reloaded/replaced and no activation happened.
      expect(mocks.listAccounts).not.toHaveBeenCalled()
      expect(mocks.questsStore.setActiveAccount).not.toHaveBeenCalled()
      // Only the duplicate flag changed: quest execution state was not touched.
      expect(mocks.questsStore.activeCdpPort).toBe(9224)
      expect(mocks.questsStore.cdpAvailable).toBe(false)
      expect(mocks.questsStore.gameQuestMode).toBe('simulate')
      expect(mocks.questsStore.quests).toEqual([{ id: 'cached-quest' }])
      expect(mocks.questsStore.questsByAccount).toEqual({
        B: [{ id: 'cached-account-quest' }],
      })
    })

    it('an add-account new capture activates the account and stores its port', async () => {
      const authStore = useAuthStore()
      authStore.accounts = [{ id: 'A', username: 'a', isAuthenticated: true }]
      authStore.activeAccountId = 'A'
      mocks.autoAddAccountViaCdp.mockResolvedValue({
        user: { ...user, id: 'B' },
        alreadyKnown: false,
      })
      mocks.listAccounts.mockResolvedValue({
        accounts: [{ id: 'B', username: 'quest-user', isAuthenticated: true }],
        activeAccountId: 'B',
      })

      await expect(authStore.addAccountViaCdp(undefined, { port: 9224 })).resolves.toBe(true)

      expect(authStore.duplicateLoginAccountId).toBeNull()
      expect(authStore.user?.id).toBe('B')
      expect(authStore.activeAccountId).toBe('B')
      expect(authStore.accountPorts['B']).toBe(9224)
      expect(JSON.parse(localStorage.getItem('questHelper_accountCdpPorts') ?? '{}')).toMatchObject({
        B: 9224,
      })
      expect(mocks.autoAddAccountViaCdp).toHaveBeenCalledWith(9224, undefined)
      expect(mocks.questsStore.setActiveAccount).toHaveBeenCalledWith('B', 9224)
      expect(mocks.questsStore.cdpAvailable).toBe(true)
      expect(mocks.questsStore.gameQuestMode).toBe('cdp')
    })

    it('previews a client identity without changing account or port state', async () => {
      const authStore = useAuthStore()
      authStore.accounts = [
        accountSummary('A', true, 9223),
        accountSummary('B', false, 9224),
      ]
      authStore.activeAccountId = 'A'
      authStore.user = { ...user, id: 'A' }
      expect(authStore.setAccountPort('A', 9223)).toBe(true)
      expect(authStore.setAccountPort('B', 9224)).toBe(true)
      mocks.questsStore.setActiveAccount.mockClear()
      const accountsBefore = [...authStore.accounts]
      const portsBefore = { ...authStore.accountPorts }
      const userBefore = { ...authStore.user! }
      const storageBefore = localStorage.getItem('questHelper_accountCdpPorts')
      const preview = { port: 9225, user: { ...user, id: 'C' } }
      mocks.previewCdpIdentity.mockResolvedValue(preview)

      await expect(authStore.previewClientAccount(9225)).resolves.toBe(preview)

      expect(mocks.previewCdpIdentity).toHaveBeenCalledWith(9225)
      expect(authStore.accounts).toEqual(accountsBefore)
      expect(authStore.accountPorts).toEqual(portsBefore)
      expect(authStore.activeAccountId).toBe('A')
      expect(authStore.user).toEqual(userBefore)
      expect(localStorage.getItem('questHelper_accountCdpPorts')).toBe(storageBefore)
      expect(mocks.questsStore.setActiveAccount).not.toHaveBeenCalled()
      expect(mocks.listAccounts).not.toHaveBeenCalled()
    })

    it('returns identityChanged from confirmed Add without changing active state', async () => {
      const authStore = useAuthStore()
      authStore.accounts = [accountSummary('A', true, 9223), accountSummary('B', false, 9224)]
      authStore.activeAccountId = 'A'
      authStore.user = { ...user, id: 'A' }
      expect(authStore.setAccountPort('A', 9223)).toBe(true)
      expect(authStore.setAccountPort('B', 9224)).toBe(true)
      mocks.questsStore.setActiveAccount.mockClear()
      mocks.confirmAddCdpAccount.mockResolvedValue({
        status: 'identityChanged',
        user: { ...user, id: 'C' },
        port: 9225,
      })
      const portsBefore = { ...authStore.accountPorts }

      await expect(authStore.confirmAddClientAccount(9225, 'B')).resolves.toMatchObject({
        status: 'identityChanged',
        user: { id: 'C' },
      })

      expect(mocks.confirmAddCdpAccount).toHaveBeenCalledWith(9225, 'B', undefined)
      expect(mocks.listAccounts).not.toHaveBeenCalled()
      expect(authStore.accountPorts).toEqual(portsBefore)
      expect(authStore.activeAccountId).toBe('A')
      expect(authStore.user?.id).toBe('A')
      expect(mocks.questsStore.setActiveAccount).not.toHaveBeenCalled()
    })

    it('keeps AlreadySaved Add informational without duplicating the account', async () => {
      const authStore = useAuthStore()
      authStore.accounts = [accountSummary('A', true, 9223), accountSummary('B', false, 9224)]
      authStore.activeAccountId = 'A'
      authStore.user = { ...user, id: 'A' }
      authStore.accountPorts = { A: 9223, B: 9224 }
      mocks.confirmAddCdpAccount.mockResolvedValue({
        status: 'alreadySaved',
        user: { ...user, id: 'B' },
        port: 9225,
      })
      mocks.questsStore.setActiveAccount.mockClear()

      const result = await authStore.confirmAddClientAccount(9225, 'B')

      expect(result.status).toBe('alreadySaved')
      expect(authStore.accounts.map(account => account.id)).toEqual(['A', 'B'])
      expect(authStore.accountPorts).toEqual({ A: 9223, B: 9224 })
      expect(authStore.activeAccountId).toBe('A')
      expect(authStore.user?.id).toBe('A')
      expect(mocks.listAccounts).not.toHaveBeenCalled()
      expect(mocks.questsStore.setActiveAccount).not.toHaveBeenCalled()
    })

    it('reconciles and saves the port only after a verified new Add', async () => {
      const authStore = useAuthStore()
      authStore.accounts = [accountSummary('A', true, 9223)]
      authStore.activeAccountId = 'A'
      authStore.user = { ...user, id: 'A' }
      expect(authStore.setAccountPort('A', 9223)).toBe(true)
      mocks.confirmAddCdpAccount.mockResolvedValue({
        status: 'added',
        user: { ...user, id: 'B' },
        port: 9224,
      })
      mocks.listAccounts.mockResolvedValue({
        accounts: [accountSummary('A', true, 9223), accountSummary('B', true, 9224)],
        activeAccountId: 'B',
      })

      await expect(authStore.confirmAddClientAccount(9224, 'B')).resolves.toMatchObject({
        status: 'added',
        user: { id: 'B' },
      })

      expect(authStore.accounts.map(account => account.id)).toEqual(['A', 'B'])
      expect(authStore.activeAccountId).toBe('B')
      expect(authStore.user?.id).toBe('B')
      expect(authStore.accountPorts).toEqual({ A: 9223, B: 9224 })
      expect(mocks.listAccounts).toHaveBeenCalledOnce()
      expect(mocks.questsStore.setActiveAccount).toHaveBeenLastCalledWith('B', 9224)
    })

    it('leaves B unchanged when reconnecting its saved profile yields identity A', async () => {
      localStorage.setItem('questHelper_accountCdpPorts', JSON.stringify({ A: 9223, B: null }))
      const authStore = useAuthStore()
      authStore.accounts = [accountSummary('A', true, 9223), accountSummary('B', false, 9224)]
      authStore.activeAccountId = 'A'
      authStore.user = { ...user, id: 'A' }
      mocks.reconnectCdpAccount.mockResolvedValue({
        status: 'identityChanged',
        user: { ...user, id: 'A' },
        port: 9224,
      })
      mocks.questsStore.activeCdpPort = 9223
      mocks.questsStore.setActiveAccount.mockClear()
      const accountsBefore = [...authStore.accounts]
      const portsBefore = { ...authStore.accountPorts }
      const userBefore = { ...authStore.user! }

      await expect(authStore.reconnectSavedAccount('B', 9224)).resolves.toMatchObject({
        status: 'identityChanged',
        user: { id: 'A' },
      })

      expect(mocks.reconnectCdpAccount).toHaveBeenCalledWith('B', 9224, undefined)
      expect(mocks.listAccounts).not.toHaveBeenCalled()
      expect(authStore.accounts).toEqual(accountsBefore)
      expect(authStore.accountPorts).toEqual(portsBefore)
      expect(authStore.activeAccountId).toBe('A')
      expect(authStore.user).toEqual(userBefore)
      expect(mocks.questsStore.activeCdpPort).toBe(9223)
      expect(mocks.questsStore.setActiveAccount).not.toHaveBeenCalled()
    })

    it('reconnects a matching saved B and activates it after authoritative reconciliation', async () => {
      localStorage.setItem('questHelper_accountCdpPorts', JSON.stringify({ A: 9223, B: null }))
      const authStore = useAuthStore()
      authStore.accounts = [accountSummary('A', true, 9223), accountSummary('B', false, 9224)]
      authStore.activeAccountId = 'A'
      authStore.user = { ...user, id: 'A' }
      mocks.reconnectCdpAccount.mockResolvedValue({
        status: 'reconnected',
        user: { ...user, id: 'B' },
        port: 9224,
      })
      mocks.listAccounts.mockResolvedValue({
        accounts: [accountSummary('A', true, 9223), accountSummary('B', true, 9224)],
        activeAccountId: 'B',
      })

      await expect(authStore.reconnectSavedAccount('B', 9224)).resolves.toMatchObject({
        status: 'reconnected',
        user: { id: 'B' },
      })

      expect(authStore.activeAccountId).toBe('B')
      expect(authStore.user?.id).toBe('B')
      expect(authStore.accountPorts).toEqual({ A: 9223, B: 9224 })
      expect(mocks.listAccounts).toHaveBeenCalledOnce()
      expect(mocks.questsStore.setActiveAccount).toHaveBeenLastCalledWith('B', 9224)
    })

    it('retains the safe reconnect projection when account-list refresh fails', async () => {
      localStorage.setItem('questHelper_accountCdpPorts', JSON.stringify({ A: 9223, B: null }))
      const authStore = useAuthStore()
      authStore.accounts = [accountSummary('A', true, 9223), accountSummary('B', false, 9224)]
      authStore.activeAccountId = 'A'
      authStore.user = { ...user, id: 'A' }
      mocks.reconnectCdpAccount.mockResolvedValue({
        status: 'reconnected',
        user: { ...user, id: 'B' },
        port: 9224,
      })
      mocks.listAccounts.mockRejectedValueOnce(new Error('list unavailable'))

      await expect(authStore.reconnectSavedAccount('B', 9224)).resolves.toMatchObject({
        status: 'reconnected',
        user: { id: 'B' },
      })

      expect(authStore.activeAccountId).toBe('B')
      expect(authStore.user?.id).toBe('B')
      expect(authStore.accountPorts).toEqual({ A: 9223, B: 9224 })
      expect(authStore.portForAccount('B')).toBe(9224)
      expect(authStore.error).toContain('Account reconnect succeeded')
    })

    it('returns identityChanged from confirmed Add without changing active state', async () => {
      const authStore = useAuthStore()
      authStore.accounts = [accountSummary('A', true, 9223), accountSummary('B', false, 9224)]
      authStore.activeAccountId = 'A'
      authStore.user = { ...user, id: 'A' }
      expect(authStore.setAccountPort('A', 9223)).toBe(true)
      expect(authStore.setAccountPort('B', 9224)).toBe(true)
      mocks.questsStore.setActiveAccount.mockClear()
      mocks.confirmAddCdpAccount.mockResolvedValue({
        status: 'identityChanged',
        user: { ...user, id: 'C' },
        port: 9225,
      })
      const portsBefore = { ...authStore.accountPorts }

      await expect(authStore.confirmAddClientAccount(9225, 'B')).resolves.toMatchObject({
        status: 'identityChanged',
        user: { id: 'C' },
      })

      expect(mocks.confirmAddCdpAccount).toHaveBeenCalledWith(9225, 'B', undefined)
      expect(mocks.listAccounts).not.toHaveBeenCalled()
      expect(authStore.accountPorts).toEqual(portsBefore)
      expect(authStore.activeAccountId).toBe('A')
      expect(authStore.user?.id).toBe('A')
      expect(mocks.questsStore.setActiveAccount).not.toHaveBeenCalled()
    })

    it('keeps AlreadySaved Add informational and does not duplicate its profile', async () => {
      const authStore = useAuthStore()
      authStore.accounts = [accountSummary('A', true, 9223), accountSummary('B', false, 9224)]
      authStore.activeAccountId = 'A'
      authStore.user = { ...user, id: 'A' }
      authStore.accountPorts = { A: 9223, B: 9224 }
      mocks.confirmAddCdpAccount.mockResolvedValue({
        status: 'alreadySaved',
        user: { ...user, id: 'B' },
        port: 9225,
      })
      mocks.questsStore.setActiveAccount.mockClear()

      const result = await authStore.confirmAddClientAccount(9225, 'B')

      expect(result.status).toBe('alreadySaved')
      expect(authStore.accounts.map(account => account.id)).toEqual(['A', 'B'])
      expect(authStore.accountPorts).toEqual({ A: 9223, B: 9224 })
      expect(authStore.activeAccountId).toBe('A')
      expect(authStore.user?.id).toBe('A')
      expect(mocks.listAccounts).not.toHaveBeenCalled()
      expect(mocks.questsStore.setActiveAccount).not.toHaveBeenCalled()
    })

    it('reconciles and saves the port only after a verified new Add', async () => {
      const authStore = useAuthStore()
      authStore.accounts = [accountSummary('A', true, 9223)]
      authStore.activeAccountId = 'A'
      authStore.user = { ...user, id: 'A' }
      expect(authStore.setAccountPort('A', 9223)).toBe(true)
      mocks.confirmAddCdpAccount.mockResolvedValue({
        status: 'added',
        user: { ...user, id: 'B' },
        port: 9224,
      })
      mocks.listAccounts.mockResolvedValue({
        accounts: [accountSummary('A', true, 9223), accountSummary('B', true, 9224)],
        activeAccountId: 'B',
      })

      await expect(authStore.confirmAddClientAccount(9224, 'B')).resolves.toMatchObject({
        status: 'added',
        user: { id: 'B' },
      })

      expect(authStore.accounts.map(account => account.id)).toEqual(['A', 'B'])
      expect(authStore.activeAccountId).toBe('B')
      expect(authStore.user?.id).toBe('B')
      expect(authStore.accountPorts).toEqual({ A: 9223, B: 9224 })
      expect(mocks.listAccounts).toHaveBeenCalledOnce()
      expect(mocks.questsStore.setActiveAccount).toHaveBeenLastCalledWith('B', 9224)
    })

    it('leaves account B unchanged when reconnect verifies a different user A', async () => {
      localStorage.setItem('questHelper_accountCdpPorts', JSON.stringify({ A: 9223, B: null }))
      const authStore = useAuthStore()
      authStore.accounts = [accountSummary('A', true, 9223), accountSummary('B', false, 9224)]
      authStore.activeAccountId = 'A'
      authStore.user = { ...user, id: 'A' }
      mocks.reconnectCdpAccount.mockResolvedValue({
        status: 'identityChanged',
        user: { ...user, id: 'A' },
        port: 9224,
      })
      mocks.questsStore.activeCdpPort = 9223
      mocks.questsStore.setActiveAccount.mockClear()
      const accountsBefore = [...authStore.accounts]
      const portsBefore = { ...authStore.accountPorts }
      const userBefore = { ...authStore.user! }

      await expect(authStore.reconnectSavedAccount('B', 9224)).resolves.toMatchObject({
        status: 'identityChanged',
        user: { id: 'A' },
      })

      expect(mocks.reconnectCdpAccount).toHaveBeenCalledWith('B', 9224, undefined)
      expect(mocks.listAccounts).not.toHaveBeenCalled()
      expect(authStore.accounts).toEqual(accountsBefore)
      expect(authStore.accountPorts).toEqual(portsBefore)
      expect(authStore.activeAccountId).toBe('A')
      expect(authStore.user).toEqual(userBefore)
      expect(mocks.questsStore.activeCdpPort).toBe(9223)
      expect(mocks.questsStore.setActiveAccount).not.toHaveBeenCalled()
    })

    it('reconnects the matching saved account and activates B', async () => {
      localStorage.setItem('questHelper_accountCdpPorts', JSON.stringify({ A: 9223, B: null }))
      const authStore = useAuthStore()
      authStore.accounts = [accountSummary('A', true, 9223), accountSummary('B', false, 9224)]
      authStore.activeAccountId = 'A'
      authStore.user = { ...user, id: 'A' }
      mocks.reconnectCdpAccount.mockResolvedValue({
        status: 'reconnected',
        user: { ...user, id: 'B' },
        port: 9224,
      })
      mocks.listAccounts.mockResolvedValue({
        accounts: [accountSummary('A', true, 9223), accountSummary('B', true, 9224)],
        activeAccountId: 'B',
      })

      await expect(authStore.reconnectSavedAccount('B', 9224)).resolves.toMatchObject({
        status: 'reconnected',
        user: { id: 'B' },
      })

      expect(authStore.activeAccountId).toBe('B')
      expect(authStore.user?.id).toBe('B')
      expect(authStore.accountPorts).toEqual({ A: 9223, B: 9224 })
      expect(mocks.listAccounts).toHaveBeenCalledOnce()
      expect(mocks.questsStore.setActiveAccount).toHaveBeenLastCalledWith('B', 9224)
    })

    it('keeps the verified reconnect projection if authoritative list refresh fails', async () => {
      localStorage.setItem('questHelper_accountCdpPorts', JSON.stringify({ A: 9223, B: null }))
      const authStore = useAuthStore()
      authStore.accounts = [accountSummary('A', true, 9223), accountSummary('B', false, 9224)]
      authStore.activeAccountId = 'A'
      authStore.user = { ...user, id: 'A' }
      mocks.reconnectCdpAccount.mockResolvedValue({
        status: 'reconnected',
        user: { ...user, id: 'B' },
        port: 9224,
      })
      mocks.listAccounts.mockRejectedValueOnce(new Error('list unavailable'))

      await expect(authStore.reconnectSavedAccount('B', 9224)).resolves.toMatchObject({
        status: 'reconnected',
        user: { id: 'B' },
      })

      expect(authStore.activeAccountId).toBe('B')
      expect(authStore.user?.id).toBe('B')
      expect(authStore.accountPorts).toEqual({ A: 9223, B: 9224 })
      expect(authStore.portForAccount('B')).toBe(9224)
      expect(authStore.error).toContain('Account reconnect succeeded')
    })

    it('rejects an explicitly selected Add port already assigned to another profile before IPC', async () => {
      const authStore = useAuthStore()
      authStore.accounts = [accountSummary('A')]
      expect(authStore.setAccountPort('A', 9333)).toBe(true)

      await expect(authStore.addAccountViaCdp(undefined, { port: 9333 })).resolves.toBe(false)

      expect(authStore.error).toBe('accounts.cdp_port_conflict')
      expect(mocks.autoAddAccountViaCdp).not.toHaveBeenCalled()
      expect(mocks.listAccounts).not.toHaveBeenCalled()
    })

    it('rejects a post-capture Add port conflict after authoritative reconciliation', async () => {
      const authStore = useAuthStore()
      const addResult = createDeferred<{ user: DiscordUser; alreadyKnown: boolean }>()
      authStore.accounts = [accountSummary('A')]
      authStore.activeAccountId = 'A'
      mocks.autoAddAccountViaCdp.mockReturnValueOnce(addResult.promise)
      mocks.listAccounts.mockResolvedValue({
        accounts: [accountSummary('A', true, 9333), accountSummary('B', true, 9333)],
        activeAccountId: 'B',
      })

      const adding = authStore.addAccountViaCdp(undefined, { port: 9333 })
      await vi.waitFor(() => expect(mocks.autoAddAccountViaCdp).toHaveBeenCalledOnce())
      // Another account claims the port while the backend capture is pending.
      expect(authStore.setAccountPort('A', 9333)).toBe(true)
      addResult.resolve({ user: { ...user, id: 'B' }, alreadyKnown: false })

      await expect(adding).resolves.toBe(false)

      expect(mocks.listAccounts).toHaveBeenCalledOnce()
      expect(authStore.error).toBe('accounts.cdp_port_conflict')
      expect(authStore.activeAccountId).toBe('B')
      expect(authStore.user).toMatchObject({ id: 'B' })
      expect(authStore.accountPorts).toEqual({ A: 9333, B: null })
      expect(JSON.parse(localStorage.getItem('questHelper_accountCdpPorts') ?? '{}')).toEqual({
        A: 9333,
        B: null,
      })
      expect(authStore.portForAccount('B')).toBe(0)
      expect(mocks.questsStore.setActiveAccount).toHaveBeenLastCalledWith('B', 0)

      // The fail-closed state survives reload; only an explicit valid repair clears it.
      setActivePinia(createPinia())
      const reloaded = useAuthStore()
      await reloaded.loadAccounts()
      expect(reloaded.accountPorts).toEqual({ A: 9333, B: null })
      expect(reloaded.portForAccount('B')).toBe(0)
      const storageBeforeRepair = localStorage.getItem('questHelper_accountCdpPorts')
      expect(reloaded.setAccountPort('B', 9333)).toBe(false)
      expect(localStorage.getItem('questHelper_accountCdpPorts')).toBe(storageBeforeRepair)
      expect(reloaded.setAccountPort('B', 9225)).toBe(true)
      expect(reloaded.accountPorts).toEqual({ A: 9333, B: 9225 })
      expect(reloaded.portForAccount('B')).toBe(9225)
      expect(mocks.questsStore.setActiveAccount).toHaveBeenLastCalledWith('B', 9225)
    })

    it('keeps a published Add account blocked and active when conflict refresh fails', async () => {
      const authStore = useAuthStore()
      const addResult = createDeferred<{ user: DiscordUser; alreadyKnown: boolean }>()
      authStore.accounts = [accountSummary('A')]
      authStore.activeAccountId = 'A'
      mocks.autoAddAccountViaCdp.mockReturnValueOnce(addResult.promise)
      mocks.listAccounts.mockRejectedValueOnce(new Error('list unavailable'))

      const adding = authStore.addAccountViaCdp(undefined, { port: 9333 })
      await vi.waitFor(() => expect(mocks.autoAddAccountViaCdp).toHaveBeenCalledOnce())
      expect(authStore.setAccountPort('A', 9333)).toBe(true)
      addResult.resolve({ user: { ...user, id: 'B' }, alreadyKnown: false })

      await expect(adding).resolves.toBe(false)

      expect(mocks.listAccounts).toHaveBeenCalledOnce()
      expect(authStore.activeAccountId).toBe('B')
      expect(authStore.user).toMatchObject({ id: 'B' })
      expect(authStore.accountPorts).toEqual({ A: 9333, B: null })
      expect(authStore.portForAccount('B')).toBe(0)
      expect(mocks.questsStore.setActiveAccount).toHaveBeenLastCalledWith('B', 0)
      expect(localStorage.getItem('questHelper_accountCdpPorts')).toBe(JSON.stringify({
        A: 9333,
        B: null,
      }))
      expect(authStore.error).toContain('accounts.cdp_port_conflict')
      expect(authStore.error).toContain('list unavailable')
    })

    it('returns Add failure if its authoritative snapshot reveals conflicting explicit assignments', async () => {
      const authStore = useAuthStore()
      mocks.autoAddAccountViaCdp.mockResolvedValue({
        user: { ...user, id: 'B' },
        alreadyKnown: false,
      })
      mocks.listAccounts.mockResolvedValue({
        accounts: [accountSummary('A', true, 9333), accountSummary('B', true, 9333)],
        activeAccountId: 'B',
      })

      await expect(authStore.addAccountViaCdp(undefined, { port: 9333 })).resolves.toBe(false)

      expect(authStore.error).toBe('accounts.cdp_port_conflict')
      expect(authStore.activeAccountId).toBe('B')
      expect(authStore.portForAccount('B')).toBe(0)
      // Both pre-existing explicit owners are blocked rather than one being
      // silently selected as the winner.
      expect(authStore.accountPorts).toEqual({ A: null, B: null })
      expect(mocks.questsStore.setActiveAccount).toHaveBeenLastCalledWith('B', 0)
    })

    it('waits for Add and its authoritative snapshot before activating a queued account', async () => {
      const authStore = useAuthStore()
      const addResult = createDeferred<{ user: DiscordUser; alreadyKnown: boolean }>()
      const addSnapshot = createDeferred<AccountsSnapshot>()
      const activeA = accountSummary('A', true, 9444)
      mocks.autoAddAccountViaCdp.mockReturnValueOnce(addResult.promise)
      mocks.listAccounts
        .mockReturnValueOnce(addSnapshot.promise)
        .mockResolvedValueOnce({ accounts: [activeA], activeAccountId: 'A' })
      mocks.activateAccount.mockResolvedValue(activeA)

      const add = authStore.addAccountViaCdp(undefined, { port: 9333 })
      await vi.waitFor(() => expect(mocks.autoAddAccountViaCdp).toHaveBeenCalledOnce())
      const activate = authStore.activateAccount('A')
      await Promise.resolve()
      expect(mocks.activateAccount).not.toHaveBeenCalled()

      addResult.resolve({ user: { ...user, id: 'B' }, alreadyKnown: false })
      await vi.waitFor(() => expect(mocks.listAccounts).toHaveBeenCalledOnce())
      expect(mocks.activateAccount).not.toHaveBeenCalled()

      addSnapshot.resolve({ accounts: [accountSummary('B')], activeAccountId: 'B' })
      await add
      await activate

      expect(mocks.activateAccount).toHaveBeenCalledWith('A')
      expect(authStore.activeAccountId).toBe('A')
      expect(authStore.user).toMatchObject({ id: 'A' })
      expect(authStore.portForAccount('A')).toBe(9444)
      expect(mocks.questsStore.setActiveAccount).toHaveBeenLastCalledWith('A', 9444)
    })

    it('waits for activation and its authoritative snapshot before adding a queued account', async () => {
      const authStore = useAuthStore()
      const activationResult = createDeferred<ReturnType<typeof accountSummary>>()
      const activationSnapshot = createDeferred<AccountsSnapshot>()
      const activeA = accountSummary('A', true, 9444)
      mocks.activateAccount.mockReturnValueOnce(activationResult.promise)
      mocks.listAccounts
        .mockReturnValueOnce(activationSnapshot.promise)
        .mockResolvedValueOnce({ accounts: [accountSummary('B')], activeAccountId: 'B' })
      mocks.autoAddAccountViaCdp.mockResolvedValue({
        user: { ...user, id: 'B' },
        alreadyKnown: false,
      })

      const activate = authStore.activateAccount('A')
      await vi.waitFor(() => expect(mocks.activateAccount).toHaveBeenCalledOnce())
      const add = authStore.addAccountViaCdp(undefined, { port: 9333 })
      await Promise.resolve()
      expect(mocks.autoAddAccountViaCdp).not.toHaveBeenCalled()

      activationResult.resolve(activeA)
      await vi.waitFor(() => expect(mocks.listAccounts).toHaveBeenCalledOnce())
      expect(mocks.autoAddAccountViaCdp).not.toHaveBeenCalled()
      activationSnapshot.resolve({ accounts: [activeA], activeAccountId: 'A' })
      await activate

      await add
      expect(mocks.autoAddAccountViaCdp).toHaveBeenCalledWith(9333, undefined)
      expect(authStore.activeAccountId).toBe('B')
      expect(authStore.user).toMatchObject({ id: 'B' })
      expect(authStore.portForAccount('B')).toBe(9333)
    })

    it('discards a pre-mutation list response after Add reconciles the new account', async () => {
      const authStore = useAuthStore()
      const oldSnapshot = createDeferred<AccountsSnapshot>()
      mocks.listAccounts
        .mockReturnValueOnce(oldSnapshot.promise)
        .mockResolvedValueOnce({ accounts: [accountSummary('B')], activeAccountId: 'B' })
      mocks.autoAddAccountViaCdp.mockResolvedValue({
        user: { ...user, id: 'B' },
        alreadyKnown: false,
      })

      const oldLoad = authStore.loadAccounts()
      await authStore.addAccountViaCdp(undefined, { port: 9333 })
      oldSnapshot.resolve({ accounts: [accountSummary('A')], activeAccountId: 'A' })
      await oldLoad

      expect(authStore.accounts.map(account => account.id)).toEqual(['B'])
      expect(authStore.activeAccountId).toBe('B')
      expect(authStore.user).toMatchObject({ id: 'B' })
      expect(mocks.questsStore.setActiveAccount).toHaveBeenLastCalledWith('B', 9333)
    })

    it('discards a pre-mutation list response after Activate reconciles its account', async () => {
      const authStore = useAuthStore()
      const oldSnapshot = createDeferred<AccountsSnapshot>()
      const activeA = accountSummary('A', true, 9444)
      mocks.listAccounts
        .mockReturnValueOnce(oldSnapshot.promise)
        .mockResolvedValueOnce({ accounts: [activeA], activeAccountId: 'A' })
      mocks.activateAccount.mockResolvedValue(activeA)

      const oldLoad = authStore.loadAccounts()
      await authStore.activateAccount('A')
      oldSnapshot.resolve({
        accounts: [accountSummary('B')],
        activeAccountId: 'B',
      })
      await oldLoad

      expect(authStore.accounts.map(account => account.id)).toEqual(['A'])
      expect(authStore.activeAccountId).toBe('A')
      expect(authStore.user).toMatchObject({ id: 'A' })
      expect(mocks.questsStore.setActiveAccount).toHaveBeenLastCalledWith('A', 9444)
    })

    it('keeps the published Add fallback and warns when refresh fails, then releases the queue', async () => {
      const authStore = useAuthStore()
      const activeA = accountSummary('A', true, 9444)
      mocks.autoAddAccountViaCdp.mockResolvedValue({
        user: { ...user, id: 'B' },
        alreadyKnown: false,
      })
      mocks.listAccounts
        .mockRejectedValueOnce(new Error('list unavailable'))
        .mockResolvedValueOnce({ accounts: [activeA], activeAccountId: 'A' })
      mocks.activateAccount.mockResolvedValue(activeA)

      await expect(authStore.addAccountViaCdp(undefined, { port: 9333 })).resolves.toBe(true)
      expect(authStore.activeAccountId).toBe('B')
      expect(authStore.user).toMatchObject({ id: 'B' })
      expect(authStore.accounts).toContainEqual(expect.objectContaining({
        id: 'B',
        isAuthenticated: true,
        lastCdpPort: 9333,
      }))
      expect(authStore.accountPorts['B']).toBe(9333)
      expect(authStore.error).toMatch(/Add account succeeded, but account refresh failed: list unavailable/)

      await authStore.activateAccount('A')
      expect(mocks.activateAccount).toHaveBeenCalledWith('A')
      expect(authStore.activeAccountId).toBe('A')
      expect(authStore.user).toMatchObject({ id: 'A' })
      expect(authStore.portForAccount('A')).toBe(9444)
    })

    it('propagates a rejected mutation without wedging the account action queue', async () => {
      const authStore = useAuthStore()
      mocks.activateAccount.mockRejectedValueOnce(new Error('activation failed'))
      mocks.autoAddAccountViaCdp.mockResolvedValue({
        user: { ...user, id: 'B' },
        alreadyKnown: false,
      })
      mocks.listAccounts.mockResolvedValue({
        accounts: [accountSummary('B')],
        activeAccountId: 'B',
      })

      await expect(authStore.activateAccount('A')).rejects.toThrow('activation failed')
      await expect(authStore.addAccountViaCdp(undefined, { port: 9333 })).resolves.toBe(true)

      expect(mocks.autoAddAccountViaCdp).toHaveBeenCalledWith(9333, undefined)
      expect(authStore.activeAccountId).toBe('B')
    })

    it('normal login reauthenticates and activates an existing offline account (no duplicate early-return)', async () => {
      const authStore = useAuthStore()
      const offlineB = { id: 'B', username: 'b', isAuthenticated: false, lastCdpPort: 9224 }
      authStore.accounts = [offlineB]
      authStore.user = null
      authStore.activeAccountId = 'B'
      authStore.setAccountPort('B', 9224)
      mocks.questsStore.setActiveAccount.mockClear()

      mocks.autoLoginViaCdp.mockResolvedValue({ ...user, id: 'B' })
      mocks.listAccounts.mockResolvedValue({
        accounts: [{ id: 'B', username: 'b', isAuthenticated: true }],
        activeAccountId: 'B',
      })

      await expect(authStore.loginViaCdp()).resolves.toBe(true)

      // A known/offline account is published and activated, never treated as a duplicate.
      expect(authStore.duplicateLoginAccountId).toBeNull()
      expect(authStore.user).toMatchObject({ id: 'B' })
      expect(authStore.activeAccountId).toBe('B')
      expect(authStore.accountPorts['B']).toBe(9224)
      expect(mocks.autoLoginViaCdp).toHaveBeenLastCalledWith(9224, undefined)
      expect(mocks.questsStore.cdpAvailable).toBe(true)
      expect(mocks.questsStore.gameQuestMode).toBe('cdp')
    })

    it('returns a port-conflict failure after legacy login publishes a now-conflicting account', async () => {
      const authStore = useAuthStore()
      const loginResult = createDeferred<DiscordUser>()
      authStore.accounts = [
        accountSummary('A'),
        accountSummary('B', false),
      ]
      authStore.activeAccountId = 'B'
      mocks.autoLoginViaCdp.mockReturnValueOnce(loginResult.promise)
      mocks.listAccounts.mockResolvedValue({
        accounts: [accountSummary('A', true, 9334), accountSummary('B', true, 9334)],
        activeAccountId: 'B',
      })

      const loggingIn = authStore.loginViaCdp(undefined, { port: 9334 })
      await vi.waitFor(() => expect(mocks.autoLoginViaCdp).toHaveBeenCalledOnce())
      expect(authStore.setAccountPort('A', 9334)).toBe(true)
      loginResult.resolve({ ...user, id: 'B' })

      await expect(loggingIn).resolves.toBe(false)

      expect(mocks.autoLoginViaCdp).toHaveBeenCalledWith(9334, undefined)
      expect(mocks.listAccounts).toHaveBeenCalledOnce()
      expect(authStore.error).toBe('accounts.cdp_port_conflict')
      expect(authStore.activeAccountId).toBe('B')
      expect(authStore.user).toMatchObject({ id: 'B' })
      expect(authStore.accountPorts).toEqual({ A: 9334, B: null })
      expect(authStore.portForAccount('B')).toBe(0)
      expect(mocks.questsStore.setActiveAccount).toHaveBeenLastCalledWith('B', 0)
      expect(JSON.parse(localStorage.getItem('questHelper_accountCdpPorts') ?? '{}')).toEqual({
        A: 9334,
        B: null,
      })
    })

    it('discards a stale account snapshot that resolves after a newer one', async () => {
      const authStore = useAuthStore()
      const deferredA = createDeferred<AccountsSnapshot>()
      const deferredB = createDeferred<AccountsSnapshot>()
      mocks.listAccounts
        .mockReturnValueOnce(deferredA.promise)
        .mockReturnValueOnce(deferredB.promise)

      const loadA = authStore.loadAccounts()
      const loadB = authStore.loadAccounts()

      const accountB = { id: 'B', username: 'b', isAuthenticated: true }
      deferredB.resolve({ accounts: [accountB], activeAccountId: 'B' })
      await loadB
      expect(authStore.activeAccountId).toBe('B')
      expect(authStore.accounts).toEqual([accountB])
      expect(mocks.questsStore.setActiveAccount).toHaveBeenLastCalledWith('B', 9223)

      mocks.questsStore.setActiveAccount.mockClear()
      // The older A response settles last and must not overwrite B.
      deferredA.resolve({ accounts: [{ id: 'A', username: 'a', isAuthenticated: true }], activeAccountId: 'A' })
      await loadA

      expect(authStore.activeAccountId).toBe('B')
      expect(authStore.accounts).toEqual([accountB])
      expect(mocks.questsStore.setActiveAccount).not.toHaveBeenCalled()
    })

    it('does not flag a newly captured account as a duplicate', async () => {
      const authStore = useAuthStore()
      authStore.accounts = []

      await authStore.loginViaCdp(undefined, { port: 9223 })

      expect(authStore.duplicateLoginAccountId).toBeNull()
    })

    it('clears duplicateLoginAccountId at the start of each add attempt', async () => {
      const authStore = useAuthStore()

      mocks.autoAddAccountViaCdp.mockResolvedValueOnce({
        user: { ...user, id: '123' },
        alreadyKnown: true,
      })
      await authStore.addAccountViaCdp(undefined, { port: 9223 })
      expect(authStore.duplicateLoginAccountId).toBe('123')

      mocks.autoAddAccountViaCdp.mockResolvedValueOnce({
        user: { ...user, id: '456' },
        alreadyKnown: false,
      })
      await authStore.addAccountViaCdp(undefined, { port: 9223 })
      expect(authStore.duplicateLoginAccountId).toBeNull()
    })

    it('add-account falls back to the global port when none is supplied', async () => {
      const authStore = useAuthStore()
      mocks.autoAddAccountViaCdp.mockResolvedValue({
        user: { ...user, id: 'B' },
        alreadyKnown: true,
      })

      await authStore.addAccountViaCdp()

      expect(mocks.autoAddAccountViaCdp).toHaveBeenCalledWith(9223, undefined)
    })

    it('persists and hydrates accountPorts across store recreation', () => {
      const authStore = useAuthStore()
      authStore.setAccountPort('111', 9555)

      const raw = localStorage.getItem('questHelper_accountCdpPorts')
      expect(JSON.parse(raw ?? '{}')).toEqual({ '111': 9555 })

      setActivePinia(createPinia())
      const reloaded = useAuthStore()
      expect(reloaded.accountPorts).toEqual({ '111': 9555 })
      expect(reloaded.portForAccount('111')).toBe(9555)
    })
  })
})
