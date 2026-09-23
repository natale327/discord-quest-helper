import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'
import type { AccountsSnapshot, DiscordUser } from '@/api/tauri'
import { useAuthStore } from './auth'

const mocks = vi.hoisted(() => ({
  autoLoginViaCdp: vi.fn(),
  autoAddAccountViaCdp: vi.fn(),
  getProgramRewards: vi.fn(),
  listAccounts: vi.fn(),
  activateAccount: vi.fn(),
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
  getProgramRewards: mocks.getProgramRewards,
  listAccounts: mocks.listAccounts,
  activateAccount: mocks.activateAccount,
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
    mocks.questsStore.cdpAvailable = false
    mocks.questsStore.activeCdpPort = 9223
    mocks.questsStore.gameQuestMode = 'simulate'
    mocks.questsStore.quests = []
    mocks.questsStore.questsByAccount = {}
    mocks.autoLoginViaCdp.mockResolvedValue(user)
    mocks.autoAddAccountViaCdp.mockResolvedValue({ user, alreadyKnown: false })
    mocks.getProgramRewards.mockResolvedValue([])
    mocks.listAccounts.mockResolvedValue({
      accounts: [accountSummary(user.id)],
      activeAccountId: user.id,
    })
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

      // Explicit port wins for a login attempt.
      await authStore.loginViaCdp(undefined, { port: 9777 })
      expect(mocks.autoLoginViaCdp).toHaveBeenCalledWith(9777, undefined)

      // No explicit port: the active account's saved port is used.
      authStore.activeAccountId = 'acct-a'
      await authStore.loginViaCdp()
      expect(mocks.autoLoginViaCdp).toHaveBeenLastCalledWith(9666, undefined)
    })

    it('uses each active account own port for CDP login', async () => {
      const authStore = useAuthStore()
      authStore.accounts = [
        { id: '111', username: 'a', isAuthenticated: true },
        { id: '222', username: 'b', isAuthenticated: true },
      ]
      authStore.setAccountPort('111', 9223)
      authStore.setAccountPort('222', 9333)
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
