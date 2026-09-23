<script setup lang="ts">
import { computed } from 'vue'
import { useI18n } from 'vue-i18n'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { AlertTriangle } from 'lucide-vue-next'
import type { AccountSummary } from './AccountListItem.vue'

const props = withDefaults(
  defineProps<{
    open?: boolean
    account?: AccountSummary | null
    busy?: boolean
  }>(),
  {
    open: false,
    account: null,
    busy: false,
  },
)

const emit = defineEmits<{
  'update:open': [value: boolean]
  confirm: [id: string]
  cancel: []
}>()

const { t } = useI18n()

const isOpen = computed({
  get: () => props.open,
  set: (value) => emit('update:open', value),
})

const accountDisplayName = computed(() => {
  if (!props.account) return ''
  return props.account.globalName || props.account.username
})

function handleConfirm() {
  if (props.account && !props.busy) {
    emit('confirm', props.account.id)
  }
}

function handleCancel() {
  if (!props.busy) {
    emit('cancel')
    isOpen.value = false
  }
}
</script>

<template>
  <Dialog v-model:open="isOpen">
    <DialogContent class="sm:max-w-md">
      <DialogHeader>
        <div class="flex items-center gap-3">
          <div class="flex h-10 w-10 shrink-0 items-center justify-center rounded-full bg-destructive/10 text-destructive">
            <AlertTriangle class="h-5 w-5" />
          </div>
          <div>
            <DialogTitle>{{ t('accounts.remove_title') }}</DialogTitle>
            <DialogDescription class="mt-1">
              {{ t('accounts.remove_description', { username: accountDisplayName }) }}
            </DialogDescription>
          </div>
        </div>
      </DialogHeader>

      <DialogFooter class="gap-2 sm:gap-0">
        <Button
          variant="outline"
          :disabled="busy"
          @click="handleCancel"
        >
          {{ t('accounts.remove_cancel') }}
        </Button>
        <Button
          variant="destructive"
          :disabled="busy"
          @click="handleConfirm"
        >
          {{ busy ? t('accounts.remove_busy') : t('accounts.remove_confirm') }}
        </Button>
      </DialogFooter>
    </DialogContent>
  </Dialog>
</template>
