// @vitest-environment happy-dom
import { describe, it, expect, vi, beforeEach } from 'vitest'
import { mount, flushPromises } from '@vue/test-utils'
import { defineComponent, h } from 'vue'
import { createI18n } from 'vue-i18n'
import { createPinia, setActivePinia } from 'pinia'
import DiscordIntegrationSettings from './DiscordIntegrationSettings.vue'
import { useQuestsStore } from '@/stores/quests'

// Partial mock: keep every real api export so the quests store still imports,
// and spy on the desktop-client / super-properties entries under test.
vi.mock('@/api/tauri', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/api/tauri')>()
  return {
    ...actual,
    fetchSuperPropertiesCdp: vi.fn(),
    getDebugInfo: vi.fn(),
    listRunningDesktopCdpSessions: vi.fn(),
    launchDesktopClientCdp: vi.fn(),
    createDiscordCdpLauncherShortcut: vi.fn(),
    getQuestsFull: vi.fn(),
    getDesktopClientState: vi.fn(),
    setDesktopClientSelection: vi.fn(),
    addDesktopClientInstallation: vi.fn(),
    removeDesktopClientInstallation: vi.fn(),
    // The quests store performs these on creation (platform caps, run snapshot,
    // event listeners). Stub them so no real Tauri IPC runs under happy-dom.
    getPlatformCapabilities: vi.fn(),
    listAllQuestRuns: vi.fn(),
    onQuestProgress: vi.fn(),
    onQuestComplete: vi.fn(),
    onQuestError: vi.fn(),
    onQuestStopped: vi.fn(),
  }
})

// Mock the desktop-client composable so the test can drive the render state and
// observe which port the desktop-scan call sites use.
const { clientsMock, clientsState, refreshMock } = vi.hoisted(() => {
  const state = { value: null as unknown }
  const refresh = vi.fn()
  return {
    clientsState: state,
    refreshMock: refresh,
    clientsMock: {
      state,
      loading: { value: false },
      error: { value: null },
      selectedInstallation: { value: null },
      selectedProviderId: { value: null },
      selectedIsRunning: { value: false },
      refresh,
      select: vi.fn(),
      addInstallation: vi.fn(),
      removeInstallation: vi.fn(),
      migrateLegacySelection: vi.fn(),
      installationPath: vi.fn(() => undefined),
    },
  }
})

vi.mock('@/composables/desktopClientState', () => ({
  useDesktopClientState: () => clientsMock,
  desktopClientArgForProvider: (providerId: string) =>
    providerId === 'vencord.vesktop' ? 'vesktop' : 'official',
}))

import {
  createDiscordCdpLauncherShortcut,
  fetchSuperPropertiesCdp,
  getQuestsFull,
  getDebugInfo,
  launchDesktopClientCdp,
} from '@/api/tauri'
import type { CdpSuperProperties, DebugInfo } from '@/api/tauri'
import {
  getPlatformCapabilities,
  listAllQuestRuns,
  onQuestProgress,
  onQuestComplete,
  onQuestError,
  onQuestStopped,
} from '@/api/tauri'
import type { PlatformCapabilities } from '@/api/tauri'

const mockedFetchSuperPropertiesCdp = vi.mocked(fetchSuperPropertiesCdp)
const mockedGetDebugInfo = vi.mocked(getDebugInfo)
const mockedLaunchDesktopClientCdp = vi.mocked(launchDesktopClientCdp)
const mockedCreateDiscordCdpLauncherShortcut = vi.mocked(createDiscordCdpLauncherShortcut)

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
  Label: PassthroughStub('Label'),
  Badge: PassthroughStub('Badge'),
  AlertDialog: PassthroughStub('AlertDialog'),
  AlertDialogContent: PassthroughStub('AlertDialogContent'),
  AlertDialogHeader: PassthroughStub('AlertDialogHeader'),
  AlertDialogTitle: PassthroughStub('AlertDialogTitle'),
  AlertDialogDescription: PassthroughStub('AlertDialogDescription'),
  AlertDialogFooter: PassthroughStub('AlertDialogFooter'),
  AlertDialogAction: PassthroughStub('AlertDialogAction'),
  AlertDialogCancel: PassthroughStub('AlertDialogCancel'),
  AdvancedDisclosure: PassthroughStub('AdvancedDisclosure'),
  SettingsSectionCard: PassthroughStub('SettingsSectionCard'),
  SettingsStatusPanel: PassthroughStub('SettingsStatusPanel'),
  DesktopClientPicker: PassthroughStub('DesktopClientPicker'),
}

const i18n = createI18n({
  legacy: false,
  locale: 'en',
  missingWarn: false,
  fallbackWarn: false,
  messages: {
    en: {
      settings: {
        cdp_title: 'Discord CDP',
        cdp_desc: 'Discord CDP description',
        cdp_checking: 'Checking...',
        cdp_connected: 'Connected',
        cdp_disconnected: 'Disconnected',
        integration_setup: 'Integration setup',
        cdp_launch: 'Launch',
        cdp_restart: 'Restart',
        cdp_launch_desc: 'Launch description',
        cdp_shortcut_title: 'Shortcut',
        cdp_shortcut_desc: 'Shortcut description',
        cdp_create_shortcut: 'Create shortcut',
        client_emulation: 'Client emulation',
        client_emulation_desc: 'Client emulation description',
        cdp_sync: 'Sync Super Properties',
        cdp_sync_success: 'Synced',
        super_props_mode: 'Super Properties mode',
        custom_port: 'Custom port',
        custom_port_desc: 'Custom port description',
        cdp_port: 'CDP port',
        cdp_port_hint: 'Port hint',
      },
      desktop_clients: {
        owner_conflict_title: 'Owner conflict',
        owner_conflict_desc: 'Owner conflict description',
        use_current: 'Use current',
        switch_selected: 'Switch selected',
      },
      general: { refresh: 'Refresh' },
      dialog: { cancel: 'Cancel' },
    },
  },
})

function connectedSnapshot(port: number) {
  return {
    port,
    selection: { kind: 'auto' },
    installations: [],
    processes: [],
    endpoint: { status: 'discordReady', targetTitle: null, ownerProviderId: null },
  }
}

function mountSettings(activePort = 9224) {
  const pinia = createPinia()
  setActivePinia(pinia)
  const store = useQuestsStore()
  // Global default stays 9223; the active account's port is explicit.
  store.cdpPort = 9223
  store.setActiveAccount('acct-b', activePort)

  const wrapper = mount(DiscordIntegrationSettings, {
    global: { plugins: [i18n, pinia], stubs },
  })
  return { wrapper, store }
}

describe('DiscordIntegrationSettings account CDP port scoping', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    localStorage.clear()
    quietQuestsStoreStartup()
    clientsState.value = connectedSnapshot(9223)
    refreshMock.mockResolvedValue(connectedSnapshot(9223))
    mockedFetchSuperPropertiesCdp.mockResolvedValue({} as unknown as CdpSuperProperties)
    mockedGetDebugInfo.mockResolvedValue(null as unknown as DebugInfo)
    mockedCreateDiscordCdpLauncherShortcut.mockResolvedValue(undefined as never)
  })

  it('fetches Super Properties on the active account port while desktop scans keep the global default', async () => {
    const { wrapper, store } = mountSettings()
    await flushPromises()

    const sync = wrapper.findAll('button').find(button => button.text().includes('Sync Super Properties'))
    expect(sync, 'sync button').toBeTruthy()

    await sync!.trigger('click')
    await flushPromises()

    // Account-targeted fetch uses the active account port...
    expect(mockedFetchSuperPropertiesCdp).toHaveBeenCalledTimes(1)
    expect(mockedFetchSuperPropertiesCdp).toHaveBeenCalledWith(9224)
    // ...while the read-only desktop scan (checkCdp) keeps the global default.
    expect(refreshMock).toHaveBeenCalledWith(9223)
    expect(refreshMock).not.toHaveBeenCalledWith(9224)
    expect(store.cdpPort).toBe(9223)
    expect(store.activeCdpPort).toBe(9224)
  })

  it('blocks Super Properties IPC for port 0, then uses the reassigned port while desktop scan stays global', async () => {
    const { wrapper, store } = mountSettings(0)
    await flushPromises()

    const sync = wrapper.findAll('button').find(button => button.text().includes('Sync Super Properties'))
    expect(sync, 'sync button').toBeTruthy()
    await sync!.trigger('click')
    await flushPromises()
    expect(mockedFetchSuperPropertiesCdp).not.toHaveBeenCalled()
    expect(refreshMock).toHaveBeenCalledWith(9223)
    expect(refreshMock).not.toHaveBeenCalledWith(0)

    store.setActiveAccount('acct-b', 9224)
    await sync!.trigger('click')
    await flushPromises()

    expect(mockedFetchSuperPropertiesCdp).toHaveBeenCalledTimes(1)
    expect(mockedFetchSuperPropertiesCdp).toHaveBeenCalledWith(9224)
    expect(refreshMock).not.toHaveBeenCalledWith(9224)
    expect(store.cdpPort).toBe(9223)
  })

  it('keeps launcher shortcut creation on the global desktop-management port', async () => {
    const { wrapper, store } = mountSettings(0)
    await flushPromises()
    store.platformCapabilities = {
      os: 'win32',
      executableOsPriority: ['win32'],
      launcherEntry: true,
    } as unknown as PlatformCapabilities
    await flushPromises()

    const shortcut = wrapper.findAll('button').find(button => button.text().includes('Create shortcut'))
    expect(shortcut, 'shortcut button').toBeTruthy()
    await shortcut!.trigger('click')
    await flushPromises()

    expect(mockedCreateDiscordCdpLauncherShortcut).toHaveBeenCalledWith(9223, 'auto', 'auto', undefined)
    expect(store.activeCdpPort).toBe(0)
  })

  it('renders a structured CDP launch error message instead of [object Object]', async () => {
    const disconnectedSnapshot = {
      ...connectedSnapshot(9223),
      endpoint: { status: 'unreachable', targetTitle: null, ownerProviderId: null },
    }
    clientsState.value = disconnectedSnapshot
    refreshMock.mockResolvedValue(disconnectedSnapshot)
    mockedLaunchDesktopClientCdp.mockRejectedValue({
      code: 'port_occupied',
      params: { port: 9223, secret: 'do not show this' },
      message: 'Port 9223 is already in use.',
    })

    const { wrapper } = mountSettings()
    await flushPromises()

    const launch = wrapper.findAll('button').find(button => button.text().includes('Launch'))
    expect(launch, 'launch button').toBeTruthy()
    await launch!.trigger('click')
    await flushPromises()

    expect(wrapper.text()).toContain('Port 9223 is already in use.')
    expect(wrapper.text()).not.toContain('[object Object]')
    expect(wrapper.text()).not.toContain('do not show this')

    wrapper.unmount()
  })
})
