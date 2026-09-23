<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import {
  AlertCircle,
  AppWindow,
  Check,
  ChevronRight,
  Loader2,
  Monitor,
  RadioTower,
  Search,
  AlertTriangle,
} from 'lucide-vue-next'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from '@/components/ui/alert-dialog'
import { useAuthStore } from '@/stores/auth'
import { useQuestsStore } from '@/stores/quests'
import {
  listRunningDesktopCdpSessions,
  launchDesktopClientCdp,
  checkCdpStatus,
  type AuthProgress,
  type ClientSelection,
  type CdpStatus,
  type DesktopClientInventory,
  type DesktopClientState,
} from '@/api/tauri'
import { desktopClientArgForProvider, useDesktopClientState } from '@/composables/desktopClientState'
import {
  classifyCdpAvailability,
  canBeginLogin,
  installedCdpLaunchTargets,
  presentAuthProgress,
  shouldAskCdpLaunchTarget,
  shouldPollCdp,
  selectionForCdpLaunchTarget,
  findCurrentCdpOwnerSession,
  selectionForCurrentCdpOwner,
  startCdpPolling,
  usesVesktopForCdpLogin,
  hasUnchanneledOfficialMacInstallation,
  type CdpLaunchTarget,
  type LoginMethod,
  type LoginProgressState,
} from './loginFlow'
import { toErrorMessage } from '@/utils/errorMessage'

const props = withDefaults(defineProps<{
  allowPortSelection?: boolean
}>(), {
  allowPortSelection: false
})

const emit = defineEmits<{
  navigateToHome: []
}>()

const CDP_POLL_INTERVAL_MS = 5_000
const CDP_CANDIDATE_PORTS = [9223, 9224, 9225, 9226]

const { t } = useI18n()
const authStore = useAuthStore()
const questsStore = useQuestsStore()
const clients = useDesktopClientState()

const activeMethod = ref<LoginMethod | null>(null)
const progress = ref<{
  method: LoginMethod
  state: LoginProgressState
  key: string
  params?: Record<string, number>
  detail?: string
} | null>(null)

const cdpStatus = ref<CdpStatus | null>(null)
const cdpChecking = ref(false)
const cdpProbeFailed = ref(false)
const cdpRestartDialogOpen = ref(false)
const cdpChooseDialogOpen = ref(false)
const cdpLaunchChoices = ref<CdpLaunchTarget[]>([])
const selectedCdpTarget = ref<CdpLaunchTarget | null>(null)
const rememberCdpChoice = ref(false)
const desktopClients = ref<DesktopClientInventory | null>(null)
const ownerConflict = ref(false)
let stopCdpPolling: (() => void) | null = null

// Port selection state (Phase 6.5)
const selectedPort = ref<number>(questsStore.cdpPort)
const detectingPorts = ref(false)
const detectedPorts = ref<Map<number, boolean>>(new Map())
const portError = ref<string | null>(null)
// Port-detection and status-probe generations are deliberately separate: a
// concurrent `detectCdpPorts` run must never cancel an in-flight CDP status
// probe (and vice versa), otherwise `onMounted`'s parallel detection would
// discard the ready status response.
let statusGeneration = 0
let detectGeneration = 0

// Computed login port. Add mode always uses the selected port; standalone mode
// honors the selected saved account's own port (saved override / lastCdpPort)
// and only falls back to the global default when no account profile is selected.
const loginPort = computed(() => {
  if (props.allowPortSelection) return selectedPort.value
  const activeId = authStore.activeAccountId
  if (activeId) return authStore.portForAccount(activeId)
  return questsStore.cdpPort
})

// Port validation function
function validatePort(port: number): string | null {
  if (!Number.isInteger(port)) {
    return t('auth.port_invalid_integer')
  }
  if (port < 1024 || port > 65535) {
    return t('auth.port_invalid_range')
  }
  return null
}

const busy = computed(() => (
  activeMethod.value !== null
  || authStore.loading
  || cdpRestartDialogOpen.value
  || cdpChooseDialogOpen.value
))
const cdpAvailability = computed(() => classifyCdpAvailability(
  cdpChecking.value,
  cdpStatus.value,
  cdpProbeFailed.value,
))
const cdpStatusKey = computed(() => ({
  checking: 'settings.cdp_checking',
  ready: 'settings.cdp_connected',
  starting: 'auth.cdp_status_starting',
  offline: 'settings.cdp_disconnected_short',
  error: 'auth.cdp_status_error',
})[cdpAvailability.value])
const usingVesktopCdp = computed(() => (
  usesVesktopForCdpLogin(desktopClients.value)
))
const restartUsesVesktop = computed(() => (
  selectedCdpTarget.value === 'vesktop'
  || usingVesktopCdp.value
))
const cdpLoginDetailKey = computed(() => (
  usingVesktopCdp.value ? 'auth.cdp_login_detail_vesktop' : 'auth.cdp_login_detail'
))
const cdpLoginActionKey = computed(() => (
  usingVesktopCdp.value ? 'auth.cdp_login_action_vesktop' : 'auth.cdp_login_action'
))
const cdpStatusClass = computed(() => ({
  checking: 'bg-muted text-muted-foreground',
  ready: 'bg-emerald-500/10 text-emerald-700 dark:text-emerald-300',
  starting: 'bg-amber-500/10 text-amber-700 dark:text-amber-300',
  offline: 'bg-muted text-muted-foreground',
  error: 'bg-destructive/10 text-destructive',
})[cdpAvailability.value])
const progressText = computed(() => {
  if (!progress.value) return ''
  const message = t(progress.value.key, progress.value.params ?? {})
  return progress.value.detail ? `${message}: ${progress.value.detail}` : message
})

function setProgress(
  method: LoginMethod,
  state: LoginProgressState,
  key: string,
  params?: Record<string, number>,
  detail?: string,
) {
  progress.value = { method, state, key, params, detail }
}

function handleBackendProgress(method: LoginMethod, event: AuthProgress) {
  const presentation = presentAuthProgress(event)
  setProgress(method, presentation.state, presentation.key, presentation.params)
}

function errorDetail(error: unknown): string {
  return toErrorMessage(error)
}

function begin(method: LoginMethod): boolean {
  if (!canBeginLogin(activeMethod.value, authStore.loading)) return false
  activeMethod.value = method
  authStore.error = null
  return true
}

function finish() {
  activeMethod.value = null
}

async function refreshDesktopClients() {
  const port = loginPort.value
  const snapshot = await clients.refresh(port)
  // A late scan for a previous port must not replace the current port's inventory.
  if (snapshot && port === loginPort.value) desktopClients.value = inventoryFromState(snapshot)
}

interface CdpProbeResult {
  status: CdpStatus
  snapshot: DesktopClientState
}

async function refreshCdpStatus(): Promise<CdpProbeResult | null> {
  // Snapshot the port this probe targets before any await, and take a
  // status-only generation. The probe is never refused while another is in
  // flight: a newer probe simply supersedes the older one, so selecting a new
  // port always launches a probe for that port.
  const port = loginPort.value
  const currentGeneration = ++statusGeneration
  cdpChecking.value = true
  cdpProbeFailed.value = false
  try {
    const snapshot = await clients.refresh(port)
    // Discard a superseded response, or one for a port the user has since
    // navigated away from, so a late old status can never overwrite a new port.
    if (currentGeneration !== statusGeneration || port !== loginPort.value) return null
    if (!snapshot || snapshot.port !== port) {
      throw new Error(clients.error.value ?? 'Desktop client state is unavailable')
    }
    desktopClients.value = inventoryFromState(snapshot)
    const ready = snapshot.endpoint.status === 'discordReady'
    const status: CdpStatus = {
      available: ready,
      connected: ready,
      target_title: snapshot.endpoint.targetTitle,
      error: ready ? null : snapshot.endpoint.status,
    }
    cdpStatus.value = status
    // Don't overwrite global cdpAvailable when in add mode
    if (!props.allowPortSelection) {
      questsStore.cdpAvailable = status.connected
    }
    return { status, snapshot }
  } catch (error) {
    // Guard against late responses
    if (currentGeneration !== statusGeneration || port !== loginPort.value) return null
    cdpProbeFailed.value = true
    cdpStatus.value = null
    if (!props.allowPortSelection) {
      questsStore.cdpAvailable = false
    }
    console.warn('Login page CDP probe failed:', error)
    return null
  } finally {
    // Only the newest probe owns the shared checking flag.
    if (currentGeneration === statusGeneration) {
      cdpChecking.value = false
    }
  }
}

function inventoryFromState(snapshot: DesktopClientState): DesktopClientInventory {
  const installed = (providerId: string, variantId?: string) => snapshot.installations.some(item => (
    item.providerId === providerId
    && item.validation === 'valid'
    && (!variantId || item.variantId === variantId)
  ))
  const running = (providerId: string, variantId?: string) => snapshot.processes.some(item => (
    item.providerId === providerId && (!variantId || item.variantId === variantId)
  ))
  const customOfficialMacInstalled = hasUnchanneledOfficialMacInstallation(snapshot.installations)
  return {
    officialInstalled: installed('discord.official'),
    vesktopInstalled: installed('vencord.vesktop'),
    officialRunning: running('discord.official'),
    vesktopRunning: running('vencord.vesktop'),
    cdpOwner: snapshot.endpoint.owner,
    stableInstalled: installed('discord.official', 'stable') || customOfficialMacInstalled,
    ptbInstalled: installed('discord.official', 'ptb'),
    canaryInstalled: installed('discord.official', 'canary'),
    stableRunning: running('discord.official', 'stable'),
    ptbRunning: running('discord.official', 'ptb'),
    canaryRunning: running('discord.official', 'canary'),
  }
}

function selectionForTarget(snapshot: DesktopClientState, target: CdpLaunchTarget | null): ClientSelection {
  // Derive the installation/provider strictly from the snapshot requested for
  // the login port — never from the shared `clients.state`, which may belong to
  // a concurrent scan on a different port.
  return selectionForCdpLaunchTarget(snapshot, target)
}

function selectionIsRunning(snapshot: DesktopClientState, selection: ClientSelection): boolean {
  if (selection.kind === 'installation') {
    return snapshot.processes.some(process => process.installationId === selection.installationId)
  }
  if (selection.kind === 'provider') {
    return snapshot.processes.some(process => (
      process.providerId === selection.providerId
      && (!selection.variantId || process.variantId === selection.variantId)
    ))
  }
  return snapshot.processes.length > 0
}

function selectionProvider(snapshot: DesktopClientState, selection: ClientSelection): string | null {
  if (selection.kind === 'provider') return selection.providerId
  if (selection.kind === 'installation') {
    return snapshot.installations.find(item => item.id === selection.installationId)?.providerId ?? null
  }
  return null
}

function syncLegacyDesktopClientPreference(selection: ClientSelection) {
  if (selection.kind !== 'provider') {
    questsStore.desktopClient = 'auto'
    return
  }
  questsStore.desktopClient = desktopClientArgForProvider(selection.providerId)
}

async function finishCdpLogin() {
  const port = loginPort.value
  const onProgress = (event: AuthProgress) => handleBackendProgress('cdp', event)
  const succeeded = props.allowPortSelection
    ? await authStore.addAccountViaCdp(onProgress, { port })
    : await authStore.loginViaCdp(onProgress, { port })
  if (!succeeded) {
    setProgress('cdp', 'error', 'auth.progress.failed', undefined, authStore.error ?? undefined)
    return false
  }

  // Add-only CDP reports duplicates from the backend. The completion event may
  // already have shown success, so replace it with a neutral duplicate status
  // and leave the modal open for the user's next action.
  if (props.allowPortSelection && authStore.duplicateLoginAccountId) {
    setProgress('cdp', 'neutral', 'auth.duplicate_account_title')
    return false
  }

  // In add mode, emit success to close the modal
  if (props.allowPortSelection) {
    emit('navigateToHome')
  }

  return true
}

function requestCdpRestart(target: CdpLaunchTarget | null) {
  selectedCdpTarget.value = target
  setProgress(
    'cdp',
    'waiting',
    target === 'vesktop' ? 'auth.progress.restart_required_vesktop' : 'auth.progress.restart_required',
  )
  cdpRestartDialogOpen.value = true
}

async function launchOrRestartSelectedTarget(target: CdpLaunchTarget | null) {
  selectedCdpTarget.value = target
  const snapshot = await clients.refresh(loginPort.value)
  if (!snapshot) throw new Error(clients.error.value ?? 'Desktop client state is unavailable')
  const selection = selectionForTarget(snapshot, target)
  if (selectionIsRunning(snapshot, selection)) {
    requestCdpRestart(target)
    return
  }

  setProgress(
    'cdp',
    'running',
    target === 'vesktop' ? 'auth.progress.launching_vesktop' : 'auth.progress.launching_discord',
  )
  try {
    await launchDesktopClientCdp(loginPort.value, selection, false)
    await refreshCdpStatus()
  } catch (launchError) {
    const latest = await clients.refresh(loginPort.value)
    if (!latest) throw launchError
    if (latest.endpoint.status !== 'discordReady') {
      if (selectionIsRunning(latest, selection)) {
        requestCdpRestart(target)
        return
      }
      throw launchError
    }
    const provider = selectionProvider(latest, selection)
    if (provider && latest.endpoint.ownerProviderId !== provider) {
      ownerConflict.value = true
      requestCdpRestart(target)
      return
    }
  }
  await finishCdpLogin()
}

async function handleCdpLogin() {
  // Validate port in add mode
  if (props.allowPortSelection) {
    const validationError = validatePort(selectedPort.value)
    if (validationError) {
      portError.value = validationError
      return
    }
    portError.value = null
  }

  if (!begin('cdp')) return
  setProgress('cdp', 'running', 'auth.progress.checking_cdp')
  try {
    const probe = await refreshCdpStatus()
    const status = probe?.status ?? null
    // Use the exact snapshot the probe returned for the selected port, never the
    // shared store: a concurrent scan on another port must not decide readiness
    // or which installation/provider this login targets.
    const currentSnapshot = probe?.snapshot ?? null
    if (status?.connected && currentSnapshot) {
      const provider = selectionProvider(currentSnapshot, currentSnapshot.selection)
      if (provider && currentSnapshot.endpoint.ownerProviderId !== provider) {
        ownerConflict.value = true
        selectedCdpTarget.value = null
        setProgress('cdp', 'waiting', 'auth.progress.choose_client')
        cdpRestartDialogOpen.value = true
        return
      }
      await finishCdpLogin()
      return
    }
    if (currentSnapshot?.selection.kind !== 'auto') {
      await launchOrRestartSelectedTarget(null)
      return
    }
    const targets = installedCdpLaunchTargets(desktopClients.value)
    if (shouldAskCdpLaunchTarget(false, targets)) {
      cdpLaunchChoices.value = targets
      selectedCdpTarget.value = null
      rememberCdpChoice.value = false
      setProgress('cdp', 'waiting', 'auth.progress.choose_client')
      cdpChooseDialogOpen.value = true
      return
    }
    await launchOrRestartSelectedTarget(targets[0] ?? null)
  } catch (error) {
    authStore.error = errorDetail(error)
    setProgress('cdp', 'error', 'auth.progress.failed', undefined, authStore.error)
  } finally {
    finish()
    if (!authStore.user && !cdpRestartDialogOpen.value && !cdpChooseDialogOpen.value) {
      void refreshCdpStatus()
    }
  }
}

async function selectCdpLaunchTarget(target: CdpLaunchTarget) {
  const shouldRemember = rememberCdpChoice.value
  setProgress(
    'cdp',
    'running',
    target === 'vesktop' ? 'auth.progress.launching_vesktop' : 'auth.progress.launching_discord',
  )
  cdpChooseDialogOpen.value = false
  if (!begin('cdp')) return
  try {
    const snapshot = await clients.refresh(loginPort.value)
    if (!snapshot) throw new Error(clients.error.value ?? 'Desktop client state is unavailable')
    const selected = selectionForTarget(snapshot, target)
    const persisted = shouldRemember ? selected : { kind: 'auto' as const }
    await clients.select(persisted, loginPort.value)
    syncLegacyDesktopClientPreference(persisted)
    await launchOrRestartSelectedTarget(target)
  } catch (error) {
    authStore.error = errorDetail(error)
    setProgress('cdp', 'error', 'auth.progress.failed', undefined, authStore.error)
  } finally {
    finish()
    if (!authStore.user && !cdpRestartDialogOpen.value) void refreshCdpStatus()
  }
}

async function confirmCdpRestart() {
  cdpRestartDialogOpen.value = false
  if (!begin('cdp')) return
  setProgress(
    'cdp',
    'running',
    selectedCdpTarget.value === 'vesktop'
      ? 'auth.progress.restarting_vesktop'
      : 'auth.progress.restarting_discord',
  )
  try {
    const snapshot = await clients.refresh(loginPort.value)
    if (!snapshot) throw new Error(clients.error.value ?? 'Desktop client state is unavailable')
    await launchDesktopClientCdp(
      loginPort.value,
      selectionForTarget(snapshot, selectedCdpTarget.value),
      true,
    )
    ownerConflict.value = false
    await refreshCdpStatus()
    await finishCdpLogin()
  } catch (error) {
    authStore.error = errorDetail(error)
    setProgress('cdp', 'error', 'auth.progress.failed', undefined, authStore.error)
  } finally {
    finish()
    if (!authStore.user) void refreshCdpStatus()
  }
}

async function useCurrentCdpOwner() {
  try {
    const snapshot = await clients.refresh(loginPort.value)
    const providerId = snapshot?.endpoint.ownerProviderId
    if (!snapshot || !providerId) return
    const ownerSession = findCurrentCdpOwnerSession(
      await listRunningDesktopCdpSessions(),
      loginPort.value,
      providerId,
    )
    if (!ownerSession) throw new Error('The current CDP owner could not be mapped to one exact installation.')
    const selection = selectionForCurrentCdpOwner(snapshot, ownerSession)
    await clients.select(selection, loginPort.value)
    syncLegacyDesktopClientPreference(selection)
    ownerConflict.value = false
    cdpRestartDialogOpen.value = false
    if (!begin('cdp')) return
    await finishCdpLogin()
  } catch (error) {
    authStore.error = errorDetail(error)
    setProgress('cdp', 'error', 'auth.progress.failed', undefined, authStore.error)
  } finally {
    finish()
  }
}

function handleRestartDialogOpenChange(open: boolean) {
  cdpRestartDialogOpen.value = open
  if (!open && progress.value?.state === 'waiting') {
    setProgress('cdp', 'neutral', 'auth.progress.restart_cancelled')
    void refreshCdpStatus()
  }
}

function handleChooseDialogOpenChange(open: boolean) {
  cdpChooseDialogOpen.value = open
  if (open) {
    rememberCdpChoice.value = false
    void refreshDesktopClients()
  }
  if (!open && progress.value?.key === 'auth.progress.choose_client') {
    setProgress('cdp', 'neutral', 'auth.progress.launch_cancelled')
    void refreshCdpStatus()
  }
}

function pollCdpIfNeeded() {
  // In add-account mode the user is verifying a DIFFERENT port while an existing
  // account may remain signed in, so the signed-in gate must not stop probing.
  // The standalone path keeps its original authenticated gate.
  const authenticated = props.allowPortSelection ? false : Boolean(authStore.user)
  if (shouldPollCdp({
    busy: busy.value,
    authenticated,
    visible: document.visibilityState === 'visible',
  })) {
    void refreshCdpStatus()
  }
}

function handleVisibilityChange() {
  if (document.visibilityState === 'visible') pollCdpIfNeeded()
}

function getDuplicateAccountName(): string {
  if (!authStore.duplicateLoginAccountId) return ''
  const account = authStore.accounts.find(a => a.id === authStore.duplicateLoginAccountId)
  return account?.globalName || account?.username || authStore.duplicateLoginAccountId
}

// Port detection for Phase 6.5
async function detectCdpPorts() {
  if (detectingPorts.value) return

  const currentGeneration = ++detectGeneration
  detectingPorts.value = true
  detectedPorts.value.clear()

  try {
    const results = await Promise.all(
      CDP_CANDIDATE_PORTS.map(async (port) => {
        try {
          const status = await checkCdpStatus(port)
          return { port, available: status.available }
        } catch {
          return { port, available: false }
        }
      })
    )

    // Guard against late responses
    if (currentGeneration !== detectGeneration) return

    const newMap = new Map<number, boolean>()
    for (const result of results) {
      newMap.set(result.port, result.available)
    }
    detectedPorts.value = newMap
  } finally {
    if (currentGeneration === detectGeneration) detectingPorts.value = false
  }
}

function isPortDetected(port: number): boolean {
  return detectedPorts.value.get(port) === true
}

function selectDetectedPort(port: number) {
  if (isPortDetected(port)) {
    selectedPort.value = port
    portError.value = null
  }
}

onMounted(() => {
  pollCdpIfNeeded()
  void refreshDesktopClients()
  void clients.migrateLegacySelection(loginPort.value, questsStore.desktopClient)
  stopCdpPolling = startCdpPolling(pollCdpIfNeeded, CDP_POLL_INTERVAL_MS)
  document.addEventListener('visibilitychange', handleVisibilityChange)

  // Auto-detect ports when port selection is allowed
  if (props.allowPortSelection) {
    void detectCdpPorts()
  }
})

onUnmounted(() => {
  stopCdpPolling?.()
  document.removeEventListener('visibilitychange', handleVisibilityChange)
})

watch(() => questsStore.cdpPort, () => {
  // Update selectedPort when global port changes (only in add mode)
  if (props.allowPortSelection) {
    selectedPort.value = questsStore.cdpPort
  }
  void refreshCdpStatus()
})

// Refresh status when selectedPort changes in add mode
watch(() => selectedPort.value, () => {
  if (props.allowPortSelection) {
    portError.value = null
    void refreshCdpStatus()
  }
})
</script>

<template>
  <section
    class="login-panel-stage mx-auto my-auto flex w-full max-w-2xl flex-col gap-6 py-4 sm:py-8"
    aria-labelledby="login-heading"
  >
    <div class="flex justify-center">
      <slot name="toolbar" />
    </div>

    <div class="login-brand-stage flex justify-center py-2 sm:py-4">
      <div class="flex items-center gap-3 sm:gap-4">
        <img src="/icons/logo.png" :alt="t('general.title')" class="h-12 w-12 select-none sm:h-14 sm:w-14" />
        <div>
          <h2 id="login-heading" class="text-2xl font-semibold tracking-tight sm:text-3xl">
            {{ t('general.welcome') }}
          </h2>
          <p class="mt-1 max-w-xl text-sm leading-6 text-muted-foreground sm:text-base">
            {{ t('general.login_prompt') }}
          </p>
        </div>
      </div>
    </div>

    <div class="login-card-shell overflow-hidden rounded-xl border bg-card/60 shadow-[0_16px_50px_-32px_hsl(var(--primary)/0.45)]">
      <div class="login-card-content">
        <div class="login-methods">
          <article class="login-method px-5 py-5 sm:px-6">
            <div class="login-method-copy flex min-w-0 items-start gap-4">
              <div class="flex h-11 w-11 shrink-0 items-center justify-center rounded-lg bg-primary/10 text-primary">
                <RadioTower class="h-5 w-5" />
              </div>
              <div class="min-w-0">
                <div class="flex flex-wrap items-center gap-2">
                  <h3 class="font-semibold">{{ t('auth.cdp_login') }}</h3>
                  <span :class="['inline-flex items-center gap-1.5 rounded-md px-2 py-1 text-xs font-medium', cdpStatusClass]">
                    <Loader2 v-if="cdpAvailability === 'checking'" class="h-3 w-3 animate-spin" />
                    <span v-else class="h-1.5 w-1.5 rounded-full bg-current opacity-80" />
                    {{ t(cdpStatusKey) }}
                  </span>
                </div>
                <p class="mt-1 max-w-md text-sm leading-5 text-muted-foreground">{{ t(cdpLoginDetailKey) }}</p>
              </div>
            </div>
            <div class="login-method-action space-y-4">
              <!-- Port selection for adding accounts (Phase 6.5) -->
              <div v-if="allowPortSelection" class="space-y-3 rounded-lg border border-border/60 bg-muted/30 p-4">
                <div class="space-y-2">
                  <Label for="cdp-port">{{ t('auth.cdp_port_label') }}</Label>
                  <div class="flex gap-2">
                    <Input
                      id="cdp-port"
                      v-model.number="selectedPort"
                      type="number"
                      min="1024"
                      max="65535"
                      :disabled="busy"
                      class="flex-1"
                      :placeholder="t('auth.cdp_port_placeholder')"
                    />
                    <Button
                      type="button"
                      variant="outline"
                      size="sm"
                      :disabled="busy || detectingPorts"
                      @click="detectCdpPorts"
                    >
                      <Loader2 v-if="detectingPorts" class="mr-2 h-4 w-4 animate-spin" />
                      <Search v-else class="mr-2 h-4 w-4" />
                      {{ t('auth.cdp_port_detect') }}
                    </Button>
                  </div>
                  <p class="text-xs text-muted-foreground">
                    {{ t('auth.cdp_port_hint') }}
                  </p>
                  <p v-if="portError" class="text-xs text-destructive">
                    {{ portError }}
                  </p>
                </div>

                <!-- Detected ports display -->
                <div v-if="detectedPorts.size > 0" class="space-y-2">
                  <p class="text-xs font-medium text-muted-foreground">
                    {{ t('auth.cdp_detected_ports') }}
                  </p>
                  <div class="flex flex-wrap gap-2">
                    <button
                      v-for="port in CDP_CANDIDATE_PORTS"
                      :key="port"
                      type="button"
                      :disabled="!isPortDetected(port)"
                      :class="[
                        'inline-flex items-center gap-1 rounded-md px-2 py-1 text-xs font-medium transition-colors',
                        isPortDetected(port)
                          ? selectedPort === port
                            ? 'bg-primary text-primary-foreground cursor-pointer hover:bg-primary/90'
                            : 'bg-muted text-foreground cursor-pointer hover:bg-muted/80'
                          : 'bg-muted/50 text-muted-foreground cursor-not-allowed opacity-50'
                      ]"
                      @click="selectDetectedPort(port)"
                    >
                      {{ port }}
                      <span v-if="isPortDetected(port)" class="ml-1">✓</span>
                    </button>
                  </div>
                </div>

                <!-- Multi-port explanation -->
                <div class="rounded-md bg-blue-500/10 border border-blue-500/20 p-3">
                  <p class="text-xs text-blue-700 dark:text-blue-300">
                    {{ t('auth.cdp_multi_port_hint') }}
                  </p>
                </div>
              </div>

              <!-- Duplicate account notice (Phase 6.5) -->
              <div
                v-if="authStore.duplicateLoginAccountId"
                class="rounded-lg border border-amber-500/50 bg-amber-500/10 p-4"
              >
                <div class="flex items-start gap-3">
                  <AlertTriangle class="h-5 w-5 shrink-0 text-amber-600 dark:text-amber-400 mt-0.5" />
                  <div class="flex-1 space-y-1">
                    <p class="text-sm font-medium text-amber-900 dark:text-amber-100">
                      {{ t('auth.duplicate_account_title') }}
                    </p>
                    <p class="text-xs text-amber-800 dark:text-amber-200">
                      {{ t('auth.duplicate_account_desc', {
                        account: getDuplicateAccountName()
                      }) }}
                    </p>
                  </div>
                </div>
              </div>

              <Button
                size="lg"
                class="login-method-button gap-2"
                :disabled="busy"
                @click="handleCdpLogin"
              >
                <Loader2 v-if="activeMethod === 'cdp'" class="h-4 w-4 shrink-0 animate-spin" />
                {{ t(cdpLoginActionKey) }}
              </Button>
            </div>
          </article>
        </div>
      </div>

      <Transition name="progress-status">
        <div v-if="progress" class="progress-status-grid">
          <div
            :role="progress.state === 'error' ? 'alert' : 'status'"
            aria-live="polite"
            aria-atomic="true"
            :class="[
              'progress-status-row flex items-start gap-2.5 border-t px-5 py-3 text-sm sm:px-6',
              progress.state === 'error' && 'bg-destructive/5 text-destructive',
              progress.state === 'success' && 'bg-emerald-500/5 text-emerald-700 dark:text-emerald-300',
              (progress.state === 'running' || progress.state === 'waiting' || progress.state === 'neutral') && 'text-muted-foreground',
            ]"
          >
            <Loader2 v-if="progress.state === 'running'" class="mt-0.5 h-4 w-4 shrink-0 animate-spin text-primary" />
            <Check v-else-if="progress.state === 'success'" class="mt-0.5 h-4 w-4 shrink-0" />
            <AlertCircle v-else-if="progress.state === 'error'" class="mt-0.5 h-4 w-4 shrink-0" />
            <RadioTower v-else class="mt-0.5 h-4 w-4 shrink-0" />
            <span class="min-w-0 break-words">{{ progressText }}</span>
          </div>
        </div>
      </Transition>
    </div>

    <AlertDialog :open="cdpChooseDialogOpen" @update:open="handleChooseDialogOpenChange">
      <AlertDialogContent class="client-picker-dialog max-w-[560px] gap-0 overflow-hidden border-border/70 bg-background/95 p-0 shadow-[0_24px_80px_-32px_hsl(var(--primary)/0.45)] backdrop-blur-xl">
        <div class="border-b border-border/60 bg-primary/[0.045]">
          <AlertDialogHeader class="px-6 pb-6 pt-6 sm:px-8 sm:pb-7 sm:pt-8">
            <div class="flex items-start justify-between gap-4">
              <div class="flex min-w-0 items-start gap-3.5">
                <div class="flex h-11 w-11 shrink-0 items-center justify-center rounded-xl bg-primary text-primary-foreground shadow-[0_10px_24px_-12px_hsl(var(--primary))]">
                  <RadioTower class="h-5 w-5" :stroke-width="1.8" />
                </div>
                <div class="min-w-0">
                  <AlertDialogTitle class="text-xl font-semibold tracking-[-0.02em] sm:text-[1.35rem]">
                    {{ t('auth.cdp_choose_title') }}
                  </AlertDialogTitle>
                  <AlertDialogDescription class="mt-2 max-w-[38rem] text-sm leading-6 text-muted-foreground sm:text-[0.95rem]">
                    {{ t('auth.cdp_choose_desc') }}
                  </AlertDialogDescription>
                </div>
              </div>
              <span class="hidden shrink-0 items-center gap-2 rounded-full border border-border/70 bg-background/70 px-2.5 py-1.5 text-[11px] font-semibold tracking-[0.04em] text-muted-foreground sm:inline-flex">
                <span class="h-1.5 w-1.5 rounded-full bg-muted-foreground/60" aria-hidden="true" />
                {{ t('settings.cdp_disconnected_short') }}
              </span>
            </div>
          </AlertDialogHeader>
        </div>

        <div class="space-y-4 px-6 py-5 sm:px-8 sm:py-6">
          <label class="group flex cursor-pointer items-center gap-3 rounded-xl border border-border/70 bg-card/60 px-3.5 py-3 transition-colors duration-200 hover:border-primary/35 hover:bg-primary/[0.035]">
            <input
              v-model="rememberCdpChoice"
              type="checkbox"
              class="peer sr-only"
            />
            <span class="flex h-5 w-5 shrink-0 items-center justify-center rounded-[6px] border border-muted-foreground/35 bg-background text-transparent transition-all duration-200 peer-checked:border-primary peer-checked:bg-primary peer-checked:text-primary-foreground peer-focus-visible:ring-2 peer-focus-visible:ring-primary/40 peer-focus-visible:ring-offset-2">
              <Check class="h-3.5 w-3.5" :stroke-width="3" aria-hidden="true" />
            </span>
            <span class="text-sm font-medium text-foreground">{{ t('auth.cdp_remember_choice') }}</span>
          </label>

          <div class="grid gap-2.5">
            <Button
              v-for="target in cdpLaunchChoices"
              :key="target"
              type="button"
              variant="outline"
              class="group flex h-auto min-h-[72px] w-full items-center justify-between rounded-xl border-border/70 bg-card/70 px-4 py-3.5 text-left shadow-sm transition-all duration-200 hover:-translate-y-0.5 hover:border-primary/45 hover:bg-primary/[0.045] hover:shadow-[0_12px_28px_-20px_hsl(var(--primary)/0.7)] active:translate-y-0 focus-visible:ring-2 focus-visible:ring-primary/40 focus-visible:ring-offset-2"
              @click="selectCdpLaunchTarget(target)"
            >
              <span class="flex min-w-0 items-center gap-3.5">
                <span class="flex h-10 w-10 shrink-0 items-center justify-center rounded-lg bg-primary/10 text-primary transition-colors duration-200 group-hover:bg-primary group-hover:text-primary-foreground">
                  <component
                    :is="target === 'vesktop' ? AppWindow : Monitor"
                    class="h-[18px] w-[18px]"
                    :stroke-width="1.9"
                    aria-hidden="true"
                  />
                </span>
                <span class="min-w-0 truncate text-[15px] font-semibold tracking-[-0.01em] text-foreground">
                  {{ t(`auth.cdp_client_${target}`) }}
                </span>
              </span>
              <ChevronRight class="h-4 w-4 shrink-0 text-muted-foreground/60 transition-transform duration-200 group-hover:translate-x-0.5 group-hover:text-primary" aria-hidden="true" />
            </Button>
          </div>
        </div>

        <AlertDialogFooter class="border-t border-border/60 bg-muted/20 px-6 py-4 sm:px-8">
          <AlertDialogCancel class="mt-0 rounded-lg border-transparent bg-transparent px-3 text-muted-foreground hover:bg-background hover:text-foreground sm:mt-0">
            {{ t('dialog.cancel') }}
          </AlertDialogCancel>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>

    <AlertDialog :open="cdpRestartDialogOpen" @update:open="handleRestartDialogOpenChange">
      <AlertDialogContent class="max-w-[520px]">
        <AlertDialogHeader>
          <AlertDialogTitle>{{
            ownerConflict
              ? t('desktop_clients.owner_conflict_title')
              : restartUsesVesktop
              ? t('settings.cdp_dialog_title_disconnected_vesktop')
              : t('settings.cdp_dialog_title_disconnected')
          }}</AlertDialogTitle>
          <AlertDialogDescription>{{
            ownerConflict
              ? t('desktop_clients.owner_conflict_desc')
              : restartUsesVesktop
              ? t('settings.cdp_dialog_desc_disconnected_vesktop')
              : t('settings.cdp_dialog_desc_disconnected')
          }}</AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>{{ t('dialog.cancel') }}</AlertDialogCancel>
          <Button v-if="ownerConflict" variant="outline" @click="useCurrentCdpOwner">
            {{ t('desktop_clients.use_current') }}
          </Button>
          <AlertDialogAction @click="confirmCdpRestart">
            {{ ownerConflict ? t('desktop_clients.switch_selected') : t('settings.cdp_dialog_confirm') }}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  </section>
</template>

<style scoped>
.login-brand-stage {
  view-transition-name: app-brand;
}

.login-methods {
  display: grid;
  grid-template-columns: minmax(0, 1fr);
}

.login-method {
  display: grid;
  grid-template-columns: minmax(0, 1fr);
  gap: 1rem;
}

.login-method-action,
.login-method-button {
  width: 100%;
}

.login-method-button {
  min-height: 2.75rem;
  height: auto;
  white-space: normal;
  line-height: 1.25;
  text-align: center;
}

.progress-status-grid {
  display: grid;
  grid-template-rows: 1fr;
}

.progress-status-row {
  min-height: 3rem;
}

.progress-status-enter-active,
.progress-status-leave-active {
  overflow: hidden;
  transform-origin: top;
  transition:
    grid-template-rows 320ms cubic-bezier(0.22, 1, 0.36, 1),
    opacity 220ms ease,
    transform 320ms cubic-bezier(0.22, 1, 0.36, 1);
}

.progress-status-enter-from,
.progress-status-leave-to {
  grid-template-rows: 0fr;
  opacity: 0;
  transform: translateY(-0.375rem) scaleY(0.97);
}

.progress-status-enter-from .progress-status-row,
.progress-status-leave-to .progress-status-row {
  min-height: 0;
}

@media (min-width: 640px) {
  .login-methods {
    grid-template-columns: minmax(0, 1fr) max-content;
  }

  .login-method {
    grid-column: 1 / -1;
    grid-template-columns: subgrid;
    align-items: center;
  }

  .login-method-copy {
    grid-column: 1;
  }

  .login-method-action {
    grid-column: 2;
    max-width: 18rem;
  }

  .login-method-button {
    min-width: 9rem;
  }
}

@media (prefers-reduced-motion: reduce) {
  .progress-status-enter-active,
  .progress-status-leave-active {
    transition-duration: 1ms;
  }
}
</style>
