import { computed, getCurrentScope, onScopeDispose, ref, watch } from 'vue'
import {
  addDesktopClientInstallation,
  getDesktopClientState,
  listRunningDesktopCdpSessions,
  removeDesktopClientInstallation,
  setDesktopClientSelection,
  type ClientInstallation,
  type ClientSelection,
  type CdpEndpointState,
  type DesktopClientArg,
  type DesktopClientState,
  type ProviderId,
  type RunningDesktopCdpSession,
  type SessionOwnership,
  type CdpIdentityPreview,
} from '@/api/tauri'
import { useAuthStore } from '@/stores/auth'
import { useQuestsStore } from '@/stores/quests'

export interface ClientAccountCandidate {
  id: string
  port: number
  providerId: ProviderId | null
  variantId: string | null
  installationId: string | null
  displayName: string
  ready: boolean
  endpointState: CdpEndpointState
  ownership: SessionOwnership | null
}

export const MAX_ACCOUNT_CLIENT_CANDIDATES = 32
export const MAX_CONCURRENT_ACCOUNT_CLIENT_PROBES = 4
const MAX_RUNNING_SESSION_PORTS = 24

const state = ref<DesktopClientState | null>(null)
const loading = ref(false)
const error = ref<string | null>(null)
let latestRequest = 0
const inFlightRefreshes = new Map<number, Promise<DesktopClientState | null>>()

function errorMessage(value: unknown): string {
  if (typeof value === 'object' && value && 'message' in value) return String(value.message)
  return value instanceof Error ? value.message : String(value)
}

async function apply(
  operation: () => Promise<DesktopClientState>,
  port: number,
): Promise<DesktopClientState> {
  const request = ++latestRequest
  loading.value = true
  error.value = null
  try {
    const snapshot = await operation()
    if (request === latestRequest && snapshot.port === port) state.value = snapshot
    return snapshot
  } catch (cause) {
    if (request === latestRequest) error.value = errorMessage(cause)
    throw cause
  } finally {
    if (request === latestRequest) loading.value = false
  }
}

async function refresh(port: number): Promise<DesktopClientState | null> {
  const existing = inFlightRefreshes.get(port)
  if (existing) return existing

  const request = apply(() => getDesktopClientState(port), port).catch(() => null)
  inFlightRefreshes.set(port, request)
  void request.finally(() => {
    if (inFlightRefreshes.get(port) === request) inFlightRefreshes.delete(port)
  })
  return request
}

async function select(selection: ClientSelection, port: number): Promise<DesktopClientState> {
  return apply(() => setDesktopClientSelection(selection, port), port)
}

async function addInstallation(providerId: ProviderId, path: string, port: number) {
  return apply(() => addDesktopClientInstallation(providerId, path, port), port)
}

async function removeInstallation(installationId: string, port: number) {
  return apply(() => removeDesktopClientInstallation(installationId, port), port)
}

async function migrateLegacySelection(port: number, legacy: DesktopClientArg) {
  const marker = 'questHelper_desktopClientMigratedV1'
  if (localStorage.getItem(marker) === 'true') return
  const snapshot = state.value?.port === port ? state.value : await refresh(port)
  if (!snapshot || snapshot.port !== port) return
  if (snapshot.selection.kind === 'auto' && legacy !== 'auto') {
    const providerId: ProviderId = legacy === 'vesktop' ? 'vencord.vesktop' : 'discord.official'
    await select({ kind: 'provider', providerId, variantId: null }, port)
  }
  localStorage.setItem(marker, 'true')
}

export function desktopClientArgForProvider(providerId: ProviderId): DesktopClientArg {
  if (providerId === 'vencord.vesktop') return 'vesktop'
  if (providerId === 'discord.official') return 'official'
  return 'auto'
}

function installationPath(installation: ClientInstallation | null | undefined): string | undefined {
  if (!installation) return undefined
  if (installation.launchTarget.kind === 'executable') return installation.launchTarget.path
  if (installation.launchTarget.kind === 'macBundle') return installation.launchTarget.executablePath
  return undefined
}

function providerForSelection(selection: ClientSelection | null | undefined): ProviderId | null {
  if (!selection || selection.kind === 'auto') return null
  if (selection.kind === 'provider') return selection.providerId
  return state.value?.installations.find(item => item.id === selection.installationId)?.providerId ?? null
}

export function useDesktopClientState() {
  const authStore = useAuthStore()
  const questsStore = useQuestsStore()
  const accountCandidates = ref<ClientAccountCandidate[]>([])
  const accountCandidatesLoading = ref(false)
  const accountCandidatesError = ref<string | null>(null)
  const selectedAccountCandidateId = ref<string | null>(null)
  const selectedClientAccountPreview = ref<CdpIdentityPreview | null>(null)
  const accountPreviewLoading = ref(false)
  const accountPreviewError = ref<string | null>(null)
  let accountCandidateScanRevision = 0
  let accountPreviewRevision = 0

  function cancelAccountClientSelection() {
    accountCandidateScanRevision += 1
    accountPreviewRevision += 1
    authStore.invalidateClientAccountPreviews()
    selectedAccountCandidateId.value = null
    selectedClientAccountPreview.value = null
    accountCandidatesLoading.value = false
    accountPreviewLoading.value = false
    accountCandidatesError.value = null
    accountPreviewError.value = null
  }

  const stopActiveAccountWatch = watch(
    () => authStore.activeAccountId,
    () => {
      cancelAccountClientSelection()
      accountCandidates.value = []
    },
  )
  if (getCurrentScope()) onScopeDispose(stopActiveAccountWatch)

  function accountClientPorts(
    runningSessions: RunningDesktopCdpSession[],
    activeAccountId: string | null,
  ): number[] {
    const isValidPort = (port: number | null | undefined): port is number =>
      typeof port === 'number' && Number.isInteger(port) && port >= 1024 && port <= 65535
    const currentPorts = [
      activeAccountId ? authStore.portForAccount(activeAccountId) : undefined,
      questsStore.activeCdpPort,
      questsStore.cdpPort,
    ].filter(isValidPort)
    const commonPorts = [9223, 9224, 9225, 9226]
    const rankedSessions = runningSessions
      .filter(session => isValidPort(session.port))
      .slice()
      .sort((left, right) => {
        const priority = (port: number) => currentPorts.includes(port) ? 0 : commonPorts.includes(port) ? 1 : 2
        return priority(left.port) - priority(right.port) ||
          left.port - right.port ||
          left.providerId.localeCompare(right.providerId) ||
          (left.installationId ?? '').localeCompare(right.installationId ?? '')
      })

    const ports = new Set<number>()
    const add = (port: number | null | undefined) => {
      if (isValidPort(port) && ports.size < MAX_ACCOUNT_CLIENT_CANDIDATES) ports.add(port)
    }

    // Reserve room for the selected/default/common probes, while still taking
    // observed live sessions before saved-account preference ports.
    for (const session of rankedSessions) {
      if (ports.size >= MAX_RUNNING_SESSION_PORTS) break
      add(session.port)
    }
    for (const port of currentPorts) add(port)
    for (const port of commonPorts) add(port)

    const savedPreferences = [
      ...Object.entries(authStore.accountPorts)
        .filter((entry): entry is [string, number] => isValidPort(entry[1]))
        .sort(([left], [right]) => left.localeCompare(right))
        .map(([, port]) => port),
      ...[...authStore.accounts]
        .sort((left, right) => left.id.localeCompare(right.id))
        .map(account => account.lastCdpPort)
        .filter(isValidPort),
    ]
    for (const port of savedPreferences) add(port)

    return [...ports]
  }

  async function scanAccountClients(): Promise<ClientAccountCandidate[]> {
    const requestRevision = ++accountCandidateScanRevision
    accountPreviewRevision += 1
    authStore.invalidateClientAccountPreviews()
    selectedAccountCandidateId.value = null
    selectedClientAccountPreview.value = null
    accountPreviewLoading.value = false
    accountPreviewError.value = null
    accountCandidatesLoading.value = true
    accountCandidatesError.value = null
    const startingActiveAccountId = authStore.activeAccountId
    let runningSessions: RunningDesktopCdpSession[] = []
    let scanError: string | null = null

    try {
      runningSessions = await listRunningDesktopCdpSessions()
    } catch (cause) {
      scanError = errorMessage(cause)
    }
    if (
      requestRevision !== accountCandidateScanRevision ||
      startingActiveAccountId !== authStore.activeAccountId
    ) {
      if (requestRevision === accountCandidateScanRevision) accountCandidatesLoading.value = false
      return accountCandidates.value
    }
    const ports = accountClientPorts(runningSessions, startingActiveAccountId)
    const snapshots: Array<{ port: number; state: DesktopClientState | null }> = new Array(ports.length)
    let nextProbeIndex = 0
    const probeWorker = async () => {
      while (true) {
        const index = nextProbeIndex++
        if (index >= ports.length) return
        const port = ports[index]!
        try {
          snapshots[index] = { port, state: await getDesktopClientState(port) }
        } catch {
          snapshots[index] = { port, state: null }
        }
      }
    }
    const workerCount = Math.min(MAX_CONCURRENT_ACCOUNT_CLIENT_PROBES, ports.length)
    await Promise.all(Array.from({ length: workerCount }, () => probeWorker()))

    if (
      requestRevision !== accountCandidateScanRevision ||
      startingActiveAccountId !== authStore.activeAccountId
    ) {
      if (requestRevision === accountCandidateScanRevision) accountCandidatesLoading.value = false
      return accountCandidates.value
    }

    const stateByPort = new Map(snapshots.map(snapshot => [snapshot.port, snapshot.state]))
    const probedPorts = new Set(ports)
    const candidatesById = new Map<string, ClientAccountCandidate>()
    const displayNameFor = (
      providerId: ProviderId | null,
      variantId: string | null,
      installation: ClientInstallation | undefined,
    ) => installation?.displayName ?? (
      providerId === 'vencord.vesktop'
        ? 'Vesktop'
        : providerId === 'discord.official'
          ? `Discord ${variantId ?? 'client'}`
          : providerId ?? 'Discord client'
    )

    const addCandidate = (
      port: number,
      descriptor: {
        providerId: ProviderId | null
        variantId: string | null
        installationId: string | null
        ownership: SessionOwnership | null
      },
    ) => {
      if (!probedPorts.has(port) || candidatesById.size >= MAX_ACCOUNT_CLIENT_CANDIDATES) return
      const desktopState = stateByPort.get(port)
      const installation = descriptor.installationId
        ? desktopState?.installations.find(item => item.id === descriptor.installationId)
        : desktopState?.installations.find(item =>
            item.providerId === descriptor.providerId &&
            (descriptor.variantId === null || item.variantId === descriptor.variantId))
      const providerId = descriptor.providerId ?? installation?.providerId ?? null
      const variantId = descriptor.variantId ?? installation?.variantId ?? null
      const installationId = descriptor.installationId ?? installation?.id ?? null
      const id = `${port}:${providerId ?? 'client'}:${variantId ?? ''}:${installationId ?? ''}`
      candidatesById.set(id, {
        id,
        port,
        providerId,
        variantId,
        installationId,
        displayName: displayNameFor(providerId, variantId, installation),
        ready: desktopState?.endpoint.status === 'discordReady',
        endpointState: desktopState?.endpoint.status ?? 'unreachable',
        ownership: descriptor.ownership,
      })
    }

    const probedSessions = runningSessions
      .filter(session => probedPorts.has(session.port))
      .sort((left, right) => left.port - right.port || left.providerId.localeCompare(right.providerId))
    for (const session of probedSessions) {
      addCandidate(session.port, {
        providerId: session.providerId,
        variantId: session.variantId,
        installationId: session.installationId,
        ownership: session.ownership,
      })
    }

    for (const { port, state: desktopState } of snapshots) {
      if (!desktopState) continue
      for (const process of desktopState.processes) {
        if (!process.running) continue
        addCandidate(port, {
          providerId: process.providerId,
          variantId: process.variantId,
          installationId: process.installationId,
          ownership: null,
        })
      }

      // Some ready endpoints have no inventory/process row. Use only structured
      // selection/owner metadata; never infer an account or provider from titles.
      const hasCandidateAtPort = [...candidatesById.values()].some(item => item.port === port)
      if (!hasCandidateAtPort && desktopState.endpoint.status === 'discordReady') {
        const selection = desktopState.selection
        const installation = selection.kind === 'installation'
          ? desktopState.installations.find(item => item.id === selection.installationId)
          : selection.kind === 'provider'
            ? desktopState.installations.find(item => item.providerId === selection.providerId &&
                (selection.variantId == null || item.variantId === selection.variantId))
            : undefined
        const providerId = selection.kind === 'provider'
          ? selection.providerId
          : installation?.providerId ?? desktopState.endpoint.ownerProviderId
        const variantId = selection.kind === 'provider'
          ? selection.variantId ?? null
          : installation?.variantId ?? null
        addCandidate(port, {
          providerId,
          variantId,
          installationId: installation?.id ?? null,
          ownership: null,
        })
      }
    }

    if (
      requestRevision !== accountCandidateScanRevision ||
      startingActiveAccountId !== authStore.activeAccountId
    ) {
      if (requestRevision === accountCandidateScanRevision) accountCandidatesLoading.value = false
      return accountCandidates.value
    }

    accountCandidates.value = [...candidatesById.values()].sort((left, right) =>
      left.port - right.port || left.displayName.localeCompare(right.displayName))
    accountCandidatesError.value = scanError
    accountCandidatesLoading.value = false
    return accountCandidates.value
  }

  async function selectAccountClientCandidate(candidateId: string): Promise<CdpIdentityPreview | null> {
    const candidate = accountCandidates.value.find(item => item.id === candidateId)
    accountCandidateScanRevision += 1
    accountCandidatesLoading.value = false
    const requestRevision = ++accountPreviewRevision
    authStore.invalidateClientAccountPreviews()
    selectedAccountCandidateId.value = candidateId
    selectedClientAccountPreview.value = null
    accountPreviewLoading.value = false
    accountPreviewError.value = null
    if (!candidate) {
      accountPreviewError.value = 'The selected client is no longer available.'
      return null
    }
    if (!candidate.ready) {
      accountPreviewError.value = 'The selected client is not ready for identity verification.'
      return null
    }

    const startingActiveAccountId = authStore.activeAccountId
    accountPreviewLoading.value = true
    try {
      const preview = await authStore.previewClientAccount(candidate.port)
      if (
        requestRevision !== accountPreviewRevision ||
        selectedAccountCandidateId.value !== candidateId ||
        startingActiveAccountId !== authStore.activeAccountId
      ) return null
      if (!preview) return null
      if (preview.port !== candidate.port) {
        accountPreviewError.value = 'Identity preview came from a different client port.'
        return null
      }
      selectedClientAccountPreview.value = preview
      return preview
    } catch (cause) {
      if (requestRevision === accountPreviewRevision) accountPreviewError.value = errorMessage(cause)
      return null
    } finally {
      if (requestRevision === accountPreviewRevision) accountPreviewLoading.value = false
    }
  }

  const selectedInstallation = computed(() => {
    const selection = state.value?.selection
    if (selection?.kind !== 'installation') return null
    return state.value?.installations.find(item => item.id === selection.installationId) ?? null
  })
  const selectedProviderId = computed(() => providerForSelection(state.value?.selection))
  const selectedIsRunning = computed(() => {
    const snapshot = state.value
    if (!snapshot) return false
    const selection = snapshot.selection
    if (selection.kind === 'installation') {
      return snapshot.processes.some(process => process.installationId === selection.installationId)
    }
    if (selection.kind === 'provider') {
      return snapshot.processes.some(process => process.providerId === selection.providerId)
    }
    return snapshot.processes.length > 0
  })
  return {
    state,
    loading,
    error,
    selectedInstallation,
    selectedProviderId,
    selectedIsRunning,
    refresh,
    select,
    addInstallation,
    removeInstallation,
    migrateLegacySelection,
    installationPath,
    accountCandidates,
    accountCandidatesLoading,
    accountCandidatesError,
    selectedAccountCandidateId,
    selectedClientAccountPreview,
    accountPreviewLoading,
    accountPreviewError,
    scanAccountClients,
    selectAccountClientCandidate,
    cancelAccountClientSelection,
  }
}
