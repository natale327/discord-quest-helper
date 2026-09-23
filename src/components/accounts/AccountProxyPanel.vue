<script setup lang="ts">
import { ref, computed, onMounted, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { Globe, Loader2, Save, Trash2, KeyRound } from 'lucide-vue-next'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Badge } from '@/components/ui/badge'
import {
  getAccountProxySettings,
  setAccountProxyOverride,
  clearAccountProxyOverride,
  type AccountProxySettingsDto,
  type AccountProxyOverrideInput,
  type ProxyMode,
} from '@/api/tauri'
import { cn } from '@/lib/utils'

const props = defineProps<{
  accountId: string
  accountName: string
}>()

const { t } = useI18n()

// State
const loading = ref(false)
const mutating = ref(false) // Mutual exclusion for save/clear
const loadError = ref('')
const mutationError = ref('')

// Proxy settings from backend
const settings = ref<AccountProxySettingsDto | null>(null)

// Form state (for editing)
const mode = ref<ProxyMode | 'inherit'>('inherit')
const endpoint = ref('')
const noProxy = ref('')
const username = ref('')
const password = ref('')

// Track if credentials have been touched
const credentialsTouched = ref(false)

// Generation tracking for accountId changes
let currentGeneration = 0

// Computed
const isCustomMode = computed(() => mode.value === 'custom')
const isInheritMode = computed(() => mode.value === 'inherit')
const hasOverride = computed(() => settings.value?.hasOverride ?? false)
const hasCredentials = computed(() => {
  if (hasOverride.value) {
    return settings.value?.overrideHasCredentials ?? false
  }
  return settings.value?.effectiveHasCredentials ?? false
})

const effectiveModeLabel = computed(() => {
  if (!settings.value) return ''
  const mode = settings.value.effectiveMode
  return t(`settings.proxy_mode_${mode}`)
})

const canSave = computed(() => {
  if (mutating.value || loading.value) return false
  if (isCustomMode.value && !endpoint.value.trim()) return false
  // If in inherit mode and no override exists, disable save (no-op)
  if (isInheritMode.value && !hasOverride.value) return false
  return true
})

const canClear = computed(() => {
  if (mutating.value || loading.value) return false
  return hasOverride.value
})

// Methods
async function loadSettings() {
  const generation = ++currentGeneration
  // Snapshot the account id BEFORE the await: a later account switch must never
  // make this request read the newly selected account.
  const accountId = props.accountId
  loading.value = true
  loadError.value = ''

  // Clear form immediately when loading
  settings.value = null
  mode.value = 'inherit'
  endpoint.value = ''
  noProxy.value = ''
  username.value = ''
  password.value = ''
  credentialsTouched.value = false

  try {
    const result = await getAccountProxySettings(accountId)

    // Check if this response is still for the current account
    if (generation !== currentGeneration) return

    settings.value = result
    applySettingsToForm(result)
  } catch (e) {
    // Check if this error is still for the current account
    if (generation !== currentGeneration) return
    loadError.value = e instanceof Error ? e.message : String(e)
  } finally {
    // Only clear loading if this is still the current account
    if (generation === currentGeneration) {
      loading.value = false
    }
  }
}

function applySettingsToForm(dto: AccountProxySettingsDto) {
  if (dto.hasOverride && dto.overrideMode) {
    mode.value = dto.overrideMode
    endpoint.value = dto.overrideEndpoint ?? ''
    noProxy.value = dto.overrideNoProxy ?? ''
  } else {
    mode.value = 'inherit'
    endpoint.value = ''
    noProxy.value = ''
  }
  // Clear credential inputs
  username.value = ''
  password.value = ''
  credentialsTouched.value = false
}

function validateForm(): string | null {
  if (isCustomMode.value && !endpoint.value.trim()) {
    return t('accounts.proxy_endpoint_required')
  }

  // Validate credentials: if either is supplied, both must be supplied
  const usernameFilled = username.value.trim().length > 0
  const passwordFilled = password.value.trim().length > 0
  if (credentialsTouched.value && (usernameFilled !== passwordFilled)) {
    return t('accounts.proxy_credentials_both_required')
  }

  return null
}

async function handleSave() {
  if (!canSave.value) return

  const validationError = validateForm()
  if (validationError) {
    mutationError.value = validationError
    return
  }

  const generation = currentGeneration
  // Snapshot the account id BEFORE any await so this request always targets the
  // account the user acted on, never whichever account is selected later.
  const accountId = props.accountId
  mutating.value = true
  mutationError.value = ''

  try {
    // If in inherit mode and override exists, clear the override
    if (isInheritMode.value) {
      if (hasOverride.value) {
        const result = await clearAccountProxyOverride(accountId)

        // Check if this response is still for the current account
        if (generation !== currentGeneration) return

        settings.value = result
        applySettingsToForm(result)
      }
      // If no override exists, this is a no-op
      return
    }

    const input: AccountProxyOverrideInput = {}
    input.mode = mode.value as ProxyMode

    if (isCustomMode.value) {
      input.endpoint = endpoint.value.trim()
      if (noProxy.value.trim()) {
        input.noProxy = noProxy.value.trim()
      }

      // Only send credentials if they were touched AND both are filled
      if (credentialsTouched.value) {
        const usernameFilled = username.value.trim().length > 0
        const passwordFilled = password.value.trim().length > 0
        if (usernameFilled && passwordFilled) {
          input.username = username.value.trim()
          input.password = password.value.trim()
        }
      }
    }

    const result = await setAccountProxyOverride(accountId, input)

    // Check if this response is still for the current account
    if (generation !== currentGeneration) return

    settings.value = result
    applySettingsToForm(result)
  } catch (e) {
    // Check if this error is still for the current account
    if (generation !== currentGeneration) return
    mutationError.value = e instanceof Error ? e.message : String(e)
  } finally {
    // Only clear mutating if this is still the current account
    if (generation === currentGeneration) {
      mutating.value = false
    }
  }
}

async function handleClear() {
  if (!canClear.value) return

  const generation = currentGeneration
  const accountId = props.accountId
  mutating.value = true
  mutationError.value = ''

  try {
    const result = await clearAccountProxyOverride(accountId)

    // Check if this response is still for the current account
    if (generation !== currentGeneration) return

    settings.value = result
    applySettingsToForm(result)
  } catch (e) {
    // Check if this error is still for the current account
    if (generation !== currentGeneration) return
    mutationError.value = e instanceof Error ? e.message : String(e)
  } finally {
    // Only clear mutating if this is still the current account
    if (generation === currentGeneration) {
      mutating.value = false
    }
  }
}

function onCredentialsInput() {
  credentialsTouched.value = true
}

// Load settings on mount
onMounted(() => {
  void loadSettings()
})

// Reload when account changes - clear form immediately
watch(() => props.accountId, () => {
  // Synchronously invalidate in-flight responses and clear form
  currentGeneration++
  settings.value = null
  mode.value = 'inherit'
  endpoint.value = ''
  noProxy.value = ''
  username.value = ''
  password.value = ''
  credentialsTouched.value = false
  loadError.value = ''
  mutationError.value = ''
  mutating.value = false

  void loadSettings()
})
</script>

<template>
  <div class="space-y-4">
    <!-- Loading state -->
    <div v-if="loading" class="flex items-center justify-center py-8">
      <Loader2 class="h-6 w-6 animate-spin text-muted-foreground" />
    </div>

    <!-- Error state -->
    <div v-else-if="loadError" class="rounded-lg border border-destructive/50 bg-destructive/10 p-4">
      <p class="text-sm text-destructive">{{ loadError }}</p>
      <Button variant="outline" size="sm" class="mt-2" @click="loadSettings">
        {{ t('general.retry') }}
      </Button>
    </div>

    <!-- Settings form -->
    <div v-else-if="settings" class="space-y-4">
      <!-- Current status -->
      <div class="flex items-start justify-between gap-4">
        <div class="flex-1 space-y-1">
          <div class="flex items-center gap-2">
            <Globe class="h-4 w-4 text-muted-foreground" />
            <span class="text-sm font-medium">{{ t('accounts.proxy_status') }}</span>
            <Badge v-if="hasOverride" variant="default" class="text-xs">
              {{ t('accounts.proxy_custom') }}
            </Badge>
            <Badge v-else variant="outline" class="text-xs">
              {{ t('accounts.proxy_inherit') }}
            </Badge>
          </div>
          <p class="text-xs text-muted-foreground">
            {{ t('accounts.proxy_effective_mode', { mode: effectiveModeLabel }) }}
            <span v-if="settings.effectiveHasCredentials" class="ml-1">
              · {{ t('accounts.proxy_has_credentials') }}
            </span>
          </p>
        </div>
      </div>

      <!-- Mode selection -->
      <div class="space-y-2">
        <Label>{{ t('accounts.proxy_mode_label') }}</Label>
        <div class="grid grid-cols-2 gap-2 sm:grid-cols-4">
          <button
            type="button"
            :class="cn(
              'rounded-lg border px-3 py-2 text-sm font-medium transition-colors',
              mode === 'inherit'
                ? 'border-primary bg-primary/10 text-primary'
                : 'border-border bg-background hover:bg-accent hover:text-accent-foreground',
            )"
            @click="mode = 'inherit'"
          >
            {{ t('accounts.proxy_mode_inherit') }}
          </button>
          <button
            type="button"
            :class="cn(
              'rounded-lg border px-3 py-2 text-sm font-medium transition-colors',
              mode === 'system'
                ? 'border-primary bg-primary/10 text-primary'
                : 'border-border bg-background hover:bg-accent hover:text-accent-foreground',
            )"
            @click="mode = 'system'"
          >
            {{ t('settings.proxy_mode_system') }}
          </button>
          <button
            type="button"
            :class="cn(
              'rounded-lg border px-3 py-2 text-sm font-medium transition-colors',
              mode === 'direct'
                ? 'border-primary bg-primary/10 text-primary'
                : 'border-border bg-background hover:bg-accent hover:text-accent-foreground',
            )"
            @click="mode = 'direct'"
          >
            {{ t('settings.proxy_mode_direct') }}
          </button>
          <button
            type="button"
            :class="cn(
              'rounded-lg border px-3 py-2 text-sm font-medium transition-colors',
              mode === 'custom'
                ? 'border-primary bg-primary/10 text-primary'
                : 'border-border bg-background hover:bg-accent hover:text-accent-foreground',
            )"
            @click="mode = 'custom'"
          >
            {{ t('settings.proxy_mode_custom') }}
          </button>
        </div>
      </div>

      <!-- Custom mode fields -->
      <div v-if="isCustomMode" class="space-y-3 rounded-lg border bg-muted/30 p-4">
        <div class="space-y-2">
          <Label for="endpoint">{{ t('settings.proxy_endpoint') }}</Label>
          <Input
            id="endpoint"
            v-model="endpoint"
            type="text"
            placeholder="http://proxy.example.com:8080"
            :disabled="mutating"
          />
          <p class="text-xs text-muted-foreground">{{ t('settings.proxy_endpoint_hint') }}</p>
        </div>

        <div class="space-y-2">
          <Label for="noProxy">{{ t('settings.proxy_no_proxy') }}</Label>
          <Input
            id="noProxy"
            v-model="noProxy"
            type="text"
            placeholder="localhost,127.0.0.1,.example.com"
            :disabled="mutating"
          />
          <p class="text-xs text-muted-foreground">{{ t('settings.proxy_no_proxy_hint') }}</p>
        </div>

        <div class="space-y-3 border-t pt-3">
          <div class="flex items-center gap-2">
            <KeyRound class="h-4 w-4 text-muted-foreground" />
            <span class="text-sm font-medium">{{ t('settings.proxy_credentials') }}</span>
            <Badge v-if="hasCredentials && !credentialsTouched" variant="secondary" class="text-xs">
              {{ t('settings.proxy_credentials_saved') }}
            </Badge>
          </div>
          <p class="text-xs text-muted-foreground">{{ t('settings.proxy_credentials_desc') }}</p>

          <div class="grid gap-3 sm:grid-cols-2">
            <div class="space-y-2">
              <Label for="username">{{ t('settings.proxy_username') }}</Label>
              <Input
                id="username"
                v-model="username"
                type="text"
                autocomplete="off"
                :disabled="mutating"
                @input="onCredentialsInput"
              />
            </div>
            <div class="space-y-2">
              <Label for="password">{{ t('settings.proxy_password') }}</Label>
              <Input
                id="password"
                v-model="password"
                type="password"
                autocomplete="new-password"
                :disabled="mutating"
                @input="onCredentialsInput"
              />
            </div>
          </div>
        </div>
      </div>

      <!-- Error messages -->
      <div v-if="mutationError" class="rounded-lg border border-destructive/50 bg-destructive/10 p-3">
        <p class="text-sm text-destructive">{{ mutationError }}</p>
      </div>

      <!-- Action buttons -->
      <div class="flex flex-wrap gap-2">
        <Button
          :disabled="!canSave"
          @click="handleSave"
        >
          <Loader2 v-if="mutating" class="mr-2 h-4 w-4 animate-spin" />
          <Save v-else class="mr-2 h-4 w-4" />
          {{ t('settings.proxy_save') }}
        </Button>
        <Button
          v-if="hasOverride"
          variant="outline"
          :disabled="!canClear"
          @click="handleClear"
        >
          <Loader2 v-if="mutating" class="mr-2 h-4 w-4 animate-spin" />
          <Trash2 v-else class="mr-2 h-4 w-4" />
          {{ t('accounts.proxy_clear_override') }}
        </Button>
      </div>
    </div>
  </div>
</template>
