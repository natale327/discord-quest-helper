<script setup lang="ts">
import { CheckCircle2, Loader2, LogOut, ShieldCheck, UserRound } from 'lucide-vue-next'
import { useI18n } from 'vue-i18n'
import { Button } from '@/components/ui/button'
import { useAuthStore } from '@/stores/auth'
import SettingsSectionCard from './SettingsSectionCard.vue'
import SettingsStatusPanel from './SettingsStatusPanel.vue'
import { settingToneClass } from './settingTones'

const { t } = useI18n()
const authStore = useAuthStore()
const emit = defineEmits<{
  navigateToHome: []
}>()

async function handleCdpLogin() {
  await authStore.loginViaCdp()
  if (authStore.user) emit('navigateToHome')
}
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
</template>
