// @vitest-environment happy-dom
import { describe, it, expect, vi, beforeEach } from 'vitest'
import { mount, flushPromises } from '@vue/test-utils'
import { defineComponent, h } from 'vue'
import { createI18n } from 'vue-i18n'
import { createPinia, setActivePinia } from 'pinia'
import AdvancedSettings from './AdvancedSettings.vue'
import { useQuestsStore } from '@/stores/quests'

// Partial mock: keep every real export (the quests store imports many) and only
// spy on the two super-properties entry points under test.
vi.mock('@/api/tauri', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/api/tauri')>()
  return {
    ...actual,
    getSuperPropertiesMode: vi.fn(),
    retrySuperProperties: vi.fn(),
    // The quests store performs these on creation (platform caps, run snapshot,
    // event listeners). Stub them so no real Tauri IPC runs under happy-dom.
    getQuestsFull: vi.fn(),
    getPlatformCapabilities: vi.fn(),
    listAllQuestRuns: vi.fn(),
    onQuestProgress: vi.fn(),
    onQuestComplete: vi.fn(),
    onQuestError: vi.fn(),
    onQuestStopped: vi.fn(),
  }
})
vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }))
vi.mock('@tauri-apps/api/path', () => ({
  documentDir: vi.fn().mockResolvedValue('C:/tmp'),
  join: vi.fn().mockResolvedValue('C:/tmp/DiscordQuestGames'),
}))
vi.mock('@tauri-apps/plugin-fs', () => ({ mkdir: vi.fn() }))

import {
  getSuperPropertiesMode,
  retrySuperProperties,
  getQuestsFull,
  getPlatformCapabilities,
  listAllQuestRuns,
  onQuestProgress,
  onQuestComplete,
  onQuestError,
  onQuestStopped,
} from '@/api/tauri'
import type { AutoFetchResult, PlatformCapabilities, SuperPropertiesModeInfo } from '@/api/tauri'

const mockedGetSuperPropertiesMode = vi.mocked(getSuperPropertiesMode)
const mockedRetrySuperProperties = vi.mocked(retrySuperProperties)

type Unlisten = Awaited<ReturnType<typeof onQuestProgress>>
const noopUnlisten = (() => {}) as unknown as Unlisten

function quietQuestsStoreStartup() {
  vi.mocked(getQuestsFull).mockResolvedValue({ quests: [], excluded_quests: [] } as never)
  vi.mocked(getPlatformCapabilities).mockResolvedValue({
    os: 'win32',
    executableOsPriority: ['win32'],
    launcherEntry: false,
  } as unknown as PlatformCapabilities)
  vi.mocked(listAllQuestRuns).mockResolvedValue([])
  vi.mocked(onQuestProgress).mockResolvedValue(noopUnlisten)
  vi.mocked(onQuestComplete).mockResolvedValue(noopUnlisten)
  vi.mocked(onQuestError).mockResolvedValue(noopUnlisten)
  vi.mocked(onQuestStopped).mockResolvedValue(noopUnlisten)
}

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

const BadgeStub = defineComponent({
  name: 'Badge',
  inheritAttrs: false,
  setup(_, { attrs, slots }) {
    return () => h('span', attrs, slots.default?.())
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

const LabelStub = PassthroughStub('Label')
const SettingRowStub = PassthroughStub('SettingRow')
const SettingsSectionCardStub = PassthroughStub('SettingsSectionCard')

const i18n = createI18n({
  legacy: false,
  locale: 'en',
  missingWarn: false,
  fallbackWarn: false,
  messages: {
    en: {
      settings: {
        advanced_title: 'Advanced',
        advanced_desc: 'Advanced settings',
        cdp_port: 'CDP port',
        cdp_port_hint: 'Port hint',
        edit_port: 'Edit',
        super_props_mode: 'Super Properties mode',
        super_props_mode_desc: 'Super properties mode source',
        default_mode: 'Default',
        developer_mode: 'Developer mode',
        developer_mode_desc: 'Developer mode description',
        debug_already_unlocked: 'Unlocked',
        developer_mode_locked: 'Locked',
        cache: 'Cache',
        cache_desc: 'Cache description',
        open_cache_dir: 'Open cache dir',
      },
      debug: { copy: 'Copy path' },
    },
  },
})

function mountSettings(activePort = 9224) {
  const pinia = createPinia()
  setActivePinia(pinia)
  const store = useQuestsStore()
  // Global default stays 9223; the active account's port is explicit.
  store.cdpPort = 9223
  store.setActiveAccount('acct-b', activePort)

  const wrapper = mount(AdvancedSettings, {
    global: {
      plugins: [i18n, pinia],
      stubs: {
        Button: ButtonStub,
        Badge: BadgeStub,
        Label: LabelStub,
        SettingRow: SettingRowStub,
        SettingsSectionCard: SettingsSectionCardStub,
      },
    },
  })
  return { wrapper, store }
}

describe('AdvancedSettings account CDP port scoping', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    localStorage.clear()
    quietQuestsStoreStartup()
    mockedGetSuperPropertiesMode.mockResolvedValue({ mode: 'cdp' } as unknown as SuperPropertiesModeInfo)
    mockedRetrySuperProperties.mockResolvedValue({} as unknown as AutoFetchResult)
  })

  it('retries SuperProperties on the active account port, not the global default', async () => {
    const { wrapper, store } = mountSettings()
    await flushPromises()

    const retry = wrapper.find('button[aria-label="Super properties mode source"]')
    expect(retry.exists()).toBe(true)

    await retry.trigger('click')
    await flushPromises()

    expect(mockedRetrySuperProperties).toHaveBeenCalledTimes(1)
    expect(mockedRetrySuperProperties).toHaveBeenCalledWith(9224)
    // Retrying the active account must never mutate the global default.
    expect(store.cdpPort).toBe(9223)
    expect(store.activeCdpPort).toBe(9224)
  })

  it('does not retry SuperProperties for a blocked port and retries after reassignment', async () => {
    const { wrapper, store } = mountSettings(0)
    await flushPromises()

    const retry = wrapper.find('button[aria-label="Super properties mode source"]')
    expect(retry.exists()).toBe(true)
    await retry.trigger('click')
    await flushPromises()
    expect(mockedRetrySuperProperties).not.toHaveBeenCalled()

    store.setActiveAccount('acct-b', 9224)
    await retry.trigger('click')
    await flushPromises()

    expect(mockedRetrySuperProperties).toHaveBeenCalledTimes(1)
    expect(mockedRetrySuperProperties).toHaveBeenCalledWith(9224)
    expect(store.cdpPort).toBe(9223)
  })

  it('shows the global default in the port badge (the badge configures the default)', async () => {
    const { wrapper } = mountSettings()
    await flushPromises()

    expect(wrapper.text()).toContain('9223')
    expect(wrapper.text()).not.toContain('9224')
  })
})
