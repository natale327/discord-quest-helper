<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { Check, Globe, Loader2, Shield, TestTube, Trash2 } from 'lucide-vue-next'
import { useI18n } from 'vue-i18n'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import {
  getProxySettings,
  setProxySettings,
  clearProxyCredentials,
  testProxyConnection,
  type ProxyMode,
  type ProxySettingsDto,
  type ProxyTestResult,
} from '@/api/tauri'
import SettingsSectionCard from './SettingsSectionCard.vue'
import SettingsStatusPanel from './SettingsStatusPanel.vue'
import { cn } from '@/lib/utils'
import { settingToneClass, type SettingsTone } from './settingTones'

const { t } = useI18n()

// State
const loading = ref(false)
const saving = ref(false)
const testing = ref(false)
const clearing = ref(false)
const loadError = ref('')
const saveError = ref('')
const testResult = ref<ProxyTestResult | null>(null)
const testError = ref('')
const clearError = ref('')

// Form state
const mode = ref<ProxyMode>('system')
const endpoint = ref('')
const noProxy = ref('')
const username = ref('')
const password = ref('')
const hasCredentials = ref(false)

// Computed
const isCustomMode = computed(() => mode.value === 'custom')

const modeTone = computed<SettingsTone>(() => {
  if (mode.value === 'custom') return 'warning'
  if (mode.value === 'direct') return 'neutral'
  return 'success'
})

const statusTone = computed<SettingsTone>(() => {
  if (loading.value) return 'info'
  if (loadError.value) return 'danger'
  if (mode.value === 'custom' && !endpoint.value.trim()) return 'warning'
  return 'success'
})

const statusIcon = computed(() => {
  if (loading.value) return Loader2
  return Globe
})

// Methods
async function loadSettings() {
  loading.value = true
  loadError.value = ''
  try {
    const settings = await getProxySettings()
    applySettings(settings)
  } catch (e) {
    loadError.value = e instanceof Error ? e.message : String(e)
  } finally {
    loading.value = false
  }
}

function applySettings(settings: ProxySettingsDto) {
  mode.value = settings.mode
  endpoint.value = settings.endpoint ?? ''
  noProxy.value = settings.noProxy ?? ''
  hasCredentials.value = settings.hasCredentials
  // Clear credential inputs after loading
  username.value = ''
  password.value = ''
}

async function handleSave() {
  if (saving.value) return
  
  saving.value = true
  saveError.value = ''
  testResult.value = null
  testError.value = ''
  
  try {
    const input: any = { mode: mode.value }
    
    if (mode.value === 'custom') {
      if (endpoint.value.trim()) {
        input.endpoint = endpoint.value.trim()
      }
      if (noProxy.value.trim()) {
        input.noProxy = noProxy.value.trim()
      }
      if (username.value) {
        input.username = username.value
      }
      if (password.value) {
        input.password = password.value
      }
    }
    
    const result = await setProxySettings(input)
    applySettings(result)
    
    // Clear credential inputs immediately after successful save
    username.value = ''
    password.value = ''
  } catch (e) {
    saveError.value = e instanceof Error ? e.message : String(e)
  } finally {
    saving.value = false
  }
}

async function handleTest() {
  if (testing.value) return
  
  testing.value = true
  testResult.value = null
  testError.value = ''
  
  try {
    const result = await testProxyConnection()
    testResult.value = result
  } catch (e) {
    testError.value = e instanceof Error ? e.message : String(e)
  } finally {
    testing.value = false
  }
}

async function handleClearCredentials() {
  if (clearing.value) return
  
  clearing.value = true
  clearError.value = ''
  
  try {
    const result = await clearProxyCredentials()
    applySettings(result)
  } catch (e) {
    clearError.value = e instanceof Error ? e.message : String(e)
  } finally {
    clearing.value = false
  }
}

function handleModeChange(newMode: ProxyMode) {
  mode.value = newMode
  // Clear test results when mode changes
  testResult.value = null
  testError.value = ''
}

onMounted(() => {
  loadSettings()
})
</script>

<template>
  <SettingsSectionCard
    :title="t('settings.proxy_title')"
    :description="t('settings.proxy_desc')"
    :icon="Globe"
    :tone="modeTone"
    content-class="space-y-5"
  >
    <!-- Status Panel -->
    <SettingsStatusPanel :tone="statusTone" :icon="statusIcon">
      <div class="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
        <span class="min-w-0">
          <template v-if="loading">{{ t('settings.proxy_loading') }}</template>
          <template v-else-if="loadError">{{ loadError }}</template>
          <template v-else-if="mode === 'system'">{{ t('settings.proxy_mode_system_desc') }}</template>
          <template v-else-if="mode === 'direct'">{{ t('settings.proxy_mode_direct_desc') }}</template>
          <template v-else-if="mode === 'custom' && endpoint">
            {{ t('settings.proxy_mode_custom_active', { endpoint }) }}
            <span v-if="hasCredentials" class="ml-2">
              <Badge variant="outline" :class="settingToneClass.success.badge">
                <Shield class="mr-1 h-3 w-3" />
                {{ t('settings.proxy_credentials_saved') }}
              </Badge>
            </span>
          </template>
          <template v-else-if="mode === 'custom'">{{ t('settings.proxy_mode_custom_desc') }}</template>
        </span>
        <Button
          variant="ghost"
          size="sm"
          class="h-8 gap-2 text-muted-foreground hover:text-foreground"
          @click="loadSettings"
          :disabled="loading"
        >
          <Loader2 v-if="loading" class="h-4 w-4 animate-spin" />
          <template v-else>{{ t('general.refresh') }}</template>
        </Button>
      </div>
    </SettingsStatusPanel>

    <!-- Mode Selection -->
    <div class="space-y-3">
      <Label>{{ t('settings.proxy_mode_label') }}</Label>
      <div class="grid gap-2 sm:grid-cols-3">
        <button
          type="button"
          :class="cn(
            'flex flex-col items-start gap-1 rounded-lg border p-3 text-left transition-colors',
            mode === 'system'
              ? 'border-primary bg-primary/5 text-primary'
              : 'border-border bg-card hover:bg-muted'
          )"
          @click="handleModeChange('system')"
        >
          <span class="text-sm font-semibold">{{ t('settings.proxy_mode_system') }}</span>
          <span class="text-xs text-muted-foreground">{{ t('settings.proxy_mode_system_short') }}</span>
        </button>
        <button
          type="button"
          :class="cn(
            'flex flex-col items-start gap-1 rounded-lg border p-3 text-left transition-colors',
            mode === 'direct'
              ? 'border-primary bg-primary/5 text-primary'
              : 'border-border bg-card hover:bg-muted'
          )"
          @click="handleModeChange('direct')"
        >
          <span class="text-sm font-semibold">{{ t('settings.proxy_mode_direct') }}</span>
          <span class="text-xs text-muted-foreground">{{ t('settings.proxy_mode_direct_short') }}</span>
        </button>
        <button
          type="button"
          :class="cn(
            'flex flex-col items-start gap-1 rounded-lg border p-3 text-left transition-colors',
            mode === 'custom'
              ? 'border-primary bg-primary/5 text-primary'
              : 'border-border bg-card hover:bg-muted'
          )"
          @click="handleModeChange('custom')"
        >
          <span class="text-sm font-semibold">{{ t('settings.proxy_mode_custom') }}</span>
          <span class="text-xs text-muted-foreground">{{ t('settings.proxy_mode_custom_short') }}</span>
        </button>
      </div>
    </div>

    <!-- Custom Mode Fields -->
    <div v-if="isCustomMode" class="space-y-4 rounded-lg border border-border/60 bg-muted/20 p-4">
      <!-- Endpoint -->
      <div class="space-y-2">
        <Label for="proxy-endpoint">{{ t('settings.proxy_endpoint') }}</Label>
        <Input
          id="proxy-endpoint"
          v-model="endpoint"
          type="text"
          placeholder="http://proxy.example:8080"
          :disabled="saving"
        />
        <p class="text-xs text-muted-foreground">{{ t('settings.proxy_endpoint_hint') }}</p>
      </div>

      <!-- No Proxy -->
      <div class="space-y-2">
        <Label for="proxy-no-proxy">{{ t('settings.proxy_no_proxy') }}</Label>
        <Input
          id="proxy-no-proxy"
          v-model="noProxy"
          type="text"
          placeholder="localhost,127.0.0.1,.example.com"
          :disabled="saving"
        />
        <p class="text-xs text-muted-foreground">{{ t('settings.proxy_no_proxy_hint') }}</p>
      </div>

      <!-- Credentials -->
      <div class="space-y-3 border-t border-border/40 pt-4">
        <div class="flex items-center justify-between">
          <Label>{{ t('settings.proxy_credentials') }}</Label>
          <Button
            v-if="hasCredentials"
            variant="outline"
            size="sm"
            :class="cn('gap-2', settingToneClass.danger.buttonSoft)"
            :disabled="clearing || saving"
            @click="handleClearCredentials"
          >
            <Loader2 v-if="clearing" class="h-3 w-3 animate-spin" />
            <Trash2 v-else class="h-3 w-3" />
            {{ t('settings.proxy_clear_credentials') }}
          </Button>
        </div>
        
        <p class="text-xs text-muted-foreground">{{ t('settings.proxy_credentials_desc') }}</p>

        <div class="grid gap-3 sm:grid-cols-2">
          <div class="space-y-2">
            <Label for="proxy-username" class="text-xs">{{ t('settings.proxy_username') }}</Label>
            <Input
              id="proxy-username"
              v-model="username"
              type="text"
              autocomplete="off"
              :disabled="saving"
            />
          </div>
          <div class="space-y-2">
            <Label for="proxy-password" class="text-xs">{{ t('settings.proxy_password') }}</Label>
            <Input
              id="proxy-password"
              v-model="password"
              type="password"
              autocomplete="new-password"
              :disabled="saving"
            />
          </div>
        </div>

        <div v-if="clearError" class="rounded border border-destructive/30 bg-destructive/10 p-2 text-xs text-destructive">
          {{ clearError }}
        </div>
      </div>
    </div>

    <!-- Actions -->
    <div class="flex flex-wrap items-center gap-2">
      <Button
        :disabled="saving || loading"
        @click="handleSave"
      >
        <Loader2 v-if="saving" class="mr-2 h-4 w-4 animate-spin" />
        {{ t('settings.proxy_save') }}
      </Button>
      <Button
        v-if="isCustomMode"
        variant="outline"
        :class="cn('gap-2', settingToneClass.info.buttonSoft)"
        :disabled="testing || saving || loading || !endpoint.trim()"
        @click="handleTest"
      >
        <Loader2 v-if="testing" class="h-4 w-4 animate-spin" />
        <TestTube v-else class="h-4 w-4" />
        {{ t('settings.proxy_test') }}
      </Button>
    </div>

    <!-- Save Error -->
    <SettingsStatusPanel v-if="saveError" tone="danger">
      {{ saveError }}
    </SettingsStatusPanel>

    <!-- Test Result -->
    <div v-if="testResult || testError" class="space-y-2">
      <SettingsStatusPanel v-if="testError" tone="danger">
        {{ testError }}
      </SettingsStatusPanel>
      <SettingsStatusPanel v-else-if="testResult" :tone="testResult.ok ? 'success' : 'danger'">
        <div class="flex items-center gap-2">
          <Check v-if="testResult.ok" class="h-4 w-4" />
          <span>
            {{ testResult.ok ? t('settings.proxy_test_success') : t('settings.proxy_test_failed') }}
            <span v-if="testResult.status" class="ml-1 font-mono text-xs">({{ testResult.status }})</span>
          </span>
        </div>
        <p v-if="testResult.message" class="mt-1 text-xs">{{ testResult.message }}</p>
      </SettingsStatusPanel>
    </div>
  </SettingsSectionCard>
</template>
