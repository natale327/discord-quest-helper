import { defineStore } from 'pinia'
import { ref, computed } from 'vue'
import type { AccountSummary, AccountsSnapshot, DiscordUser, ProgramReward, AuthProgressHandler } from '@/api/tauri'
import {
  activateAccount as activateAccountIpc,
  autoAddAccountViaCdp,
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
  let accountMutationTail: Promise<void> = Promise.resolve()

  /** Serialize only account IPC mutations and their account-list reconciliation. */
  function serializeAccountMutation<T>(operation: () => Promise<T>): Promise<T> {
    const result = accountMutationTail.then(operation)
    // Keep the tail fulfilled regardless of the caller's result, while returning
    // the original promise so each caller still observes its own value/rejection.
    accountMutationTail = result.then(() => undefined, () => undefined)
    return result
  }

  // Per-account CDP ports (Phase 6.5). Persisted locally so an account keeps its
  // port across reloads; the global `questsStore.cdpPort` stays the fallback.
  const ACCOUNT_PORTS_KEY = 'questHelper_accountCdpPorts'

  function loadStoredAccountPorts(): Record<string, number> {
    try {
      const raw = localStorage.getItem(ACCOUNT_PORTS_KEY)
      if (!raw) return {}
      const parsed: unknown = JSON.parse(raw)
      if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return {}
      const ports: Record<string, number> = {}
      for (const [id, value] of Object.entries(parsed as Record<string, unknown>)) {
        if (typeof value === 'number' && Number.isInteger(value) && value > 0 && value <= 65535) {
          ports[id] = value
        }
      }
      return ports
    } catch {
      return {}
    }
  }

  const accountPorts = ref<Record<string, number>>(loadStoredAccountPorts())
  /** Set when the captured login user was already an existing account. */
  const duplicateLoginAccountId = ref<string | null>(null)

  function persistAccountPorts() {
    try {
      localStorage.setItem(ACCOUNT_PORTS_KEY, JSON.stringify(accountPorts.value))
    } catch {
      /* persistence is best-effort; the in-memory map still works */
    }
  }

  /**
   * The CDP port to use for an account: explicit account override, else the
   * profile's historical `lastCdpPort`, else the global default port.
   */
  function portForAccount(accountId: string): number {
    const known = accountPorts.value[accountId]
    if (typeof known === 'number') return known
    const profile = accounts.value.find(account => account.id === accountId)
    if (profile?.lastCdpPort) return profile.lastCdpPort
    return useQuestsStore().cdpPort
  }

  /** Record and persist a port without changing which account is active. */
  function rememberAccountPort(accountId: string, port: number): boolean {
    if (!accountId || !Number.isInteger(port) || port <= 0 || port > 65535) return false
    accountPorts.value = { ...accountPorts.value, [accountId]: port }
    persistAccountPorts()
    return true
  }

  /** Record (and persist) an account's CDP port; keeps the active port in sync. */
  function setAccountPort(accountId: string, port: number) {
    if (!rememberAccountPort(accountId, port)) return
    const questsStore = useQuestsStore()
    if (activeAccountId.value === accountId) {
      questsStore.setActiveAccount(accountId, port)
    }
  }

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
  async function loginViaCdp(
    onProgress?: AuthProgressHandler,
    options?: { port?: number }
  ) {
    return serializeAccountMutation(async () => {
      loading.value = true
      error.value = null
      // Each attempt starts clean: a previous duplicate result never leaks.
      duplicateLoginAccountId.value = null
      resetProgramRewardState()
      let backendMutationCompleted = false
      try {
        const questsStore = useQuestsStore()
        const activeId = activeAccountId.value
        // Resolution order: explicit port > the account being captured (its saved
        // port, then its profile `lastCdpPort`) > the global default port.
        const resolvedPort =
          options?.port ?? (activeId ? portForAccount(activeId) : questsStore.cdpPort)

        invalidateAccountsLoads()
        const captured = await autoLoginViaCdp(resolvedPort, onProgress)
        backendMutationCompleted = true
        invalidateAccountsLoads()

        // Normal login/reauth always publishes and activates the captured account,
        // including a known offline account. The returned user is a safe fallback
        // if the authoritative list refresh is unavailable.
        rememberAccountPort(captured.id, resolvedPort)
        applyAuthenticatedUserFallback(captured, resolvedPort)
        await refreshAccountsAfterMutation('CDP login')

        // CDP is available by definition here (we just used it). Keep the login
        // method and quest execution method aligned with the active session.
        questsStore.cdpAvailable = true
        questsStore.gameQuestMode = 'cdp'
        bootstrapAfterLogin(questsStore, 'CDP init after CDP login failed:')

        return true
      } catch (e) {
        error.value = e instanceof Error ? e.message : String(e)
        return false
      } finally {
        if (!backendMutationCompleted) invalidateAccountsLoads()
        loading.value = false
      }
    })
  }

  /**
   * Add an account by capturing the running Discord client's session over CDP.
   * Unlike {@link loginViaCdp}, the backend decides whether the captured account
   * is new: on `alreadyKnown` we surface the duplicate and leave the current
   * account, saved ports, and quest state untouched. A genuinely new account is
   * published/activated and its port persisted, exactly once.
   */
  async function addAccountViaCdp(
    onProgress?: AuthProgressHandler,
    options?: { port: number }
  ): Promise<boolean> {
    return serializeAccountMutation(async () => {
      loading.value = true
      error.value = null
      // Each attempt starts clean: a previous duplicate result never leaks.
      duplicateLoginAccountId.value = null
      let backendMutationCompleted = false
      try {
        const questsStore = useQuestsStore()
        const resolvedPort = options?.port ?? questsStore.cdpPort

        invalidateAccountsLoads()
        const result = await autoAddAccountViaCdp(resolvedPort, onProgress)
        backendMutationCompleted = true
        invalidateAccountsLoads()

        // Backend truth wins: never infer duplicates from the local account list.
        // The duplicate command does not publish or activate, so keep the existing
        // account/session/quest projection and skip account-list reconciliation.
        if (result.alreadyKnown) {
          duplicateLoginAccountId.value = result.user.id
          return true
        }

        // The Add command published this session. Apply a safe local fallback
        // immediately, then replace it with the authoritative account snapshot.
        resetProgramRewardState()
        rememberAccountPort(result.user.id, resolvedPort)
        applyAuthenticatedUserFallback(result.user, resolvedPort)
        await refreshAccountsAfterMutation('Add account')

        questsStore.cdpAvailable = true
        questsStore.gameQuestMode = 'cdp'
        bootstrapAfterLogin(questsStore, 'CDP init after CDP add-account failed:')

        return true
      } catch (e) {
        error.value = e instanceof Error ? e.message : String(e)
        return false
      } finally {
        if (!backendMutationCompleted) invalidateAccountsLoads()
        loading.value = false
      }
    })
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

  // Monotonic revision for account-list loads. Only the newest response may
  // apply, so a stale snapshot that resolves after a newer one is discarded.
  let accountsLoadRevision = 0

  function invalidateAccountsLoads() {
    accountsLoadRevision += 1
  }

  function syncUserProjection(activeAccount: AccountSummary | undefined) {
    const nextUserId = activeAccount?.isAuthenticated ? activeAccount.id : null
    if (user.value?.id !== nextUserId) resetProgramRewardState()

    if (!activeAccount || !activeAccount.isAuthenticated) {
      user.value = null
      return
    }

    // Keep the richer captured DiscordUser while it still represents the active
    // account; otherwise hydrate only from the backend's secret-free online DTO.
    if (user.value?.id !== activeAccount.id) {
      user.value = {
        id: activeAccount.id,
        username: activeAccount.username,
        discriminator: activeAccount.discriminator ?? '0',
        avatar: activeAccount.avatar ?? null,
        global_name: activeAccount.globalName ?? null,
      }
    }
  }

  function applyAccountsSnapshot(snapshot: AccountsSnapshot) {
    accounts.value = snapshot.accounts
    const activeId = snapshot.activeAccountId ?? null
    activeAccountId.value = activeId
    syncUserProjection(snapshot.accounts.find(account => account.id === activeId))
    useQuestsStore().setActiveAccount(
      activeId,
      activeId ? portForAccount(activeId) : undefined
    )
  }

  function applyAuthenticatedUserFallback(captured: DiscordUser, port: number) {
    const existing = accounts.value.find(account => account.id === captured.id)
    const fallbackAccount: AccountSummary = {
      ...existing,
      id: captured.id,
      username: captured.username,
      discriminator: captured.discriminator,
      avatar: captured.avatar ?? undefined,
      globalName: captured.global_name ?? undefined,
      lastCdpPort: Number.isInteger(port) && port > 0 && port <= 65535
        ? port
        : existing?.lastCdpPort,
      isAuthenticated: true,
    }
    // Reauthentication may return a richer/fresher user projection for the same
    // account, so keep that exact captured DTO (including premium metadata).
    user.value = captured
    applyAccountsSnapshot({
      accounts: existing
        ? accounts.value.map(account => account.id === captured.id ? fallbackAccount : account)
        : [...accounts.value, fallbackAccount],
      activeAccountId: captured.id,
    })
  }

  function applyActivatedAccountFallback(account: AccountSummary) {
    const existing = accounts.value.some(item => item.id === account.id)
    if (account.isAuthenticated) {
      if (user.value?.id !== account.id) resetProgramRewardState()
      const currentUser = user.value?.id === account.id ? user.value : null
      user.value = {
        ...currentUser,
        id: account.id,
        username: account.username,
        discriminator: account.discriminator ?? '0',
        avatar: account.avatar ?? null,
        global_name: account.globalName ?? null,
      }
    }
    applyAccountsSnapshot({
      accounts: existing
        ? accounts.value.map(item => item.id === account.id ? account : item)
        : [...accounts.value, account],
      activeAccountId: account.id,
    })
  }

  /**
   * A successful backend mutation gets an authoritative list snapshot before
   * the next mutation may run. If a newer independent list request supersedes
   * this one, retry so the mutation queue still waits for the latest snapshot.
   */
  async function loadAccountsAfterMutation(): Promise<void> {
    while (true) {
      const revision = ++accountsLoadRevision
      const snapshot = await listAccounts()
      if (revision !== accountsLoadRevision) continue
      applyAccountsSnapshot(snapshot)
      return
    }
  }

  async function refreshAccountsAfterMutation(action: string): Promise<boolean> {
    try {
      await loadAccountsAfterMutation()
      return true
    } catch (e) {
      const reason = e instanceof Error ? e.message : String(e)
      // Login/Add already returned a published user and activation/removal have
      // an IPC fallback. Keep that state aligned and make refresh failure visible.
      error.value = `${action} succeeded, but account refresh failed: ${reason}`
      return false
    }
  }

  /** Load the account snapshot (list + active id). */
  async function loadAccounts() {
    const revision = ++accountsLoadRevision
    const snapshot = await listAccounts()
    if (revision !== accountsLoadRevision) return
    applyAccountsSnapshot(snapshot)
  }

  /**
   * Activate an account. A persisted offline profile can be selected; it never
   * rehydrates a token/client, so the legacy authenticated projection is cleared
   * unless the activated account is the one already signed in.
   */
  async function activateAccount(accountId: string) {
    return serializeAccountMutation(async () => {
      error.value = null
      let backendMutationCompleted = false
      try {
        invalidateAccountsLoads()
        const account = await activateAccountIpc(accountId)
        backendMutationCompleted = true
        invalidateAccountsLoads()
        applyActivatedAccountFallback(account)
        await refreshAccountsAfterMutation('Account activation')
        return account
      } catch (e) {
        error.value = e instanceof Error ? e.message : String(e)
        throw e
      } finally {
        if (!backendMutationCompleted) invalidateAccountsLoads()
      }
    })
  }

  /**
   * Remove an account. If it was the authenticated active account, its legacy
   * projection and cached quests are cleared. Never affects another account.
   */
  async function removeAccount(accountId: string) {
    return serializeAccountMutation(async () => {
      error.value = null
      const removedActiveUser = user.value?.id === accountId
      let backendMutationCompleted = false
      try {
        invalidateAccountsLoads()
        const snapshot = await removeAccountIpc(accountId)
        backendMutationCompleted = true
        invalidateAccountsLoads()
        applyAccountsSnapshot(snapshot)
        if (removedActiveUser && !snapshot.activeAccountId) {
          useQuestsStore().resetForLogout()
        }
        await refreshAccountsAfterMutation('Account removal')
      } catch (e) {
        error.value = e instanceof Error ? e.message : String(e)
        throw e
      } finally {
        if (!backendMutationCompleted) invalidateAccountsLoads()
      }
    })
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
    accountPorts,
    duplicateLoginAccountId,
    portForAccount,
    setAccountPort,
    nitroProgramReward,
    programRewardLoading,
    programRewardError,
    nextOrbsClaim,
    nitroStatus,
    loadAccounts,
    activateAccount,
    removeAccount,
    loginViaCdp,
    addAccountViaCdp,
    logout,
    fetchNitroProgramReward
  }
})
