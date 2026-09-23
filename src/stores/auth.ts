import { defineStore } from 'pinia'
import { ref, computed } from 'vue'
import type { AccountSummary, DiscordUser, ProgramReward, AuthProgressHandler } from '@/api/tauri'
import {
  activateAccount as activateAccountIpc,
  autoLoginViaCdp,
  getProgramRewards,
  listAccounts,
  removeAccount as removeAccountIpc
} from '@/api/tauri'
import { useQuestsStore } from './quests'
import { useI18n } from 'vue-i18n'
import { useNow } from '@vueuse/core'
import { getNitroOrbsClaim } from '@/utils/nitroOrbsCountdown'

export const useAuthStore = defineStore('auth', () => {
  const { t } = useI18n()
  const user = ref<DiscordUser | null>(null)
  const loading = ref(false)
  const error = ref<string | null>(null)

  // Account surface (Phase 6.4A). The legacy `user` projection stays for the
  // currently authenticated active account only.
  const accounts = ref<AccountSummary[]>([])
  const activeAccountId = ref<string | null>(null)

  // Discord's Program Rewards endpoint owns the monthly Orbs schedule.
  const nitroProgramReward = ref<ProgramReward | null>(null)
  const programRewardLoading = ref(false)
  const programRewardError = ref<string | null>(null)
  const programRewardLoaded = ref(false)
  const currentTime = useNow({ interval: 60_000 })
  let programRewardRequestRevision = 0

  function resetProgramRewardState() {
    programRewardRequestRevision += 1
    nitroProgramReward.value = null
    programRewardLoading.value = false
    programRewardError.value = null
    programRewardLoaded.value = false
  }

  /**
   * Log in by capturing the currently running Discord client's session over CDP
   * (the primary login path on Linux). The raw token is never exposed to the
   * frontend: the backend captures, validates, and stores it, returning only the
   * DiscordUser. Requires Discord to be running with CDP enabled.
   */
  async function loginViaCdp(onProgress?: AuthProgressHandler) {
    loading.value = true
    error.value = null
    resetProgramRewardState()
    try {
      const questsStore = useQuestsStore()
      user.value = await autoLoginViaCdp(questsStore.cdpPort, onProgress)
      activeAccountId.value = user.value.id
      questsStore.setActiveAccount(user.value.id)
      void loadAccounts()

      // CDP is available by definition here (we just used it). Keep the login
      // method and quest execution method aligned so the first quest does not
      // fall back to a previously saved simulation preference.
      questsStore.cdpAvailable = true
      questsStore.gameQuestMode = 'cdp'

      // Refresh the connection state and the rest of the post-login data.
      bootstrapAfterLogin(questsStore, 'CDP init after CDP login failed:')

      return true
    } catch (e) {
      error.value = e instanceof Error ? e.message : String(e)
      return false
    } finally {
      loading.value = false
    }
  }

  // Keep post-login refresh work non-blocking.
  function bootstrapAfterLogin(questsStore: ReturnType<typeof useQuestsStore>, cdpWarning: string) {
    questsStore.initCdpMode().catch(err => {
      console.warn(cdpWarning, err)
    })
    questsStore.getDetectableGames().catch(err => {
      console.warn('Background game list fetch failed:', err)
    })
    questsStore.fetchOrbsBalance().catch(err => {
      console.warn('Background Orbs balance fetch failed:', err)
    })
    fetchNitroProgramReward().catch(err => {
      console.warn('Background Nitro program reward fetch failed:', err)
    })
  }

  async function logout() {
    // Invalidate account-scoped requests before awaiting quest shutdown.
    resetProgramRewardState()

    // Stop any in-progress quest before clearing state
    const questsStore = useQuestsStore()
    try {
      await questsStore.stop()
    } catch (e) {
      console.warn('Failed to stop quest during logout:', e)
    }

    user.value = null
    error.value = null

    // Reset quests store to clear all cached data from previous account
    questsStore.resetForLogout()
  }

  async function fetchNitroProgramReward(force = false) {
    if (programRewardLoading.value) return
    if (!force && programRewardLoaded.value) return
    // CDP auto-login is authenticated on the backend but exposes no frontend
    // token, so gate on the logged-in user rather than the raw token.
    const requestUserId = user.value?.id
    if (!requestUserId) return
    const requestRevision = ++programRewardRequestRevision
    programRewardLoading.value = true
    programRewardError.value = null
    try {
      const rewards = await getProgramRewards()
      if (requestRevision !== programRewardRequestRevision || user.value?.id !== requestUserId) return
      nitroProgramReward.value = rewards.find(reward => {
        const program = reward.reward_program
        // Discord's official ProgramReward enum is NITRO=0, XBOX=1.
        // Keep the string forms for keyed/legacy response normalization.
        return program === 0 || program === '0' || String(program).toUpperCase() === 'NITRO'
      }) ?? null
      programRewardLoaded.value = true
    } catch (e) {
      if (requestRevision !== programRewardRequestRevision || user.value?.id !== requestUserId) return
      programRewardError.value = e as string
      console.warn('Failed to fetch Nitro program reward:', e)
    } finally {
      if (requestRevision === programRewardRequestRevision && user.value?.id === requestUserId) {
        programRewardLoading.value = false
      }
    }
  }

  const activeAccount = computed(
    () => accounts.value.find(account => account.id === activeAccountId.value) ?? null
  )
  const isActiveAccountAuthenticated = computed(
    () => activeAccount.value?.isAuthenticated === true
  )

  /** Load the account snapshot (list + active id). */
  async function loadAccounts() {
    const snapshot = await listAccounts()
    accounts.value = snapshot.accounts
    activeAccountId.value = snapshot.activeAccountId ?? null
    useQuestsStore().setActiveAccount(activeAccountId.value)
  }

  /**
   * Activate an account. A persisted offline profile can be selected; it never
   * rehydrates a token/client, so the legacy authenticated projection is cleared
   * unless the activated account is the one already signed in.
   */
  async function activateAccount(accountId: string) {
    const account = await activateAccountIpc(accountId)
    activeAccountId.value = account.id
    const existing = accounts.value.find(item => item.id === account.id)
    if (existing) {
      accounts.value = accounts.value.map(item => (item.id === account.id ? account : item))
    } else {
      accounts.value = [...accounts.value, account]
    }
    if (account.isAuthenticated) {
      // Switching to a currently authenticated runtime hydrates the legacy
      // projection from the secret-free summary (no token is ever invented).
      user.value = {
        id: account.id,
        username: account.username,
        discriminator: account.discriminator ?? '0',
        avatar: account.avatar ?? null,
        global_name: account.globalName ?? null,
      }
    } else {
      // Offline profile: no authenticated projection.
      user.value = null
      resetProgramRewardState()
    }
    useQuestsStore().setActiveAccount(account.id)
    return account
  }

  /**
   * Remove an account. If it was the authenticated active account, its legacy
   * projection and cached quests are cleared. Never affects another account.
   */
  async function removeAccount(accountId: string) {
    const snapshot = await removeAccountIpc(accountId)
    accounts.value = snapshot.accounts
    activeAccountId.value = snapshot.activeAccountId ?? null
    useQuestsStore().setActiveAccount(activeAccountId.value)
    if (user.value?.id === accountId) {
      user.value = null
      resetProgramRewardState()
      useQuestsStore().resetForLogout()
    }
  }

  // Discord supplies the authoritative absolute timestamp, so no local or
  // UTC calendar arithmetic is needed here.
  const nextOrbsClaim = computed<
    { value: number; unit: 'days' | 'hours' | 'minutes' } | null
  >(() => {
    return getNitroOrbsClaim(nitroProgramReward.value?.next_reward_date, currentTime.value)
  })

  // Localized Nitro membership status label + color class (null for non-members).
  const nitroStatus = computed<{ label: string; class: string } | null>(() => {
    const pt = user.value?.premium_type
    if (!pt || pt === 0) return null
    if (pt === 1) return { label: t('user.nitro_classic'), class: 'text-sky-600 dark:text-sky-400' }
    if (pt === 2) return { label: t('user.nitro'), class: 'text-violet-600 dark:text-violet-400' }
    if (pt === 3) return { label: t('user.nitro_basic'), class: 'text-indigo-600 dark:text-indigo-400' }
    return null
  })

  return {
    user,
    loading,
    error,
    accounts,
    activeAccountId,
    activeAccount,
    isActiveAccountAuthenticated,
    nitroProgramReward,
    programRewardLoading,
    programRewardError,
    nextOrbsClaim,
    nitroStatus,
    loadAccounts,
    activateAccount,
    removeAccount,
    loginViaCdp,
    logout,
    fetchNitroProgramReward
  }
})
