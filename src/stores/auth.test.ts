import { beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'
import type { DiscordUser } from '@/api/tauri'
import { useAuthStore } from './auth'

const mocks = vi.hoisted(() => ({
  autoLoginViaCdp: vi.fn(),
  getProgramRewards: vi.fn(),
  listAccounts: vi.fn(),
  activateAccount: vi.fn(),
  removeAccount: vi.fn(),
  questsStore: {
    cdpPort: 9223,
    cdpAvailable: false,
    gameQuestMode: 'simulate',
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

describe('auth CDP-only login', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    vi.clearAllMocks()
    mocks.questsStore.cdpAvailable = false
    mocks.questsStore.gameQuestMode = 'simulate'
    mocks.autoLoginViaCdp.mockResolvedValue(user)
    mocks.getProgramRewards.mockResolvedValue([])
    mocks.listAccounts.mockResolvedValue({ accounts: [], activeAccountId: undefined })
    mocks.questsStore.initCdpMode.mockResolvedValue(undefined)
    mocks.questsStore.getDetectableGames.mockResolvedValue(undefined)
    mocks.questsStore.fetchOrbsBalance.mockResolvedValue(undefined)
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
    await authStore.activateAccount('999')

    expect(authStore.activeAccountId).toBe('999')
    expect(authStore.isActiveAccountAuthenticated).toBe(false)
    expect(authStore.user).toBeNull()
    expect(mocks.questsStore.setActiveAccount).toHaveBeenCalledWith('999')
  })

  it('removing the authenticated active account clears its projection', async () => {
    const authStore = useAuthStore()
    await authStore.loginViaCdp()

    mocks.removeAccount.mockResolvedValue({ accounts: [], activeAccountId: undefined })
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
})
