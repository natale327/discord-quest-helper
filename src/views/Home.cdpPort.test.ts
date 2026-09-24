// @vitest-environment happy-dom
import { describe, it, expect, vi, beforeEach } from 'vitest'
import { mount, flushPromises } from '@vue/test-utils'
import { defineComponent, h, nextTick } from 'vue'
import { createI18n } from 'vue-i18n'
import { createPinia, setActivePinia } from 'pinia'
import Home from './Home.vue'
import { useQuestsStore } from '@/stores/quests'
import type { Quest } from '@/api/tauri'

// Partial mock: keep every real export so the whole import graph resolves, and
// neutralise the functions the quests store calls on creation / onMounted so no
// real IPC happens in the DOM environment.
vi.mock('@/api/tauri', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/api/tauri')>()
  return {
    ...actual,
    getQuestsFull: vi.fn(),
    listAllQuestRuns: vi.fn(),
    onQuestProgress: vi.fn(),
    onQuestComplete: vi.fn(),
    onQuestError: vi.fn(),
    onQuestStopped: vi.fn(),
    getVirtualCurrencyBalance: vi.fn(),
    getPlatformCapabilities: vi.fn(),
    navigateDiscordSpa: vi.fn(),
    acceptQuest: vi.fn(),
    claimQuestReward: vi.fn(),
  }
})
vi.mock('@tauri-apps/api/path', () => ({
  documentDir: vi.fn().mockResolvedValue('C:/tmp'),
  join: vi.fn().mockResolvedValue('C:/tmp/DiscordQuestGames'),
  sep: vi.fn().mockResolvedValue('/'),
}))
vi.mock('@tauri-apps/api/event', () => ({ emit: vi.fn() }))
vi.mock('@tauri-apps/plugin-opener', () => ({ openUrl: vi.fn() }))

// These two children embed public asset URLs (`/icons/nitro.svg`, `/icons/orbs.png`)
// that Vite's test asset transform cannot resolve under happy-dom. They are
// stubbed at render time anyway, so replace the modules outright.
vi.mock('@/components/OrbsNitroStatus.vue', async () => {
  const { defineComponent } = await import('vue')
  return { default: defineComponent({ name: 'OrbsNitroStatus', render: () => null }) }
})
vi.mock('@/components/QuestCard.vue', async () => {
  const { defineComponent, h } = await import('vue')
  return {
    default: defineComponent({
      name: 'QuestCard',
      setup(_, { slots }) {
        return () => h('div', { 'data-test': 'quest-card' }, [slots.default?.(), slots.actions?.()])
      },
    }),
  }
})

// The auth store's setup fires `loadAccounts()` (real IPC). Home only needs a
// signed-in projection, so provide a lightweight fake for this mount.
const { authMock } = vi.hoisted(() => ({
  authMock: {
    user: { id: 'u1', username: 'tester', discriminator: '0', avatar: null, global_name: null },
    fetchNitroProgramReward: vi.fn().mockResolvedValue(undefined),
  },
}))
vi.mock('@/stores/auth', () => ({ useAuthStore: () => authMock }))

import {
  getQuestsFull,
  listAllQuestRuns,
  onQuestProgress,
  onQuestComplete,
  onQuestError,
  onQuestStopped,
  getVirtualCurrencyBalance,
  getPlatformCapabilities,
  navigateDiscordSpa,
} from '@/api/tauri'

const mockedGetQuestsFull = vi.mocked(getQuestsFull)
const mockedListAllQuestRuns = vi.mocked(listAllQuestRuns)
const mockedGetVirtualCurrencyBalance = vi.mocked(getVirtualCurrencyBalance)
const mockedGetPlatformCapabilities = vi.mocked(getPlatformCapabilities)
const mockedNavigateDiscordSpa = vi.mocked(navigateDiscordSpa)

const ButtonStub = defineComponent({
  name: 'Button',
  inheritAttrs: false,
  props: { disabled: Boolean },
  emits: ['click'],
  setup(props, { attrs, slots, emit }) {
    return () =>
      h('button', { ...attrs, disabled: props.disabled, onClick: (e: MouseEvent) => emit('click', e) }, slots.default?.())
  },
})

const InputStub = defineComponent({
  name: 'Input',
  inheritAttrs: false,
  props: { modelValue: { type: [String, Number], default: '' } },
  emits: ['update:modelValue'],
  setup(props, { attrs, emit }) {
    return () =>
      h('input', {
        ...attrs,
        value: props.modelValue,
        onInput: (e: Event) => emit('update:modelValue', (e.target as HTMLInputElement).value),
      })
  },
})

const QuestCardStub = defineComponent({
  name: 'QuestCard',
  setup(_, { slots }) {
    return () => h('div', { 'data-test': 'quest-card' }, [slots.default?.(), slots.actions?.()])
  },
})

const PassthroughStub = (name: string) =>
  defineComponent({
    name,
    inheritAttrs: false,
    setup(_, { attrs, slots }) {
      return () => h('div', attrs, slots.default?.())
    },
  })

const stubs = {
  Button: ButtonStub,
  Input: InputStub,
  Card: PassthroughStub('Card'),
  CardHeader: PassthroughStub('CardHeader'),
  CardTitle: PassthroughStub('CardTitle'),
  CardContent: PassthroughStub('CardContent'),
  AlertDialog: PassthroughStub('AlertDialog'),
  AlertDialogContent: PassthroughStub('AlertDialogContent'),
  AlertDialogHeader: PassthroughStub('AlertDialogHeader'),
  AlertDialogTitle: PassthroughStub('AlertDialogTitle'),
  AlertDialogDescription: PassthroughStub('AlertDialogDescription'),
  AlertDialogFooter: PassthroughStub('AlertDialogFooter'),
  AlertDialogAction: PassthroughStub('AlertDialogAction'),
  AlertDialogCancel: PassthroughStub('AlertDialogCancel'),
  Dialog: PassthroughStub('Dialog'),
  DialogContent: PassthroughStub('DialogContent'),
  DialogHeader: PassthroughStub('DialogHeader'),
  DialogTitle: PassthroughStub('DialogTitle'),
  DialogDescription: PassthroughStub('DialogDescription'),
  DialogFooter: PassthroughStub('DialogFooter'),
  OrbsNitroStatus: PassthroughStub('OrbsNitroStatus'),
  QuestListHeader: PassthroughStub('QuestListHeader'),
  QuestViewTabs: PassthroughStub('QuestViewTabs'),
  QuestCard: QuestCardStub,
  QuestProgress: PassthroughStub('QuestProgress'),
}

const i18n = createI18n({
  legacy: false,
  locale: 'en',
  missingWarn: false,
  fallbackWarn: false,
  messages: {
    en: {
      home: {
        dashboard_title: 'Dashboard',
        dashboard_desc: 'Dashboard description',
        launch_activity: 'Launch Activity',
        activity_launch_title: 'Launch activity quest',
        activity_launch_desc: 'Launch description',
        activity_launch_step1: 'Step 1',
        activity_launch_step2: 'Step 2',
        activity_launch_step3: 'Step 3',
        activity_launch_step4: 'Step 4',
        activity_launch_navigate: 'Open in Discord',
        activity_launch_cancel: 'Cancel',
        activity_launch_start: 'Start',
        accept_quest: 'Accept Quest',
        accept_all: 'Accept all',
        start_quest: 'Start Quest',
        start_watching: 'Start Watching',
        start_playing: 'Start Playing',
        start_streaming: 'Start Streaming',
        claim_reward: 'Claim Reward',
        completed: 'Completed',
        stop: 'Stop',
        reset_filters: 'Reset filters',
        back_to_recommended: 'Back to recommended',
        advanced_filters: 'Advanced filters',
        reset_advanced_filters: 'Reset',
        include_expired: 'Include expired',
        include_expired_desc: 'Description',
      },
      general: {
        login_prompt: 'Log in to continue',
        loading: 'Loading...',
        refresh: 'Refresh',
      },
      dialog: { cancel: 'Cancel', accept: 'Accept', start: 'Start', confirm: 'Confirm' },
      version: { update_available: 'Update', update_desc: 'Desc', download: 'Download' },
      filter: { type: 'Type', reward: 'Reward', video: 'Video', play: 'Play', activity: 'Activity', orbs: 'Orbs' },
    },
  },
})

function activityQuest(id: string): Quest {
  return {
    id,
    config: {
      messages: { quest_name: `Activity ${id}`, game_title: 'Test Game' },
      expires_at: '2099-01-01T00:00:00.000Z',
      task_config_v2: {
        tasks: { ACTIVITY: { type: 'ACHIEVEMENT_IN_ACTIVITY', target: 3 } },
      },
    },
    user_status: { enrolled_at: '2026-01-01T00:00:00.000Z' },
  } as unknown as Quest
}

async function mountHome(activePort = 9224) {
  const pinia = createPinia()
  setActivePinia(pinia)
  const store = useQuestsStore()
  // Global default stays 9223; the active account's port is explicit.
  store.cdpPort = 9223
  store.setActiveAccount('acct-b', activePort)
  store.cdpAvailable = true

  const wrapper = mount(Home, {
    global: { plugins: [i18n, pinia], stubs },
  })
  return { wrapper, store }
}

describe('Home account CDP port scoping', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    localStorage.clear()
    mockedGetQuestsFull.mockResolvedValue({
      quests: [],
      excluded_quests: [],
      quest_enrollment_blocked_until: null,
    } as never)
    mockedListAllQuestRuns.mockResolvedValue([] as never)
    mockedGetVirtualCurrencyBalance.mockResolvedValue(0 as never)
    mockedGetPlatformCapabilities.mockResolvedValue({
      os: 'win32',
      executableOsPriority: ['win32'],
      launcherEntry: false,
    } as never)
    for (const listener of [onQuestProgress, onQuestComplete, onQuestError, onQuestStopped]) {
      vi.mocked(listener).mockResolvedValue((() => {}) as never)
    }
    mockedNavigateDiscordSpa.mockResolvedValue(undefined as never)
  })

  it('navigates Discord to an activity quest on the active account port, not the global default', async () => {
    const { wrapper, store } = await mountHome()
    await flushPromises()

    // The onMounted fetch resolves to zero quests; inject the activity quest so
    // the next projection render surfaces the real Start -> Launch call sites.
    const quest = activityQuest('activity-1')
    store.quests = [quest]
    await nextTick()

    const startButton = wrapper.findAll('button').find(button => button.text().includes('Launch Activity'))
    expect(startButton, 'launch activity button').toBeTruthy()
    await startButton!.trigger('click')
    await nextTick()

    const navigateButton = wrapper.findAll('button').find(button => button.text().includes('Open in Discord'))
    expect(navigateButton, 'navigate-in-Discord button').toBeTruthy()
    await navigateButton!.trigger('click')
    await flushPromises()

    expect(mockedNavigateDiscordSpa).toHaveBeenCalledTimes(1)
    expect(mockedNavigateDiscordSpa).toHaveBeenCalledWith(`/quest-home#${encodeURIComponent(quest.id)}`, 9224)
    // Navigating for the active account must never mutate the global default.
    expect(store.cdpPort).toBe(9223)
    expect(store.activeCdpPort).toBe(9224)
  })

  it('blocks activity navigation at port 0 before IPC and resumes on a valid account port', async () => {
    const { wrapper, store } = await mountHome(0)
    await flushPromises()

    const quest = activityQuest('activity-blocked')
    store.quests = [quest]
    await nextTick()
    const startButton = wrapper.findAll('button').find(button => button.text().includes('Launch Activity'))
    expect(startButton, 'launch activity button').toBeTruthy()
    await startButton!.trigger('click')
    await nextTick()

    const navigateButton = wrapper.findAll('button').find(button => button.text().includes('Open in Discord'))
    expect(navigateButton, 'navigate-in-Discord button').toBeTruthy()
    await navigateButton!.trigger('click')
    await flushPromises()

    expect(mockedNavigateDiscordSpa).not.toHaveBeenCalled()

    store.setActiveAccount('acct-b', 9224)
    await navigateButton!.trigger('click')
    await flushPromises()

    expect(mockedNavigateDiscordSpa).toHaveBeenCalledTimes(1)
    expect(mockedNavigateDiscordSpa).toHaveBeenCalledWith(
      `/quest-home#${encodeURIComponent(quest.id)}`,
      9224,
    )
    expect(store.cdpPort).toBe(9223)
  })
})
