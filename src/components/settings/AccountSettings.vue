<script setup lang="ts">
import { ref, computed, onMounted } from 'vue'
import { CheckCircle2, Loader2, LogOut, Plus, ShieldCheck, UserRound, Users, Settings2, RadioTower } from 'lucide-vue-next'
import { useI18n } from 'vue-i18n'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Badge } from '@/components/ui/badge'
import { useAuthStore } from '@/stores/auth'
import { useQuestsStore } from '@/stores/quests'
import SettingsSectionCard from './SettingsSectionCard.vue'
import SettingsStatusPanel from './SettingsStatusPanel.vue'
import { settingToneClass } from './settingTones'
import AccountListItem from '../accounts/AccountListItem.vue'
import RemoveAccountDialog from '../accounts/RemoveAccountDialog.vue'
import AccountProxyPanel from '../accounts/AccountProxyPanel.vue'
import type { AccountSummary } from '../accounts/AccountListItem.vue'

const { t } = useI18n()
const authStore = useAuthStore()
const questsStore = useQuestsStore()

const removeDialogOpen = ref(false)
const accountToRemove = ref<AccountSummary | null>(null)
const busyAccountId = ref<string | null>(null)
const proxyAccountId = ref<string | null>(null)
const portEditAccountId = ref<string | null>(null)
const portEditValue = ref<number>(9223)
const portEditError = ref<string | null>(null)

// Load accounts on mount
onMounted(() => {
  void authStore.loadAccounts()
})

const accounts = computed(() => authStore.accounts)
const activeAccountId = computed(() => authStore.activeAccountId)

const proxyAccount = computed(() => {
  if (!proxyAccountId.value) return null
  return accounts.value.find(a => a.id === proxyAccountId.value) ?? null
})

async function handleCdpLogin() {
  await authStore.loginViaCdp()
  if (authStore.user) emit('navigateToHome')
}

function handleRemoveRequest(id: string) {
  const account = accounts.value.find((a) => a.id === id)
  if (account) {
    accountToRemove.value = account
    removeDialogOpen.value = true
  }
}

async function handleRemoveConfirm(id: string) {
  if (busyAccountId.value) return
  busyAccountId.value = id
  try {
    await authStore.removeAccount(id)
    removeDialogOpen.value = false
    accountToRemove.value = null
    // If we're removing the account being configured for proxy, close the panel
    if (proxyAccountId.value === id) {
      proxyAccountId.value = null
    }
  } finally {
    busyAccountId.value = null
  }
}

function handleRemoveCancel() {
  removeDialogOpen.value = false
  accountToRemove.value = null
}

async function handleSelectAccount(id: string) {
  if (busyAccountId.value) return
  busyAccountId.value = id
  try {
    await authStore.activateAccount(id)
  } finally {
    busyAccountId.value = null
  }
}

function handleAddAccount() {
  emit('addAccount')
}

function handleConfigureProxy(accountId: string) {
  proxyAccountId.value = accountId
}

function handleCloseProxy() {
  proxyAccountId.value = null
}

function handleEditPort(accountId: string) {
  portEditAccountId.value = accountId
  portEditValue.value = authStore.portForAccount(accountId)
  portEditError.value = null
}

function handleCancelPortEdit() {
  portEditAccountId.value = null
  portEditValue.value = 9223
  portEditError.value = null
}

function handleSavePort() {
  if (!portEditAccountId.value) return

  // Validate port is an integer and in valid range
  if (!Number.isInteger(portEditValue.value) || portEditValue.value < 1024 || portEditValue.value > 65535) {
    portEditError.value = t('accounts.port_invalid_range')
    return
  }

  authStore.setAccountPort(portEditAccountId.value, portEditValue.value)
  handleCancelPortEdit()
}

const emit = defineEmits<{
  navigateToHome: []
  addAccount: []
}>()
</script>

<template>
  <SettingsSectionCard
    :title="t('settings.account_title')"
    :description="t('settings.account_desc')"
    :icon="authStore.user ? ShieldCheck : UserRound"
    :tone="authStore.user ? 'success' : 'primary'"
  >
      <div v-if="authStore.user" class="space-y-3">
        <SettingsStatusPanel tone="success" :icon="CheckCircle2">
          {{ t('auth.authenticated_as') }} <span class="font-semibold">{{ authStore.user.username }}</span>
        </SettingsStatusPanel>
        <Button variant="outline" :class="['gap-2', settingToneClass.danger.buttonSoft]" @click="authStore.logout">
          <LogOut class="h-4 w-4" />
          {{ t('general.logout') }}
        </Button>
      </div>

      <div v-else class="space-y-4">
        <Button
          @click="handleCdpLogin"
          :disabled="authStore.loading"
          size="lg"
          class="w-full gap-2 shadow-sm"
        >
          <Loader2 v-if="authStore.loading" class="h-4 w-4 animate-spin" />
          {{ t('auth.cdp_login') }}
        </Button>

        <SettingsStatusPanel v-if="authStore.error" tone="danger">
          {{ authStore.error }}
        </SettingsStatusPanel>
      </div>
  </SettingsSectionCard>

  <!-- Account Management Section -->
  <SettingsSectionCard
    :title="t('accounts.manage_title')"
    :description="t('accounts.manage_desc')"
    :icon="Users"
    tone="neutral"
  >
    <div v-if="accounts.length === 0" class="py-8 text-center">
      <Users class="mx-auto h-12 w-12 text-muted-foreground/50" />
      <p class="mt-3 text-sm font-medium">{{ t('accounts.manage_empty') }}</p>
      <p class="mt-1 text-xs text-muted-foreground">{{ t('accounts.manage_empty_desc') }}</p>
      <Button class="mt-4 gap-2" @click="handleAddAccount">
        <Plus class="h-4 w-4" />
        {{ t('accounts.add_account') }}
      </Button>
    </div>

    <div v-else class="space-y-4">
      <div class="space-y-1">
        <AccountListItem
          v-for="account in accounts"
          :key="account.id"
          :account="account"
          :active="account.id === activeAccountId"
          :offline="!account.isAuthenticated"
          :busy="busyAccountId !== null"
          @select="handleSelectAccount"
          @remove="handleRemoveRequest"
        />
      </div>

      <Button variant="outline" class="w-full gap-2" :disabled="busyAccountId !== null" @click="handleAddAccount">
        <Plus class="h-4 w-4" />
        {{ t('accounts.add_account') }}
      </Button>
    </div>

    <RemoveAccountDialog
      v-model:open="removeDialogOpen"
      :account="accountToRemove"
      :busy="busyAccountId !== null"
      @confirm="handleRemoveConfirm"
      @cancel="handleRemoveCancel"
    />
  </SettingsSectionCard>

  <!-- Per-Account CDP Port Section (Phase 6.5) -->
  <SettingsSectionCard
    v-if="accounts.length > 0"
    :title="t('accounts.cdp_port_title')"
    :description="t('accounts.cdp_port_desc')"
    :icon="RadioTower"
    tone="neutral"
  >
    <div class="space-y-3">
      <div
        v-for="account in accounts"
        :key="account.id"
        class="flex items-center gap-3 rounded-lg border border-border/60 bg-card/40 p-3"
      >
        <div class="flex-1 min-w-0">
          <p class="text-sm font-medium truncate">
            {{ account.globalName || account.username }}
          </p>
          <div class="flex items-center gap-2 mt-1">
            <Badge variant="outline" class="text-xs">
              {{ t('accounts.cdp_port_label') }}: {{ authStore.portForAccount(account.id) }}
            </Badge>
            <span v-if="authStore.portForAccount(account.id) !== questsStore.cdpPort" class="text-xs text-muted-foreground">
              {{ t('accounts.cdp_port_override') }}
            </span>
          </div>
        </div>

        <!-- Port edit mode -->
        <template v-if="portEditAccountId === account.id">
          <div class="flex items-center gap-2">
            <Input
              v-model.number="portEditValue"
              type="number"
              min="1024"
              max="65535"
              class="w-24"
              :placeholder="t('accounts.cdp_port_placeholder')"
            />
            <Button size="sm" @click="handleSavePort">
              {{ t('general.save') }}
            </Button>
            <Button size="sm" variant="ghost" @click="handleCancelPortEdit">
              {{ t('general.cancel') }}
            </Button>
          </div>
        </template>

        <!-- Edit button -->
        <template v-else>
          <Button
            size="sm"
            variant="outline"
            @click="handleEditPort(account.id)"
          >
            {{ t('accounts.cdp_port_edit') }}
          </Button>
        </template>
      </div>

      <!-- Port validation error -->
      <div v-if="portEditError" class="rounded-md border border-destructive/50 bg-destructive/10 p-3">
        <p class="text-sm text-destructive">{{ portEditError }}</p>
      </div>

      <p class="text-xs text-muted-foreground">
        {{ t('accounts.cdp_port_global_hint', { port: questsStore.cdpPort }) }}
      </p>
    </div>
  </SettingsSectionCard>

  <!-- Account Proxy Settings Section -->
  <SettingsSectionCard
    v-if="proxyAccount"
    :title="t('accounts.proxy_title')"
    :description="t('accounts.proxy_desc', { account: proxyAccount.globalName || proxyAccount.username })"
    :icon="Settings2"
    tone="neutral"
  >
    <div class="space-y-4">
      <AccountProxyPanel
        :account-id="proxyAccount.id"
        :account-name="proxyAccount.globalName || proxyAccount.username"
      />
      <Button variant="ghost" size="sm" @click="handleCloseProxy">
        {{ t('general.close') }}
      </Button>
    </div>
  </SettingsSectionCard>

  <!-- Configure Proxy Button (shown when no proxy panel is open) -->
  <SettingsSectionCard
    v-else-if="accounts.length > 0"
    :title="t('accounts.proxy_configure_title')"
    :description="t('accounts.proxy_configure_desc')"
    :icon="Settings2"
    tone="neutral"
  >
    <div class="space-y-3">
      <p class="text-sm text-muted-foreground">
        {{ t('accounts.proxy_select_account') }}
      </p>
      <div class="flex flex-wrap gap-2">
        <Button
          v-for="account in accounts"
          :key="account.id"
          variant="outline"
          size="sm"
          @click="handleConfigureProxy(account.id)"
        >
          {{ account.globalName || account.username }}
        </Button>
      </div>
    </div>
  </SettingsSectionCard>
</template>
