import { defineStore } from 'pinia'
import { ref, computed } from 'vue'
import type { AccountSummary, AccountsSnapshot, DiscordUser, ProgramReward, AuthProgressHandler } from '@/api/tauri'
import {
  activateAccount as activateAccountIpc,
  activateOnlineAccount as activateOnlineAccountIpc,
  autoAddAccountViaCdp,
  autoLoginViaCdp,
  confirmAddCdpAccount as confirmAddCdpAccountIpc,
  getProgramRewards,
  listAccounts,
  previewCdpIdentity as previewCdpIdentityIpc,
  reconnectCdpAccount as reconnectCdpAccountIpc,
  removeAccount as removeAccountIpc
} from '@/api/tauri'
import type {
  CdpIdentityPreview,
  ConfirmAddCdpResult,
  ReconnectCdpResult,
} from '@/api/tauri'
import { useQuestsStore } from './quests'
import { useI18n } from 'vue-i18n'
import { useNow } from '@vueuse/core'
import { getNitroOrbsClaim } from '@/utils/nitroOrbsCountdown'

export interface ClientAccountPortConflict {
  status: 'portConflict'
  user: DiscordUser
  port: number
}

export type ConfirmAddClientAccountResult = ConfirmAddCdpResult | ClientAccountPortConflict
export type ReconnectSavedAccountResult = ReconnectCdpResult | ClientAccountPortConflict

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
  let clientIdentityRequestRevision = 0

  function invalidateClientAccountPreviews() {
    clientIdentityRequestRevision += 1
  }

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
  const MIN_ACCOUNT_CDP_PORT = 1024
  const MAX_ACCOUNT_CDP_PORT = 65535
  /** Exhaustion sentinel: zero is outside the accepted account-port range. */
  const NO_AVAILABLE_ACCOUNT_PORT = 0

  function isValidAccountCdpPort(port: number): boolean {
    return Number.isInteger(port) && port >= MIN_ACCOUNT_CDP_PORT && port <= MAX_ACCOUNT_CDP_PORT
  }

  function loadStoredAccountPorts(): Record<string, number | null> {
    try {
      const raw = localStorage.getItem(ACCOUNT_PORTS_KEY)
      if (!raw) return {}
      const parsed: unknown = JSON.parse(raw)
      if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) return {}
      const ports: Record<string, number | null> = {}
      for (const [id, value] of Object.entries(parsed as Record<string, unknown>)) {
        if (value === null || (typeof value === 'number' && isValidAccountCdpPort(value))) {
          ports[id] = value
        }
      }
      return ports
    } catch {
      return {}
    }
  }

  const accountPorts = ref<Record<string, number | null>>(loadStoredAccountPorts())
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
    if (Object.prototype.hasOwnProperty.call(accountPorts.value, accountId)) {
      const known = accountPorts.value[accountId]
      return known === null ? 0 : known
    }
    const profile = accounts.value.find(account => account.id === accountId)
    if (profile?.lastCdpPort) return profile.lastCdpPort
    return useQuestsStore().cdpPort
  }

  /** Read-only identity check for one selected client; it changes no account state. */
  async function previewClientAccount(port: number): Promise<CdpIdentityPreview | null> {
    const requestRevision = ++clientIdentityRequestRevision
    const startingActiveAccountId = activeAccountId.value
    const preview = await previewCdpIdentityIpc(port)
    if (
      requestRevision !== clientIdentityRequestRevision ||
      startingActiveAccountId !== activeAccountId.value
    ) return null
    return preview
  }

  /** Resolve each saved account's effective assignment without rewriting it. */
  function assignedAccountPorts(excludingAccountId?: string): Set<number> {
    const assigned = new Set<number>()
    const accountsById = new Map(accounts.value.map(account => [account.id, account]))
    const accountIds = new Set([
      ...accountsById.keys(),
      ...Object.keys(accountPorts.value),
    ])
    const globalDefaultPort = useQuestsStore().cdpPort

    for (const accountId of accountIds) {
      if (accountId === excludingAccountId) continue

      const hasExplicitPort = Object.prototype.hasOwnProperty.call(accountPorts.value, accountId)
      const explicitPort = accountPorts.value[accountId]
      const profile = accountsById.get(accountId)
      // A saved profile without either override resolves to the global default
      // in `portForAccount`, so reserve that fallback without persisting it.
      // Orphaned local overrides still reserve their explicit value, but an
      // account absent from the profile list does not reserve the global port.
      const port = hasExplicitPort
        ? explicitPort
        : profile ? profile.lastCdpPort || globalDefaultPort : undefined
      if (typeof port === 'number' && isValidAccountCdpPort(port)) assigned.add(port)
    }

    return assigned
  }

  /** Check if a valid port is free for the account being configured. */
  function isAccountPortAvailable(port: number, excludingAccountId?: string): boolean {
    if (!isValidAccountCdpPort(port)) return false
    return !assignedAccountPorts(excludingAccountId).has(port)
  }

  /**
   * Suggest the first free port at or above the configured global default.
   * Returns 0 if the upward range is exhausted; 0 is intentionally rejected by
   * both availability checks and `setAccountPort` (fail-closed; never reuse a
   * possibly occupied port or silently wrap to a lower one).
   */
  function suggestAccountPort(excludingAccountId?: string): number {
    const defaultPort = useQuestsStore().cdpPort
    const startPort = isValidAccountCdpPort(defaultPort)
      ? defaultPort
      : MIN_ACCOUNT_CDP_PORT
    const assigned = assignedAccountPorts(excludingAccountId)

    for (let port = startPort; port <= MAX_ACCOUNT_CDP_PORT; port += 1) {
      if (!assigned.has(port)) return port
    }

    return NO_AVAILABLE_ACCOUNT_PORT
  }

  /** Record and persist a port without changing which account is active. */
  function rememberAccountPort(accountId: string, port: number): boolean {
    if (!accountId || !isAccountPortAvailable(port, accountId)) return false
    accountPorts.value = { ...accountPorts.value, [accountId]: port }
    persistAccountPorts()
    return true
  }

  /** Record (and persist) an account's CDP port; keeps the active port in sync. */
  function setAccountPort(accountId: string, port: number): boolean {
    if (!rememberAccountPort(accountId, port)) return false
    const questsStore = useQuestsStore()
    if (activeAccountId.value === accountId) {
      questsStore.setActiveAccount(accountId, port)
    }
    return true
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
      let backendMutationCompleted = false
      try {
        invalidateClientAccountPreviews()
        const activeId = activeAccountId.value
        // A blocked active profile must be repaired in Account settings first.
        // Check before selecting a caller-supplied port or resetting account-local
        // state so an explicit option cannot bypass the persisted null block.
        if (activeId && accountPorts.value[activeId] === null) {
          error.value = t('auth.account_cdp_port_unassigned')
          return false
        }

        const questsStore = useQuestsStore()
        // Resolution order: explicit port > the account being captured (its saved
        // port, then its profile `lastCdpPort`) > the global default port.
        const resolvedPort =
          options?.port ?? (activeId ? portForAccount(activeId) : questsStore.cdpPort)

        // No blocked account remains in the login flow; clear prior attempt state.
        duplicateLoginAccountId.value = null
        resetProgramRewardState()

        // Refuse an explicitly selected port already owned by another profile
        // before asking the backend to publish a different active session.
        if (options?.port !== undefined && isValidAccountCdpPort(options.port) &&
          !isAccountPortAvailable(options.port, activeId ?? undefined)) {
          error.value = t('accounts.cdp_port_conflict')
          return false
        }

        invalidateAccountsLoads()
        const captured = await autoLoginViaCdp(resolvedPort, onProgress)
        backendMutationCompleted = true
        invalidateAccountsLoads()

        if (!isAccountPortAvailable(resolvedPort, captured.id)) {
          return await reconcileCapturedAccountPortConflict(
            captured,
            resolvedPort,
            questsStore,
            'CDP init after CDP login failed:'
          )
        }

        // Normal login/reauth always publishes and activates the captured account,
        // including a known offline account. The returned user is a safe fallback
        // if the authoritative list refresh is unavailable.
        if (!applyAuthenticatedUserFallback(captured, resolvedPort)) {
          return await reconcileCapturedAccountPortConflict(
            captured,
            resolvedPort,
            questsStore,
            'CDP init after CDP login failed:'
          )
        }
        await refreshAccountsAfterMutation('CDP login')

        if (accountPorts.value[captured.id] === null) {
          questsStore.cdpAvailable = true
          questsStore.gameQuestMode = 'cdp'
          bootstrapAfterLogin(questsStore, 'CDP init after CDP login failed:')
          error.value = t('accounts.cdp_port_conflict')
          return false
        }

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
        invalidateClientAccountPreviews()
        const questsStore = useQuestsStore()
        const resolvedPort = options?.port ?? questsStore.cdpPort

        // Add has no known target account yet, so an explicit port must be free
        // across every saved profile before starting capture.
        if (options?.port !== undefined && isValidAccountCdpPort(options.port) &&
          !isAccountPortAvailable(options.port)) {
          error.value = t('accounts.cdp_port_conflict')
          return false
        }

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

        if (!isAccountPortAvailable(resolvedPort, result.user.id)) {
          resetProgramRewardState()
          return await reconcileCapturedAccountPortConflict(
            result.user,
            resolvedPort,
            questsStore,
            'CDP init after CDP add-account failed:'
          )
        }

        // The Add command published this session. Apply a safe local fallback
        // immediately, then replace it with the authoritative account snapshot.
        resetProgramRewardState()
        if (!applyAuthenticatedUserFallback(result.user, resolvedPort)) {
          return await reconcileCapturedAccountPortConflict(
            result.user,
            resolvedPort,
            questsStore,
            'CDP init after CDP add-account failed:'
          )
        }
        await refreshAccountsAfterMutation('Add account')

        if (accountPorts.value[result.user.id] === null) {
          questsStore.cdpAvailable = true
          questsStore.gameQuestMode = 'cdp'
          bootstrapAfterLogin(questsStore, 'CDP init after CDP add-account failed:')
          error.value = t('accounts.cdp_port_conflict')
          return false
        }

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

  async function finishVerifiedClientAccount(
    captured: DiscordUser,
    port: number,
    actionLabel: string,
    cdpWarning: string,
  ): Promise<ClientAccountPortConflict | null> {
    const questsStore = useQuestsStore()
    resetProgramRewardState()

    if (
      !isAccountPortAvailable(port, captured.id) ||
      !applyAuthenticatedUserFallback(captured, port)
    ) {
      await reconcileCapturedAccountPortConflict(captured, port, questsStore, cdpWarning)
      return { status: 'portConflict', user: captured, port }
    }

    // The backend status is the verification boundary. Only after it succeeds do
    // we publish a local preference, then reconcile the authoritative profile list.
    await refreshAccountsAfterMutation(actionLabel)
    if (accountPorts.value[captured.id] === null) {
      questsStore.cdpAvailable = true
      questsStore.gameQuestMode = 'cdp'
      bootstrapAfterLogin(questsStore, cdpWarning)
      error.value = t('accounts.cdp_port_conflict')
      return { status: 'portConflict', user: captured, port }
    }

    questsStore.cdpAvailable = true
    questsStore.gameQuestMode = 'cdp'
    bootstrapAfterLogin(questsStore, cdpWarning)
    return null
  }

  /** Add only the identity explicitly previewed by the user. */
  async function confirmAddClientAccount(
    port: number,
    expectedUserId: string,
    onProgress?: AuthProgressHandler,
  ): Promise<ConfirmAddClientAccountResult> {
    return serializeAccountMutation(async () => {
      loading.value = true
      error.value = null
      invalidateClientAccountPreviews()
      let backendMutationCompleted = false
      try {
        invalidateAccountsLoads()
        const result = await confirmAddCdpAccountIpc(port, expectedUserId, onProgress)
        backendMutationCompleted = true
        invalidateAccountsLoads()

        if (result.status !== 'added') return result
        // The backend guarantees this for `added`; retain a frontend guard against
        // publishing a result that no longer matches the previewed identity.
        if (result.user.id !== expectedUserId) {
          return { ...result, status: 'identityChanged' }
        }

        return await finishVerifiedClientAccount(
          result.user,
          result.port,
          'Client-first Add',
          'CDP init after client-first Add failed:',
        ) ?? result
      } catch (cause) {
        error.value = cause instanceof Error ? cause.message : String(cause)
        throw cause
      } finally {
        if (!backendMutationCompleted) invalidateAccountsLoads()
        loading.value = false
      }
    })
  }

  /** Reconnect one selected CDP client to a specific saved account. */
  async function reconnectSavedAccount(
    accountId: string,
    port: number,
    onProgress?: AuthProgressHandler,
  ): Promise<ReconnectSavedAccountResult> {
    return serializeAccountMutation(async () => {
      loading.value = true
      error.value = null
      invalidateClientAccountPreviews()
      let backendMutationCompleted = false
      try {
        invalidateAccountsLoads()
        const result = await reconnectCdpAccountIpc(accountId, port, onProgress)
        backendMutationCompleted = true
        invalidateAccountsLoads()

        if (result.status !== 'reconnected' || result.user.id !== accountId) {
          return { ...result, status: 'identityChanged' }
        }

        return await finishVerifiedClientAccount(
          result.user,
          result.port,
          'Account reconnect',
          'CDP init after account reconnect failed:',
        ) ?? result
      } catch (cause) {
        error.value = cause instanceof Error ? cause.message : String(cause)
        throw cause
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

  /**
   * The backend has already published this account, but another profile claimed
   * its selected port during capture. Keep the session synchronized, never
   * persist the collision, and return failure so Add UI does not close as success.
   */
  async function reconcileCapturedAccountPortConflict(
    captured: DiscordUser,
    port: number,
    questsStore: ReturnType<typeof useQuestsStore>,
    cdpWarning: string,
  ): Promise<boolean> {
    // Persist the fail-closed state before any fallback projection or refresh.
    // This guarantees portForAccount returns 0 even if listAccounts fails.
    blockAccountPort(captured.id)
    applyAuthenticatedUserFallback(captured, port, false)
    let refreshFailure: string | null = null
    try {
      await loadAccountsAfterMutation()
    } catch (e) {
      refreshFailure = e instanceof Error ? e.message : String(e)
    }

    questsStore.cdpAvailable = true
    questsStore.gameQuestMode = 'cdp'
    bootstrapAfterLogin(questsStore, cdpWarning)

    const conflictMessage = t('accounts.cdp_port_conflict')
    error.value = refreshFailure
      ? `${conflictMessage} (account refresh failed: ${refreshFailure})`
      : conflictMessage
    return false
  }

  async function logout() {
    // Invalidate account-scoped requests before awaiting quest shutdown.
    resetProgramRewardState()
    invalidateClientAccountPreviews()

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

  function blockAccountPort(accountId: string) {
    if (!accountId) return
    accountPorts.value = { ...accountPorts.value, [accountId]: null }
    persistAccountPorts()
  }

  function forgetAccountPort(accountId: string) {
    if (!Object.prototype.hasOwnProperty.call(accountPorts.value, accountId)) return
    const updated = { ...accountPorts.value }
    delete updated[accountId]
    accountPorts.value = updated
    persistAccountPorts()
  }

  /** Prune orphaned overrides and fail closed on ports shared by saved profiles. */
  function reconcileStoredPortsForProfiles(profiles: AccountSummary[]) {
    const profileIds = new Set(profiles.map(profile => profile.id))
    const nextPorts: Record<string, number | null> = { ...accountPorts.value }
    let changed = false

    for (const accountId of Object.keys(nextPorts)) {
      if (!profileIds.has(accountId)) {
        delete nextPorts[accountId]
        changed = true
      }
    }

    const globalDefaultPort = useQuestsStore().cdpPort
    const ownersByPort = new Map<number, Array<{ id: string; explicit: boolean }>>()
    const reserve = (port: number, id: string, explicit: boolean) => {
      if (!isValidAccountCdpPort(port)) return
      const owners = ownersByPort.get(port) ?? []
      owners.push({ id, explicit })
      ownersByPort.set(port, owners)
    }

    for (const profile of profiles) {
      if (Object.prototype.hasOwnProperty.call(nextPorts, profile.id)) {
        const storedPort = nextPorts[profile.id]
        if (typeof storedPort === 'number') reserve(storedPort, profile.id, true)
        // `null` is an explicit block; do not fall through to profile/global.
        continue
      }

      const profilePort = profile.lastCdpPort
      if (profilePort) reserve(profilePort, profile.id, true)
      else reserve(globalDefaultPort, profile.id, false)
    }

    for (const owners of ownersByPort.values()) {
      if (owners.length < 2) continue
      const explicitOwners = owners.filter(owner => owner.explicit)
      const ownersToBlock = explicitOwners.length > 1 ? owners : owners.filter(owner => !owner.explicit)
      for (const owner of ownersToBlock) {
        if (nextPorts[owner.id] !== null) {
          nextPorts[owner.id] = null
          changed = true
        }
      }
    }

    if (changed) {
      accountPorts.value = nextPorts
      persistAccountPorts()
    }
  }

  function applyAccountsSnapshot(snapshot: AccountsSnapshot, reconcilePorts = true) {
    if (reconcilePorts) reconcileStoredPortsForProfiles(snapshot.accounts)
    accounts.value = snapshot.accounts
    const activeId = snapshot.activeAccountId ?? null
    if (activeId !== activeAccountId.value) invalidateClientAccountPreviews()
    activeAccountId.value = activeId
    syncUserProjection(snapshot.accounts.find(account => account.id === activeId))
    useQuestsStore().setActiveAccount(
      activeId,
      activeId ? portForAccount(activeId) : undefined
    )
  }

  function applyAuthenticatedUserFallback(
    captured: DiscordUser,
    port: number,
    recordPort = true,
  ): boolean {
    const portWasRecorded = recordPort && rememberAccountPort(captured.id, port)
    const existing = accounts.value.find(account => account.id === captured.id)
    const fallbackAccount: AccountSummary = {
      ...existing,
      id: captured.id,
      username: captured.username,
      discriminator: captured.discriminator,
      avatar: captured.avatar ?? undefined,
      globalName: captured.global_name ?? undefined,
      lastCdpPort: portWasRecorded
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
    }, false)
    return portWasRecorded
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
    }, false)
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
  function activateAccountMutation(
    accountId: string,
    onlineOnly: boolean,
  ): Promise<AccountSummary | null> {
    return serializeAccountMutation(async () => {
      error.value = null
      if (onlineOnly && !accounts.value.find(account => account.id === accountId)?.isAuthenticated) {
        return null
      }
      invalidateClientAccountPreviews()
      let backendMutationCompleted = false
      try {
        invalidateAccountsLoads()
        const account = await (onlineOnly
          ? activateOnlineAccountIpc(accountId)
          : activateAccountIpc(accountId))
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

  async function activateAccount(accountId: string): Promise<AccountSummary> {
    return (await activateAccountMutation(accountId, false))!
  }

  /** Online profiles may be switched; offline profiles must use confirmed reconnect. */
  async function switchOnlineAccount(accountId: string): Promise<AccountSummary | null> {
    return activateAccountMutation(accountId, true)
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
        invalidateClientAccountPreviews()
        invalidateAccountsLoads()
        const snapshot = await removeAccountIpc(accountId)
        backendMutationCompleted = true
        invalidateAccountsLoads()
        // Only a confirmed backend removal frees its locally persisted override.
        forgetAccountPort(accountId)
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
    suggestAccountPort,
    isAccountPortAvailable,
    previewClientAccount,
    invalidateClientAccountPreviews,
    confirmAddClientAccount,
    reconnectSavedAccount,
    nitroProgramReward,
    programRewardLoading,
    programRewardError,
    nextOrbsClaim,
    nitroStatus,
    loadAccounts,
    activateAccount,
    switchOnlineAccount,
    removeAccount,
    loginViaCdp,
    addAccountViaCdp,
    logout,
    fetchNitroProgramReward
  }
})
