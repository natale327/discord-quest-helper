<script setup lang="ts">
import { computed } from 'vue'
import { useI18n } from 'vue-i18n'
import { Avatar, AvatarFallback, AvatarImage } from '@/components/ui/avatar'
import { Button } from '@/components/ui/button'
import { cn } from '@/lib/utils'
import { Check, RadioTower, WifiOff } from 'lucide-vue-next'

export interface AccountSummary {
  id: string
  username: string
  discriminator?: string | null
  avatar?: string | null
  globalName?: string | null
  lastCdpPort?: number | null
  lastUsedAtMs?: number | null
  isAuthenticated?: boolean
}

const props = withDefaults(
  defineProps<{
    account: AccountSummary
    active?: boolean
    offline?: boolean
    busy?: boolean
    selected?: boolean
    showReconnect?: boolean
    portUnassigned?: boolean
  }>(),
  {
    active: false,
    offline: false,
    busy: false,
    selected: false,
    showReconnect: false,
    portUnassigned: false,
  },
)

const emit = defineEmits<{
  select: [id: string]
  remove: [id: string]
  reconnect: [id: string]
}>()

const { t } = useI18n()

const displayName = computed(() => props.account.globalName || props.account.username)

const avatarUrl = computed(() => {
  if (!props.account.avatar) return null
  return `https://cdn.discordapp.com/avatars/${props.account.id}/${props.account.avatar}.png?size=128`
})

const initials = computed(() => {
  const name = displayName.value
  return name ? name[0].toUpperCase() : '?'
})

const tagline = computed(() => {
  if (props.account.discriminator && props.account.discriminator !== '0') {
    return `${props.account.username}#${props.account.discriminator}`
  }
  return `@${props.account.username}`
})

// Determine if account is offline: use isAuthenticated field if available, otherwise fall back to offline prop
const isOffline = computed(() => {
  if (props.account.isAuthenticated !== undefined) {
    return !props.account.isAuthenticated
  }
  return props.offline
})

const statusLabel = computed(() => {
  if (props.active) return t('accounts.active')
  if (isOffline.value) return t('accounts.offline')
  return ''
})

function handleClick() {
  if (props.busy || isOffline.value) return
  emit('select', props.account.id)
}

const canSelect = computed(() => !isOffline.value)
const reconnectActionKey = computed(() => (
  props.portUnassigned ? 'accounts.choose_client' : 'accounts.reconnect_account'
))
const reconnectActionAriaLabel = computed(() => (
  props.portUnassigned
    ? t('accounts.choose_client')
    : t('accounts.reconnect_account_named', { account: displayName.value })
))
</script>

<template>
  <div class="flex min-w-0 items-center gap-2 rounded-lg">
    <component
      :is="canSelect ? 'button' : 'div'"
      :type="canSelect ? 'button' : undefined"
      :disabled="canSelect ? busy : undefined"
      :class="cn(
        'group flex min-w-0 flex-1 items-center gap-3 rounded-lg px-3 py-2 text-left transition-colors',
        canSelect && 'outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background',
        canSelect && 'disabled:cursor-not-allowed disabled:opacity-60',
        active
          ? 'bg-primary/10'
          : canSelect
            ? 'hover:bg-accent hover:text-accent-foreground'
            : 'bg-muted/25',
      )"
      @click="handleClick"
    >
      <div class="relative shrink-0">
        <Avatar :class="cn('h-10 w-10', isOffline && 'opacity-60')">
          <AvatarImage v-if="avatarUrl" :src="avatarUrl" :alt="displayName" />
          <AvatarFallback>{{ initials }}</AvatarFallback>
        </Avatar>
        <span
          v-if="isOffline"
          class="absolute -bottom-0.5 -right-0.5 flex h-5 w-5 items-center justify-center rounded-full border-2 border-background bg-muted-foreground/80 text-background"
          :title="t('accounts.offline_hint')"
        >
          <WifiOff class="h-3 w-3" />
        </span>
      </div>

      <div class="min-w-0 flex-1">
        <div class="flex items-center gap-2">
          <span class="truncate text-sm font-medium">
            {{ displayName }}
          </span>
          <span
            v-if="active"
            class="inline-flex items-center gap-1 rounded-full bg-primary/15 px-2 py-0.5 text-[10px] font-semibold uppercase tracking-wide text-primary"
          >
            <Check class="h-3 w-3" />
            {{ t('accounts.active') }}
          </span>
        </div>
        <p
          :class="cn(
            'truncate text-xs',
            isOffline ? 'text-muted-foreground/70' : 'text-muted-foreground',
          )"
        >
          {{ tagline }}
        </p>
        <p v-if="portUnassigned" class="mt-0.5 text-xs text-muted-foreground/70">
          {{ t('accounts.cdp_port_unassigned') }}
        </p>
        <p v-else-if="isOffline" class="mt-0.5 text-xs text-muted-foreground/70">
          {{ t('accounts.offline_hint') }}
        </p>
      </div>

      <span v-if="statusLabel && !active" class="sr-only">{{ statusLabel }}</span>
    </component>

    <Button
      v-if="showReconnect && (isOffline || portUnassigned)"
      type="button"
      size="sm"
      variant="outline"
      class="shrink-0 gap-1.5"
      :disabled="busy"
      :aria-label="reconnectActionAriaLabel"
      @click="emit('reconnect', account.id)"
    >
      <RadioTower class="h-3.5 w-3.5" aria-hidden="true" />
      <span>{{ t(reconnectActionKey) }}</span>
    </Button>
  </div>
</template>
