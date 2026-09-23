<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import AccountListItem from './AccountListItem.vue'
import type { AccountSummary } from './AccountListItem.vue'
import { ChevronDown, LogOut, Plus, Users } from 'lucide-vue-next'

const props = withDefaults(
  defineProps<{
    accounts: AccountSummary[]
    activeAccountId?: string | null
    busyAccountId?: string | null
    open?: boolean
  }>(),
  {
    activeAccountId: null,
    busyAccountId: null,
    open: false,
  },
)

const emit = defineEmits<{
  'update:open': [value: boolean]
  select: [id: string]
  add: []
  logout: []
  remove: [id: string]
}>()

const { t } = useI18n()

const containerRef = ref<HTMLElement | null>(null)

const isOpen = computed({
  get: () => props.open,
  set: (value) => emit('update:open', value),
})

const activeAccount = computed(() =>
  props.accounts.find((a) => a.id === props.activeAccountId) ?? null,
)

const sortedAccounts = computed(() => {
  const list = [...props.accounts]
  list.sort((a, b) => {
    if (a.id === props.activeAccountId) return -1
    if (b.id === props.activeAccountId) return 1
    const aTime = a.lastUsedAtMs ?? 0
    const bTime = b.lastUsedAtMs ?? 0
    return bTime - aTime
  })
  return list
})

const triggerAvatarUrl = computed(() => {
  if (!activeAccount.value?.avatar) return null
  return `https://cdn.discordapp.com/avatars/${activeAccount.value.id}/${activeAccount.value.avatar}.png?size=128`
})

const triggerDisplayName = computed(() => activeAccount.value?.globalName || activeAccount.value?.username || '')

const triggerUsername = computed(() => activeAccount.value?.username || '')

function toggleOpen() {
  isOpen.value = !isOpen.value
}

function handleClickOutside(e: MouseEvent) {
  if (containerRef.value && !containerRef.value.contains(e.target as Node)) {
    isOpen.value = false
  }
}

function handleKeydown(e: KeyboardEvent) {
  if (e.key === 'Escape' && isOpen.value) {
    e.preventDefault()
    isOpen.value = false
  }
}

function handleSelect(id: string) {
  emit('select', id)
  isOpen.value = false
}

function handleAdd() {
  emit('add')
  isOpen.value = false
}

function handleLogout() {
  emit('logout')
  isOpen.value = false
}

function handleRemove(id: string) {
  emit('remove', id)
}

onMounted(() => {
  document.addEventListener('mousedown', handleClickOutside)
  document.addEventListener('keydown', handleKeydown)
})

onUnmounted(() => {
  document.removeEventListener('mousedown', handleClickOutside)
  document.removeEventListener('keydown', handleKeydown)
})
</script>

<template>
  <div ref="containerRef" class="relative">
    <!-- Trigger button -->
    <button
      type="button"
      class="h-10 px-2 rounded-lg inline-flex items-center gap-2 hover:bg-muted/60 transition-colors outline-none focus-visible:ring-2 focus-visible:ring-ring"
      :aria-expanded="isOpen"
      aria-haspopup="dialog"
      @click="toggleOpen"
    >
      <div v-if="activeAccount" class="relative shrink-0">
        <div class="h-8 w-8 shrink-0 overflow-hidden rounded-full bg-muted flex items-center justify-center text-sm font-medium">
          <img
            v-if="triggerAvatarUrl"
            :src="triggerAvatarUrl"
            :alt="triggerDisplayName"
            class="h-full w-full object-cover"
          />
          <span v-else>{{ triggerDisplayName ? triggerDisplayName[0].toUpperCase() : '?' }}</span>
        </div>
      </div>
      <div v-else class="h-8 w-8 shrink-0 rounded-full bg-muted flex items-center justify-center">
        <Users class="h-4 w-4 text-muted-foreground" />
      </div>

      <span class="hidden md:inline-flex flex-col items-start min-w-0 leading-tight">
        <span v-if="activeAccount" class="text-sm font-medium max-w-[120px] truncate">
          {{ triggerDisplayName }}
        </span>
        <span v-else class="text-sm font-medium text-muted-foreground">
          {{ t('accounts.switcher_title') }}
        </span>
        <span v-if="triggerUsername" class="text-xs text-muted-foreground max-w-[120px] truncate">
          @{{ triggerUsername }}
        </span>
      </span>

      <ChevronDown
        class="w-4 h-4 text-muted-foreground shrink-0 hidden md:block transition-transform"
        :class="isOpen && 'rotate-180'"
      />
    </button>

    <!-- Dropdown panel -->
    <Transition
      enter-active-class="transition ease-out duration-150"
      enter-from-class="opacity-0 -translate-y-1 scale-95"
      enter-to-class="opacity-100 translate-y-0 scale-100"
      leave-active-class="transition ease-in duration-100"
      leave-from-class="opacity-100 translate-y-0 scale-100"
      leave-to-class="opacity-0 -translate-y-1 scale-95"
    >
      <div
        v-if="isOpen"
        role="dialog"
        aria-label="Account switcher"
        class="absolute right-0 top-full mt-2 z-50 w-80 rounded-lg border bg-popover text-popover-foreground shadow-md overflow-hidden"
      >
        <!-- Header -->
        <div class="px-3 py-3 border-b">
          <p class="text-sm font-semibold">{{ t('accounts.switcher_title') }}</p>
        </div>

        <!-- Account list -->
        <div class="max-h-[60vh] overflow-y-auto p-2">
          <div v-if="sortedAccounts.length === 0" class="py-8 text-center">
            <Users class="mx-auto h-10 w-10 text-muted-foreground/50" />
            <p class="mt-3 text-sm font-medium">{{ t('accounts.switcher_empty') }}</p>
            <p class="mt-1 text-xs text-muted-foreground">{{ t('accounts.switcher_empty_desc') }}</p>
          </div>

          <div v-else class="space-y-1">
            <AccountListItem
              v-for="account in sortedAccounts"
              :key="account.id"
              :account="account"
              :active="account.id === activeAccountId"
              :offline="account.id !== activeAccountId"
              :busy="busyAccountId !== null"
              @select="handleSelect"
              @remove="handleRemove"
            />
          </div>
        </div>

        <!-- Actions -->
        <div class="border-t p-2 space-y-1">
          <button
            type="button"
            class="flex w-full items-center gap-2 rounded-md px-3 py-2 text-sm font-medium transition-colors hover:bg-accent hover:text-accent-foreground outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:opacity-50 disabled:pointer-events-none"
            :disabled="busyAccountId !== null"
            @click="handleAdd"
          >
            <Plus class="h-4 w-4" />
            {{ t('accounts.add_account') }}
          </button>

          <button
            v-if="activeAccount"
            type="button"
            class="flex w-full items-center gap-2 rounded-md px-3 py-2 text-sm font-medium text-destructive transition-colors hover:bg-destructive/10 outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:opacity-50 disabled:pointer-events-none"
            :disabled="busyAccountId !== null"
            @click="handleLogout"
          >
            <LogOut class="h-4 w-4" />
            {{ t('accounts.logout') }}
          </button>
        </div>
      </div>
    </Transition>
  </div>
</template>
