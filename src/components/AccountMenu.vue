<script setup lang="ts">
import { ref, computed, onMounted } from 'vue'
import { useI18n } from 'vue-i18n'
import { useAuthStore } from '@/stores/auth'
import AccountSwitcher from './accounts/AccountSwitcher.vue'
import RemoveAccountDialog from './accounts/RemoveAccountDialog.vue'
import type { AccountSummary } from './accounts/AccountListItem.vue'

const authStore = useAuthStore()
const { t } = useI18n()

const open = ref(false)
const removeDialogOpen = ref(false)
const accountToRemove = ref<AccountSummary | null>(null)
const busyAccountId = ref<string | null>(null)
const accountSwitchError = ref<string | null>(null)

// Load accounts on mount
onMounted(() => {
  void authStore.loadAccounts()
})

const accounts = computed(() => authStore.accounts)
const activeAccountId = computed(() => authStore.activeAccountId)

// Show the menu if we have any accounts (even if not authenticated)
const hasAccounts = computed(() => accounts.value.length > 0)

function handleLogout() {
  accountSwitchError.value = null
  open.value = false
  void authStore.logout()
}

async function handleSelect(id: string) {
  if (busyAccountId.value) return
  accountSwitchError.value = null
  busyAccountId.value = id
  try {
    const switched = await authStore.switchOnlineAccount(id)
    if (!switched) {
      accountSwitchError.value = authStore.error || t('accounts.switch_failed')
      open.value = true
    }
  } catch (error) {
    accountSwitchError.value = error instanceof Error ? error.message : t('accounts.switch_failed')
    open.value = true
  } finally {
    busyAccountId.value = null
  }
}

function handleReconnect(id: string) {
  accountSwitchError.value = null
  open.value = false
  emit('reconnectAccount', id)
}

function handleAdd() {
  // Add account flow will be handled by parent (App.vue) via event
  accountSwitchError.value = null
  open.value = false
  emit('addAccount')
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
  } finally {
    busyAccountId.value = null
  }
}

function handleRemoveCancel() {
  removeDialogOpen.value = false
  accountToRemove.value = null
}

const emit = defineEmits<{
  addAccount: []
  reconnectAccount: [id: string]
}>()
</script>

<template>
  <div v-if="hasAccounts">
    <AccountSwitcher
      :accounts="accounts"
      :active-account-id="activeAccountId"
      :busy-account-id="busyAccountId"
      :error="accountSwitchError"
      :open="open"
      @update:open="open = $event"
      @select="handleSelect"
      @reconnect="handleReconnect"
      @add="handleAdd"
      @logout="handleLogout"
      @remove="handleRemoveRequest"
    />

    <RemoveAccountDialog
      v-model:open="removeDialogOpen"
      :account="accountToRemove"
      :busy="busyAccountId !== null"
      @confirm="handleRemoveConfirm"
      @cancel="handleRemoveCancel"
    />
  </div>
</template>
