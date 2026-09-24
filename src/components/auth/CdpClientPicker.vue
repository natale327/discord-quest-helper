<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import {
  AlertCircle,
  AppWindow,
  CheckCircle2,
  Loader2,
  Monitor,
  RadioTower,
  RefreshCw,
} from 'lucide-vue-next'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
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
import {
  launchDesktopClientCdp,
  type AuthProgress,
  type ClientSelection,
} from '@/api/tauri'
import { useAuthStore } from '@/stores/auth'
import { useDesktopClientState, type ClientAccountCandidate } from '@/composables/desktopClientState'
import { presentAuthProgress, type LoginProgressState } from './loginFlow'
import { toErrorMessage } from '@/utils/errorMessage'

const props = defineProps<{
  mode: 'add' | 'reconnect'
  targetAccountId?: string
}>()

const emit = defineEmits<{
  complete: []
  cancel: []
  mutationBusy: [busy: boolean]
  openAccountSettings: [accountId?: string]
}>()

type ClientIntent = 'add' | 'reconnect' | 'switch' | 'alreadyActive' | 'portConflict' | 'identityMismatch' | null

interface ManualCandidate extends ClientAccountCandidate {
  manual: true
}

interface PendingRestart {
  port: number
  selection: ClientSelection
  label: string
  candidateId: string
}

const { t } = useI18n()
const authStore = useAuthStore()
const clients = useDesktopClientState()

const selectedCandidateId = ref<string | null>(null)
const manualCandidate = ref<ManualCandidate | null>(null)
const preview = ref<{ port: number; user: { id: string; username: string; global_name?: string | null } } | null>(null)
const previewActiveAccountId = ref<string | null>(null)
const previewing = ref(false)
const mutationBusy = ref(false)
const manualChecking = ref(false)
const launchingClientId = ref<string | null>(null)
const manualPort = ref<number | ''>('')
const manualError = ref<string | null>(null)
const operationError = ref<string | null>(null)
const operationNotice = ref<string | null>(null)
const progressText = ref<string | null>(null)
const progressState = ref<LoginProgressState>('running')
const identityNeedsRefresh = ref(false)
const restartDialogOpen = ref(false)
const pendingRestart = ref<PendingRestart | null>(null)
const selectionRevision = ref(0)

const candidates = computed<ClientAccountCandidate[]>(() => (
  manualCandidate.value
    ? [manualCandidate.value, ...clients.accountCandidates.value]
    : clients.accountCandidates.value
))
const selectedCandidate = computed(() => (
  candidates.value.find(candidate => candidate.id === selectedCandidateId.value) ?? null
))
const targetAccount = computed(() => (
  props.targetAccountId
    ? authStore.accounts.find(account => account.id === props.targetAccountId) ?? null
    : null
))
const verifiedAccountName = computed(() => (
  preview.value?.user.global_name || preview.value?.user.username || ''
))
const previewIsCurrent = computed(() => (
  Boolean(preview.value) && previewActiveAccountId.value === authStore.activeAccountId
))
const currentPreviewUser = computed(() => (
  previewIsCurrent.value ? preview.value?.user ?? null : null
))
const matchedAccount = computed(() => (
  preview.value
    ? authStore.accounts.find(account => account.id === preview.value?.user.id) ?? null
    : null
))
const currentAccount = computed(() => (
  props.mode === 'reconnect' ? targetAccount.value : matchedAccount.value
))

function candidateKey(candidate: ClientAccountCandidate): string {
  return candidate.id
}

function isManual(candidate: ClientAccountCandidate | null): candidate is ManualCandidate {
  return Boolean(candidate && 'manual' in candidate && candidate.manual)
}

function clientName(candidate: ClientAccountCandidate | null): string {
  if (!candidate) return ''
  if (candidate.providerId === 'vencord.vesktop') return t('auth.cdp_client_vesktop')
  if (candidate.providerId === 'discord.official') {
    if (candidate.variantId === 'ptb') return t('auth.cdp_client_ptb')
    if (candidate.variantId === 'canary') return t('auth.cdp_client_canary')
    if (candidate.variantId === 'stable') return `${t('auth.cdp_client_stable')} Stable`
  }
  if (isManual(candidate)) return t('auth.client_picker_manual_client')
  return candidate.displayName || t('auth.cdp_login')
}

function clientIcon(candidate: ClientAccountCandidate) {
  return candidate.providerId === 'vencord.vesktop' ? AppWindow : Monitor
}

function ownerForPort(port: number, excludingAccountId?: string) {
  return authStore.accounts.find(account => (
    account.id !== excludingAccountId && authStore.portForAccount(account.id) === port
  )) ?? null
}

function portConflictMessage(port: number, owner: { globalName?: string | null; username: string } | null) {
  if (owner) {
    return t('auth.client_picker_port_conflict', {
      port,
      account: owner.globalName || owner.username,
    })
  }
  return `${t('auth.cdp_port_already_assigned')} (${t('auth.cdp_port_label')}: ${port})`
}

const selectedPortOwner = computed(() => (
  selectedCandidate.value
    ? ownerForPort(selectedCandidate.value.port, currentAccount.value?.id)
    : null
))
const selectedPortAvailable = computed(() => {
  if (!selectedCandidate.value) return false
  const excludingAccountId = props.mode === 'reconnect'
    ? props.targetAccountId
    : matchedAccount.value?.id
  return authStore.isAccountPortAvailable(selectedCandidate.value.port, excludingAccountId)
})

const identityMismatch = computed(() => (
  props.mode === 'reconnect' && Boolean(
    preview.value && props.targetAccountId && preview.value.user.id !== props.targetAccountId,
  )
))

const intent = computed<ClientIntent>(() => {
  const candidate = selectedCandidate.value
  if (!candidate?.ready || !previewIsCurrent.value) return null
  if (identityNeedsRefresh.value) return null
  if (props.mode === 'reconnect' && identityMismatch.value) return 'identityMismatch'

  if (props.mode === 'reconnect') {
    if (!targetAccount.value) return null
    if (targetAccount.value.isAuthenticated && authStore.portForAccount(targetAccount.value.id) > 0) return 'switch'
    if (!selectedPortAvailable.value) return 'portConflict'
    return 'reconnect'
  }

  if (matchedAccount.value) {
    if (matchedAccount.value.id === authStore.activeAccountId && authStore.user?.id === matchedAccount.value.id) {
      return 'alreadyActive'
    }
    if (matchedAccount.value.isAuthenticated) return 'switch'
    if (!selectedPortAvailable.value) return 'portConflict'
    return 'reconnect'
  }

  if (!selectedPortAvailable.value) return 'portConflict'
  return 'add'
})

const reconnectAccountId = computed(() => (
  intent.value === 'reconnect'
    ? props.mode === 'reconnect' ? props.targetAccountId : matchedAccount.value?.id
    : null
))
const reconnectAccountPort = computed(() => (
  reconnectAccountId.value ? authStore.portForAccount(reconnectAccountId.value) : 0
))
const portChangeNeedsConfirmation = computed(() => (
  intent.value === 'reconnect' &&
  selectedCandidate.value !== null &&
  selectedCandidate.value.port !== reconnectAccountPort.value
))

const primaryActionKey = computed(() => {
  if (intent.value === 'add') return 'auth.client_picker_add_account'
  if (intent.value === 'reconnect') {
    return portChangeNeedsConfirmation.value
      ? 'auth.client_picker_reassign_reconnect'
      : 'accounts.reconnect_account_named'
  }
  if (intent.value === 'switch') return 'auth.client_picker_switch_account'
  if (intent.value === 'alreadyActive') return 'accounts.active'
  return ''
})

const primaryActionLabel = computed(() => (
  primaryActionKey.value
    ? t(primaryActionKey.value, { account: verifiedAccountName.value })
    : ''
))

function endpointStatus(candidate: ClientAccountCandidate): string {
  if (launchingClientId.value === candidate.id) return t('auth.client_picker_starting')
  if (candidate.ready) return t('auth.client_picker_ready')
  return t('auth.client_picker_unavailable')
}

function statusClass(candidate: ClientAccountCandidate): string {
  if (candidate.ready) return 'bg-emerald-500/10 text-emerald-700 dark:text-emerald-300'
  if (launchingClientId.value === candidate.id) return 'bg-amber-500/10 text-amber-700 dark:text-amber-300'
  return 'bg-muted text-muted-foreground'
}

function clearSelection(messageKey?: string) {
  selectionRevision.value += 1
  selectedCandidateId.value = null
  manualCandidate.value = null
  preview.value = null
  previewActiveAccountId.value = null
  previewActiveAccountId.value = null
  previewing.value = false
  identityNeedsRefresh.value = false
  operationNotice.value = messageKey ? t(messageKey) : null
  operationError.value = null
  clients.cancelAccountClientSelection()
}

async function selectCandidate(candidate: ClientAccountCandidate) {
  if (mutationBusy.value || manualChecking.value || launchingClientId.value) return
  const sameCandidate = selectedCandidateId.value === candidateKey(candidate)
  if (sameCandidate && preview.value && !identityNeedsRefresh.value) return
  selectedCandidateId.value = candidateKey(candidate)
  operationError.value = null
  operationNotice.value = null
  identityNeedsRefresh.value = false
  preview.value = null
  previewActiveAccountId.value = null
  selectionRevision.value += 1
  const revision = selectionRevision.value
  const activeAccountIdAtPreview = authStore.activeAccountId

  if (!candidate.ready) {
    clients.cancelAccountClientSelection()
    return
  }

  previewing.value = true
  try {
    if (isManual(candidate)) clients.cancelAccountClientSelection()
    const result = isManual(candidate)
      ? await authStore.previewClientAccount(candidate.port)
      : await clients.selectAccountClientCandidate(candidate.id)
    if (revision !== selectionRevision.value || selectedCandidateId.value !== candidate.id) return
    if (!result) {
      operationError.value = clients.accountPreviewError.value || t('auth.client_picker_preview_failed')
      return
    }
    if (activeAccountIdAtPreview !== authStore.activeAccountId) {
      operationError.value = t('auth.client_picker_identity_changed')
      return
    }
    preview.value = result
    previewActiveAccountId.value = activeAccountIdAtPreview
  } catch (cause) {
    if (revision === selectionRevision.value) operationError.value = toErrorMessage(cause)
  } finally {
    if (revision === selectionRevision.value) previewing.value = false
  }
}

function retryPreview() {
  const candidate = selectedCandidate.value
  if (!candidate || !candidate.ready) return
  identityNeedsRefresh.value = false
  operationError.value = null
  operationNotice.value = null
  clients.cancelAccountClientSelection()
  void selectCandidate(candidate)
}

function invalidateChangedIdentity(messageKey: string) {
  selectionRevision.value += 1
  clients.cancelAccountClientSelection()
  preview.value = null
  previewActiveAccountId.value = null
  previewing.value = false
  operationError.value = null
  operationNotice.value = t(messageKey)
  identityNeedsRefresh.value = true
}

function handleProgress(event: AuthProgress) {
  const presentation = presentAuthProgress(event)
  progressState.value = presentation.state
  progressText.value = t(presentation.key, presentation.params ?? {})
}

async function handlePrimaryAction() {
  const candidate = selectedCandidate.value
  const verified = preview.value
  const currentIntent = intent.value
  if (!candidate || !candidate.ready || !verified || !previewIsCurrent.value || !currentIntent || mutationBusy.value) return
  if (currentIntent === 'alreadyActive') return

  mutationBusy.value = true
  emit('mutationBusy', true)
  operationError.value = null
  operationNotice.value = null
  progressText.value = t('general.loading')
  progressState.value = 'running'
  try {
    if (currentIntent === 'switch') {
      const existing = matchedAccount.value
      const accountId = props.mode === 'reconnect' ? props.targetAccountId : existing?.id
      if (!accountId) return
      const switched = await authStore.switchOnlineAccount(accountId)
      if (!switched || authStore.activeAccountId !== accountId || authStore.user?.id !== accountId) {
        operationError.value = authStore.error || t('auth.progress.failed')
        progressText.value = null
        return
      }
      emit('complete')
      return
    }

    if (currentIntent === 'add') {
      const result = await authStore.confirmAddClientAccount(candidate.port, verified.user.id, handleProgress)
      if (result.status === 'added' && result.user.id === verified.user.id) {
        emit('complete')
        return
      }
      if (result.status === 'alreadySaved') {
        if (result.user.id !== verified.user.id) {
          invalidateChangedIdentity('auth.client_picker_identity_changed')
          progressText.value = null
          return
        }
        await authStore.loadAccounts()
        preview.value = { port: result.port, user: result.user }
        previewActiveAccountId.value = authStore.activeAccountId
        operationNotice.value = t('auth.client_picker_already_saved', {
          account: result.user.global_name || result.user.username,
        })
        progressText.value = null
        return
      }
      if (result.status === 'identityChanged') {
        invalidateChangedIdentity('auth.client_picker_identity_changed')
        progressText.value = null
        return
      }
      if (result.status === 'portConflict') {
        await authStore.loadAccounts()
        preview.value = { port: result.port, user: result.user }
        previewActiveAccountId.value = authStore.activeAccountId
        operationError.value = portConflictMessage(result.port, ownerForPort(result.port))
        progressText.value = null
        return
      }
      operationError.value = authStore.error || t('auth.progress.failed')
      progressText.value = null
      return
    }

    const accountId = props.mode === 'reconnect' ? props.targetAccountId : matchedAccount.value?.id
    if (!accountId) return
    const result = await authStore.reconnectSavedAccount(accountId, candidate.port, handleProgress)
    if (result.status === 'reconnected') {
      if (result.user.id !== accountId) {
        invalidateChangedIdentity('auth.client_picker_identity_changed')
        progressText.value = null
        return
      }
      emit('complete')
      return
    }
    if (result.status === 'identityChanged') {
      invalidateChangedIdentity('auth.client_picker_identity_changed')
      progressText.value = null
      return
    }
    if (result.status === 'portConflict') {
      await authStore.loadAccounts()
      preview.value = { port: result.port, user: result.user }
      previewActiveAccountId.value = authStore.activeAccountId
      operationError.value = portConflictMessage(result.port, ownerForPort(result.port, accountId))
      progressText.value = null
      return
    }
    operationError.value = authStore.error || t('auth.progress.failed')
    progressText.value = null
  } catch (cause) {
    operationError.value = toErrorMessage(cause)
    progressText.value = null
  } finally {
    mutationBusy.value = false
    emit('mutationBusy', false)
  }
}

const launchPort = computed(() => {
  if (props.mode === 'reconnect' && props.targetAccountId) {
    const assigned = authStore.portForAccount(props.targetAccountId)
    if (assigned > 0 && authStore.isAccountPortAvailable(assigned, props.targetAccountId)) return assigned
    return authStore.suggestAccountPort(props.targetAccountId)
  }
  return authStore.suggestAccountPort()
})

function selectionForCandidate(candidate: ClientAccountCandidate): ClientSelection | null {
  if (candidate.installationId) {
    return { kind: 'installation', installationId: candidate.installationId }
  }
  if (!candidate.providerId) return null
  return {
    kind: 'provider',
    providerId: candidate.providerId,
    variantId: candidate.variantId,
  }
}

function candidateIsRunning(candidate: ClientAccountCandidate, snapshot: Awaited<ReturnType<typeof clients.refresh>>) {
  if (!snapshot) return false
  if (candidate.installationId) {
    return snapshot.processes.some(process => process.installationId === candidate.installationId)
  }
  return snapshot.processes.some(process => (
    process.providerId === candidate.providerId &&
    (!candidate.variantId || process.variantId === candidate.variantId)
  ))
}

async function launchSelectedCandidate(candidate: ClientAccountCandidate, restart = false) {
  const selection = selectionForCandidate(candidate)
  if (!selection) {
    operationError.value = t('settings.cdp_disconnected_short')
    return
  }
  if (!restart && authStore.accounts.some(account => (
    authStore.portForAccount(account.id) === candidate.port &&
    account.id !== (props.mode === 'reconnect' ? props.targetAccountId : undefined)
  ))) {
    operationError.value = portConflictMessage(candidate.port, ownerForPort(candidate.port, props.targetAccountId))
    return
  }

  launchingClientId.value = candidate.id
  operationError.value = null
  operationNotice.value = null
  try {
    if (!restart) {
      const snapshot = await clients.refresh(candidate.port)
      if (candidateIsRunning(candidate, snapshot)) {
        pendingRestart.value = {
          port: candidate.port,
          selection,
          label: clientName(candidate),
          candidateId: candidate.id,
        }
        restartDialogOpen.value = true
        return
      }
    }
    await launchDesktopClientCdp(candidate.port, selection, restart)
    await clients.scanAccountClients()
    operationNotice.value = t('auth.cdp_status_starting')
    selectedCandidateId.value = null
    preview.value = null
  } catch (cause) {
    operationError.value = toErrorMessage(cause)
  } finally {
    launchingClientId.value = null
  }
}

async function launchNamedClient(providerId: string, variantId: string | null) {
  if (launchPort.value === 0) {
    operationError.value = t('auth.cdp_port_no_available_port')
    return
  }
  const client = t(variantId === 'ptb'
    ? 'auth.cdp_client_ptb'
    : variantId === 'canary'
      ? 'auth.cdp_client_canary'
      : providerId === 'vencord.vesktop'
        ? 'auth.cdp_client_vesktop'
        : 'auth.cdp_client_stable')
  const port = launchPort.value
  const candidate: ClientAccountCandidate = {
    id: `launch:${port}:${providerId}:${variantId ?? ''}`,
    port,
    providerId,
    variantId,
    installationId: null,
    displayName: client,
    ready: false,
    endpointState: 'unreachable',
    ownership: null,
  }
  await launchSelectedCandidate(candidate)
}

async function confirmRestart() {
  const pending = pendingRestart.value
  if (!pending) return
  restartDialogOpen.value = false
  launchingClientId.value = pending.candidateId
  try {
    await launchDesktopClientCdp(pending.port, pending.selection, true)
    pendingRestart.value = null
    await clients.scanAccountClients()
    operationNotice.value = t('auth.cdp_status_starting')
  } catch (cause) {
    operationError.value = toErrorMessage(cause)
  } finally {
    launchingClientId.value = null
  }
}

async function checkManualPort() {
  const port = Number(manualPort.value)
  if (!Number.isInteger(port) || port < 1024 || port > 65535) {
    manualError.value = t('auth.port_invalid_range')
    return
  }
  manualChecking.value = true
  manualError.value = null
  operationError.value = null
  try {
    const snapshot = await clients.refresh(port)
    if (!snapshot || snapshot.endpoint.status !== 'discordReady') {
      manualError.value = t('settings.cdp_disconnected_short')
      return
    }
    const selection = snapshot.selection
    const providerId = selection.kind === 'provider'
      ? selection.providerId
      : selection.kind === 'installation'
        ? snapshot.installations.find(item => item.id === selection.installationId)?.providerId ?? snapshot.endpoint.ownerProviderId
        : snapshot.endpoint.ownerProviderId
    const variantId = selection.kind === 'provider'
      ? selection.variantId ?? null
      : selection.kind === 'installation'
        ? snapshot.installations.find(item => item.id === selection.installationId)?.variantId ?? null
        : null
    const candidate: ManualCandidate = {
      id: `manual:${port}`,
      port,
      providerId,
      variantId,
      installationId: selection.kind === 'installation' ? selection.installationId : null,
      displayName: t('auth.client_picker_manual_client'),
      ready: true,
      endpointState: 'discordReady',
      ownership: null,
      manual: true,
    }
    manualCandidate.value = candidate
    manualChecking.value = false
    await selectCandidate(candidate)
  } catch (cause) {
    manualError.value = toErrorMessage(cause)
  } finally {
    manualChecking.value = false
  }
}

function handleCandidateKeydown(event: KeyboardEvent, candidate: ClientAccountCandidate) {
  if (!['ArrowDown', 'ArrowRight', 'ArrowUp', 'ArrowLeft', 'Home', 'End'].includes(event.key)) return
  const list = candidates.value.filter(item => item.ready || item.providerId || item.installationId)
  if (list.length === 0) return
  event.preventDefault()
  const currentIndex = list.findIndex(item => item.id === candidate.id)
  const nextIndex = event.key === 'Home'
    ? 0
    : event.key === 'End'
      ? list.length - 1
      : (currentIndex + (event.key === 'ArrowDown' || event.key === 'ArrowRight' ? 1 : -1) + list.length) % list.length
  const nextCandidate = list[nextIndex]
  void selectCandidate(nextCandidate)
  void nextTick(() => document.getElementById(`client-candidate-${nextCandidate.id}`)?.focus())
}

function rescanClients() {
  clearSelection()
  manualError.value = null
  void clients.scanAccountClients()
}

function openAccountSettings() {
  if (mutationBusy.value) return
  emit('openAccountSettings', targetAccount.value?.id ?? matchedAccount.value?.id)
}

function handleCancel() {
  if (mutationBusy.value) return
  emit('cancel')
}

watch(() => props.targetAccountId, () => clearSelection())
watch(() => authStore.activeAccountId, accountId => {
  if (!mutationBusy.value && preview.value && previewActiveAccountId.value !== accountId) {
    clearSelection('auth.client_picker_identity_changed')
  }
})
watch(
  () => clients.accountCandidates.value.map(candidate => candidate.id).join('|'),
  candidateIds => {
    if (!selectedCandidateId.value || isManual(selectedCandidate.value)) return
    if (!candidateIds.split('|').includes(selectedCandidateId.value)) {
      clearSelection('auth.client_picker_identity_changed')
    }
  },
)
watch(() => selectedCandidate.value?.ready, (ready, previous) => {
  if (previous === true && ready === false && preview.value) {
    clearSelection('auth.client_picker_identity_changed')
  }
})

onMounted(() => {
  if (authStore.accounts.length === 0) {
    void authStore.loadAccounts()
      .catch(() => undefined)
      .then(() => clients.scanAccountClients())
  } else {
    void clients.scanAccountClients()
  }
})

onUnmounted(() => {
  clients.cancelAccountClientSelection()
})
</script>

<template>
  <section class="space-y-5" aria-labelledby="client-picker-title">
    <header class="space-y-2">
      <div class="flex items-start gap-3">
        <div class="flex h-10 w-10 shrink-0 items-center justify-center rounded-lg bg-primary/10 text-primary">
          <RadioTower class="h-5 w-5" aria-hidden="true" />
        </div>
        <div class="min-w-0 flex-1">
          <h2 id="client-picker-title" class="text-xl font-semibold tracking-tight">
            {{ t('auth.cdp_choose_title') }}
          </h2>
          <p class="mt-1 text-sm leading-5 text-muted-foreground">
            {{ mode === 'reconnect'
              ? t('auth.client_picker_reconnect_desc', { account: targetAccount?.globalName || targetAccount?.username || '' })
              : t('auth.client_picker_add_desc') }}
          </p>
        </div>
      </div>
    </header>

    <div v-if="clients.accountCandidatesLoading.value" class="flex items-center gap-2 rounded-lg border bg-muted/30 px-3 py-3 text-sm text-muted-foreground" role="status" aria-live="polite">
      <Loader2 class="h-4 w-4 animate-spin" aria-hidden="true" />
      {{ t('auth.progress.checking_cdp') }}
    </div>

    <div v-if="clients.accountCandidatesError.value" class="flex items-start gap-2 rounded-lg border border-destructive/40 bg-destructive/5 px-3 py-3 text-sm text-destructive" role="alert">
      <AlertCircle class="mt-0.5 h-4 w-4 shrink-0" aria-hidden="true" />
      <span class="min-w-0">{{ clients.accountCandidatesError.value }}</span>
    </div>

    <div v-if="mode === 'reconnect' && !targetAccount" class="rounded-lg border border-destructive/40 bg-destructive/5 px-3 py-3 text-sm text-destructive" role="alert">
      {{ t('auth.client_picker_saved_account_missing') }}
    </div>

    <div v-if="candidates.length > 0" class="space-y-2">
      <div class="flex items-center justify-between gap-3">
        <p id="client-candidate-label" class="text-sm font-medium">
          {{ t('auth.client_picker_candidates') }}
        </p>
        <Button type="button" size="sm" variant="ghost" class="h-8 shrink-0 gap-1.5 px-2" :disabled="clients.accountCandidatesLoading.value || mutationBusy" @click="rescanClients">
          <RefreshCw class="h-3.5 w-3.5" aria-hidden="true" />
          {{ t('auth.rescan') }}
        </Button>
      </div>
      <div role="radiogroup" aria-labelledby="client-candidate-label" class="grid gap-2">
        <button
          v-for="candidate in candidates"
          :id="`client-candidate-${candidate.id}`"
          :key="candidate.id"
          type="button"
          role="radio"
          :aria-checked="selectedCandidateId === candidate.id"
          :tabindex="selectedCandidateId === candidate.id || (!selectedCandidateId && candidate === candidates[0]) ? 0 : -1"
          :disabled="mutationBusy || manualChecking || launchingClientId !== null"
          :class="[
            'group flex min-w-0 items-center gap-3 rounded-lg border px-3.5 py-3 text-left transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2',
            selectedCandidateId === candidate.id
              ? 'border-primary/60 bg-primary/[0.06]'
              : 'border-border/70 bg-card hover:bg-muted/40',
            !candidate.ready && 'opacity-80',
          ]"
          @click="selectCandidate(candidate)"
          @keydown="handleCandidateKeydown($event, candidate)"
        >
          <span class="flex h-9 w-9 shrink-0 items-center justify-center rounded-lg bg-muted text-muted-foreground">
            <component :is="clientIcon(candidate)" class="h-4 w-4" aria-hidden="true" />
          </span>
          <span class="min-w-0 flex-1">
            <span class="flex min-w-0 flex-wrap items-center gap-2">
              <span class="truncate text-sm font-medium">{{ clientName(candidate) }}</span>
              <span :class="['inline-flex items-center gap-1 rounded-md px-2 py-0.5 text-xs font-medium', statusClass(candidate)]">
                <Loader2 v-if="launchingClientId === candidate.id" class="h-3 w-3 animate-spin" aria-hidden="true" />
                <span v-else class="h-1.5 w-1.5 rounded-full bg-current opacity-80" aria-hidden="true" />
                {{ endpointStatus(candidate) }}
              </span>
              <span
                v-if="ownerForPort(candidate.port, currentAccount?.id)"
                class="text-xs text-amber-700 dark:text-amber-300"
              >
                {{ t('auth.client_picker_port_assigned', {
                  account: ownerForPort(candidate.port, currentAccount?.id)?.globalName || ownerForPort(candidate.port, currentAccount?.id)?.username || ''
                }) }}
              </span>
            </span>
            <span v-if="selectedCandidateId === candidate.id && previewing" class="mt-1 flex items-center gap-1.5 text-xs text-muted-foreground" role="status" aria-live="polite">
              <Loader2 class="h-3 w-3 animate-spin" aria-hidden="true" />
              {{ t('auth.progress.validating_cdp_session') }}
            </span>
            <span v-else-if="selectedCandidateId === candidate.id && previewIsCurrent" class="mt-1 block truncate text-xs text-muted-foreground">
        {{ t('auth.authenticated_as') }} {{ verifiedAccountName }}
            </span>
            <span v-else-if="selectedCandidateId === candidate.id && clients.accountPreviewError.value" class="mt-1 block text-xs text-destructive">
              {{ t('auth.client_picker_preview_failed') }}
            </span>
          </span>
          <CheckCircle2 v-if="selectedCandidateId === candidate.id" class="h-4 w-4 shrink-0 text-primary" aria-hidden="true" />
        </button>
      </div>
    </div>

    <div v-else-if="!clients.accountCandidatesLoading.value" class="rounded-lg border border-dashed border-border/80 bg-muted/20 px-4 py-5 text-center">
      <RadioTower class="mx-auto h-6 w-6 text-muted-foreground" aria-hidden="true" />
      <p class="mt-2 text-sm font-medium">{{ t('auth.client_picker_no_ready') }}</p>
      <p class="mt-1 text-xs leading-5 text-muted-foreground">{{ t('auth.cdp_choose_desc') }}</p>
      <Button type="button" variant="outline" size="sm" class="mt-3 gap-2" :disabled="clients.accountCandidatesLoading.value" @click="rescanClients">
        <RefreshCw class="h-3.5 w-3.5" aria-hidden="true" />
        {{ t('auth.rescan') }}
      </Button>
    </div>

    <div v-if="identityNeedsRefresh && selectedCandidate" class="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-amber-500/40 bg-amber-500/5 px-3.5 py-3 text-sm" role="status" aria-live="polite">
      <span class="min-w-0 text-amber-800 dark:text-amber-200">{{ operationNotice || t('auth.client_picker_identity_changed') }}</span>
      <Button type="button" size="sm" variant="outline" :disabled="previewing" @click="retryPreview">
        <Loader2 v-if="previewing" class="mr-2 h-3.5 w-3.5 animate-spin" aria-hidden="true" />
        {{ t('auth.rescan') }}
      </Button>
    </div>

    <div v-if="operationError && !preview" class="rounded-lg border border-destructive/40 bg-destructive/5 px-3.5 py-3 text-sm text-destructive" role="alert">
      {{ operationError }}
      <Button v-if="mode === 'reconnect' && targetAccountId" type="button" variant="link" size="sm" class="mt-1 h-auto px-0 py-0 text-destructive" :disabled="mutationBusy" @click="openAccountSettings">
        {{ t('settings.title') }}
      </Button>
    </div>

    <div v-if="operationNotice && !preview && !identityNeedsRefresh" class="rounded-lg border border-amber-500/40 bg-amber-500/5 px-3.5 py-3 text-sm text-amber-800 dark:text-amber-200" role="status" aria-live="polite">
      {{ operationNotice }}
    </div>

    <div v-if="candidates.length === 0 && !clients.accountCandidatesLoading.value" class="space-y-2">
      <p class="text-xs font-medium text-muted-foreground">{{ t('settings.cdp_launch') }}</p>
      <div class="flex flex-wrap gap-2">
        <Button
          v-for="client in [
            { provider: 'discord.official', variant: 'stable', key: 'auth.cdp_client_stable' },
            { provider: 'discord.official', variant: 'ptb', key: 'auth.cdp_client_ptb' },
            { provider: 'discord.official', variant: 'canary', key: 'auth.cdp_client_canary' },
            { provider: 'vencord.vesktop', variant: null, key: 'auth.cdp_client_vesktop' },
          ]"
          :key="`${client.provider}:${client.variant ?? 'default'}`"
          type="button"
          size="sm"
          variant="outline"
          class="gap-1.5"
          :disabled="launchPort === 0 || launchingClientId !== null"
          @click="launchNamedClient(client.provider, client.variant)"
        >
          <Loader2 v-if="launchingClientId === `launch:${launchPort}:${client.provider}:${client.variant ?? ''}`" class="h-3.5 w-3.5 animate-spin" aria-hidden="true" />
          <component v-else :is="client.provider === 'vencord.vesktop' ? AppWindow : Monitor" class="h-3.5 w-3.5" aria-hidden="true" />
          {{ t(client.key) }}
        </Button>
      </div>
      <p v-if="launchPort === 0" class="text-xs text-destructive" role="alert">
        {{ t('auth.cdp_port_no_available_port') }}
      </p>
    </div>

    <div v-if="previewIsCurrent && selectedCandidate" class="space-y-3 rounded-lg border border-border/70 bg-muted/20 p-3.5">
      <div class="flex items-start gap-2.5">
        <CheckCircle2 class="mt-0.5 h-4 w-4 shrink-0 text-emerald-600 dark:text-emerald-400" aria-hidden="true" />
        <div class="min-w-0 flex-1">
          <p class="text-sm font-medium">{{ clientName(selectedCandidate) }}</p>
          <p class="mt-0.5 truncate text-sm text-muted-foreground">
            {{ t('auth.authenticated_as') }} {{ verifiedAccountName }} <span class="text-xs">@{{ currentPreviewUser?.username }}</span>
          </p>
        </div>
      </div>

      <div v-if="identityMismatch" class="rounded-md border border-destructive/40 bg-destructive/5 p-3 text-sm text-destructive" role="alert">
        {{ t('auth.client_picker_identity_mismatch', {
          expected: targetAccount?.globalName || targetAccount?.username || '',
          actual: verifiedAccountName,
        }) }}
      </div>

      <div v-else-if="intent === 'portConflict'" class="rounded-md border border-destructive/40 bg-destructive/5 p-3 text-sm text-destructive" role="alert">
        {{ portConflictMessage(selectedCandidate.port, selectedPortOwner) }}
        <Button type="button" variant="link" size="sm" class="mt-1 h-auto px-0 py-0 text-destructive" :disabled="mutationBusy" @click="openAccountSettings">
          {{ t('settings.title') }}
        </Button>
      </div>

      <div v-if="operationNotice" class="rounded-md border border-amber-500/40 bg-amber-500/5 p-3 text-sm text-amber-800 dark:text-amber-200" role="status" aria-live="polite">
        {{ operationNotice }}
      </div>
      <div v-if="operationError" class="rounded-md border border-destructive/40 bg-destructive/5 p-3 text-sm text-destructive" role="alert">
        {{ operationError }}
      </div>

      <p v-if="portChangeNeedsConfirmation && intent === 'reconnect'" class="text-xs leading-5 text-amber-800 dark:text-amber-200">
        {{ reconnectAccountPort === 0
          ? t('auth.client_picker_unassigned_reconnect_warning', {
            account: currentAccount?.globalName || currentAccount?.username || '',
            client: clientName(selectedCandidate),
            newPort: selectedCandidate.port,
          })
          : t('auth.client_picker_port_change_warning', {
            account: currentAccount?.globalName || currentAccount?.username || '',
            client: clientName(selectedCandidate),
            oldPort: reconnectAccountPort,
            newPort: selectedCandidate.port,
          }) }}
      </p>

      <div class="flex flex-wrap items-center justify-end gap-2 border-t border-border/60 pt-3">
        <Button v-if="identityNeedsRefresh" type="button" variant="outline" size="sm" :disabled="previewing" @click="retryPreview">
          <Loader2 v-if="previewing" class="mr-2 h-3.5 w-3.5 animate-spin" aria-hidden="true" />
          {{ t('auth.rescan') }}
        </Button>
        <Button
          v-else-if="intent && intent !== 'alreadyActive' && intent !== 'identityMismatch' && intent !== 'portConflict'"
          type="button"
          size="sm"
          class="gap-2"
          :disabled="mutationBusy || Boolean(operationError) || identityNeedsRefresh"
          @click="handlePrimaryAction"
        >
          <Loader2 v-if="mutationBusy" class="h-3.5 w-3.5 animate-spin" aria-hidden="true" />
          {{ primaryActionLabel }}
        </Button>
        <span v-else-if="intent === 'alreadyActive'" class="text-xs font-medium text-muted-foreground" role="status">
          {{ t('accounts.active') }}
        </span>
      </div>
    </div>

    <div v-else-if="selectedCandidate && !selectedCandidate.ready" class="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-border/70 bg-muted/20 p-3.5">
      <p class="min-w-0 flex-1 text-sm text-muted-foreground">
        {{ t('auth.client_picker_start_or_restart', { client: clientName(selectedCandidate) }) }}
      </p>
      <Button
        type="button"
        size="sm"
        class="shrink-0 gap-2"
        :disabled="launchingClientId !== null || !selectionForCandidate(selectedCandidate)"
        @click="launchSelectedCandidate(selectedCandidate)"
      >
        <Loader2 v-if="launchingClientId === selectedCandidate.id" class="h-3.5 w-3.5 animate-spin" aria-hidden="true" />
          {{ t('settings.cdp_launch') }}
      </Button>
    </div>

    <div v-if="progressText" :class="[
      'flex items-start gap-2 rounded-md px-3 py-2 text-sm',
      progressState === 'error' ? 'bg-destructive/5 text-destructive' : 'bg-muted/40 text-muted-foreground',
    ]" :role="progressState === 'error' ? 'alert' : 'status'" aria-live="polite">
      <Loader2 v-if="progressState === 'running'" class="mt-0.5 h-4 w-4 shrink-0 animate-spin" aria-hidden="true" />
      <AlertCircle v-else class="mt-0.5 h-4 w-4 shrink-0" aria-hidden="true" />
      <span class="min-w-0">{{ progressText }}</span>
    </div>

    <details class="rounded-lg border border-border/70 bg-muted/15 px-3.5 py-3">
      <summary class="cursor-pointer list-none text-sm font-medium outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2">
        {{ t('auth.client_picker_connection_details') }}
      </summary>
      <div class="mt-3 space-y-3 border-t border-border/60 pt-3">
        <p v-if="selectedCandidate" class="text-xs text-muted-foreground">
          {{ t('auth.cdp_port_label') }}: {{ selectedCandidate.port }}
        </p>
        <div class="space-y-2">
          <label for="client-picker-manual-port" class="text-xs font-medium text-muted-foreground">
            {{ t('auth.cdp_port_label') }}
          </label>
          <div class="flex gap-2">
            <Input
              id="client-picker-manual-port"
              v-model.number="manualPort"
              type="number"
              min="1024"
              max="65535"
              :placeholder="t('auth.cdp_port_placeholder')"
              :disabled="manualChecking || mutationBusy"
              @keydown.enter.prevent="checkManualPort"
            />
            <Button type="button" variant="outline" size="sm" class="shrink-0 gap-1.5" :disabled="manualChecking || mutationBusy" @click="checkManualPort">
              <Loader2 v-if="manualChecking" class="h-3.5 w-3.5 animate-spin" aria-hidden="true" />
              <span v-else>{{ t('auth.cdp_port_detect') }}</span>
            </Button>
          </div>
          <p v-if="manualError" class="text-xs text-destructive" role="alert">{{ manualError }}</p>
        </div>
        <p v-if="currentPreviewUser" class="break-all text-xs text-muted-foreground">
          {{ t('auth.client_picker_verified_id', { id: currentPreviewUser.id }) }}
        </p>
      </div>
    </details>

    <div class="flex justify-end border-t border-border/60 pt-3">
      <Button type="button" variant="ghost" size="sm" :disabled="mutationBusy" @click="handleCancel">
        {{ t('dialog.cancel') }}
      </Button>
    </div>

    <AlertDialog :open="restartDialogOpen" @update:open="restartDialogOpen = $event">
      <AlertDialogContent class="max-w-md">
        <AlertDialogHeader>
          <AlertDialogTitle>{{ t('settings.cdp_dialog_title_disconnected') }}</AlertDialogTitle>
          <AlertDialogDescription>
            {{ t('settings.cdp_dialog_desc_disconnected') }}
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel @click="pendingRestart = null">{{ t('dialog.cancel') }}</AlertDialogCancel>
          <AlertDialogAction @click="confirmRestart">{{ t('settings.cdp_dialog_confirm') }}</AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  </section>
</template>
