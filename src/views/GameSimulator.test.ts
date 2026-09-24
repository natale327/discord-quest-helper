// @vitest-environment happy-dom
import { describe, it, expect, vi, beforeEach } from 'vitest'
import { mount, flushPromises } from '@vue/test-utils'
import { defineComponent, h, nextTick } from 'vue'
import { createI18n } from 'vue-i18n'
import { createPinia, setActivePinia } from 'pinia'
import GameSimulator from './GameSimulator.vue'
import { useQuestsStore } from '@/stores/quests'
import type { CdpStatus, DetectableGame, ManualCdpGameSimulation, PlatformCapabilities } from '@/api/tauri'

vi.mock('@/api/tauri', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/api/tauri')>()
  return {
    ...actual,
    createSimulatedGame: vi.fn(),
    runSimulatedGame: vi.fn(),
    stopSimulatedGame: vi.fn(),
    connectToDiscordRpc: vi.fn(),
    disconnectFromDiscordRpc: vi.fn(),
    startManualCdpGameSimulation: vi.fn(),
    stopManualCdpGameSimulation: vi.fn(),
    getManualCdpGameSimulation: vi.fn(),
    checkCdpStatus: vi.fn(),
    getPlatformCapabilities: vi.fn(),
    // The quests store performs these on creation (run snapshot + event
    // listeners). Stub them so no real Tauri IPC runs under happy-dom.
    getQuestsFull: vi.fn(),
    listAllQuestRuns: vi.fn(),
    onQuestProgress: vi.fn(),
    onQuestComplete: vi.fn(),
    onQuestError: vi.fn(),
    onQuestStopped: vi.fn(),
  }
})
vi.mock('@tauri-apps/api/path', () => ({
  documentDir: vi.fn().mockResolvedValue('C:/tmp'),
  sep: vi.fn().mockResolvedValue('/'),
}))
vi.mock('@tauri-apps/plugin-dialog', () => ({ open: vi.fn() }))

import {
  startManualCdpGameSimulation,
  getManualCdpGameSimulation,
  checkCdpStatus,
  getPlatformCapabilities,
  getQuestsFull,
  listAllQuestRuns,
  onQuestProgress,
  onQuestComplete,
  onQuestError,
  onQuestStopped,
} from '@/api/tauri'

const mockedStartManualCdpGameSimulation = vi.mocked(startManualCdpGameSimulation)
const mockedGetManualCdpGameSimulation = vi.mocked(getManualCdpGameSimulation)
const mockedCheckCdpStatus = vi.mocked(checkCdpStatus)
const mockedGetPlatformCapabilities = vi.mocked(getPlatformCapabilities)

type Unlisten = Awaited<ReturnType<typeof onQuestProgress>>
const noopUnlisten = (() => {}) as unknown as Unlisten

function quietQuestsStoreStartup() {
  vi.mocked(getQuestsFull).mockResolvedValue({ quests: [], excluded_quests: [] } as never)
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

const GameSelectorStub = defineComponent({
  name: 'GameSelector',
  emits: ['select'],
  setup() {
    return () => h('div', { 'data-test': 'game-selector' })
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
  Card: PassthroughStub('Card'),
  CardHeader: PassthroughStub('CardHeader'),
  CardTitle: PassthroughStub('CardTitle'),
  CardContent: PassthroughStub('CardContent'),
  CardDescription: PassthroughStub('CardDescription'),
  CardFooter: PassthroughStub('CardFooter'),
  Dialog: PassthroughStub('Dialog'),
  DialogContent: PassthroughStub('DialogContent'),
  DialogHeader: PassthroughStub('DialogHeader'),
  DialogTitle: PassthroughStub('DialogTitle'),
  DialogDescription: PassthroughStub('DialogDescription'),
  DialogFooter: PassthroughStub('DialogFooter'),
  GameSelector: GameSelectorStub,
}

const i18n = createI18n({
  legacy: false,
  locale: 'en',
  missingWarn: false,
  fallbackWarn: false,
  messages: {
    en: {
      game_sim: {
        title: 'Game Simulator',
        mode_from_list: 'From list',
        mode_custom: 'Custom',
        config_title: 'Config',
        config_desc: 'Config description',
        custom_config_desc: 'Custom config description',
        select_game: 'Select a game',
        platform_capabilities_unavailable: 'Unavailable',
        no_exe_hint: 'No executable',
        no_linux_exe_hint: 'No Linux executable',
        no_exe_custom_warning: 'Custom warning',
        custom_exe_label: 'Executable',
        custom_exe_placeholder: 'exe',
        custom_exe_hint: 'Custom exe hint',
        install_path: 'Install path',
        select_exe: 'Select executable',
        run_game: 'Run Game',
        run_cdp_game: 'Run with CDP',
        cdp_button_hint: 'CDP hint',
        cdp_unavailable: 'CDP unavailable',
        cdp_starting: 'Starting CDP',
        cdp_started: 'CDP started',
        cdp_stopped: 'CDP stopped',
        cdp_active: 'CDP active',
        cdp_session_restored: 'Restored',
        cdp_cleanup_failed: 'Cleanup failed',
        create_game: 'Create Game',
        creating: 'Creating',
        starting: 'Starting',
        stopping: 'Stopping',
        stop_game: 'Stop',
        create_dialog_title: 'Create',
        create_dialog_desc: 'Create description',
        create_dialog_path_label: 'Path',
        create_dialog_path_hint: 'Path hint',
        create_success: 'Created',
        run_success: 'Ran',
        run_success_rpc: 'Ran with RPC',
        stopped: 'Stopped',
        no_active_process: 'No process',
      },
      general: { loading: 'Loading' },
      dialog: { cancel: 'Cancel', confirm: 'Confirm' },
    },
  },
})

const testGame: DetectableGame = {
  id: '1234567890',
  name: 'Test Game',
  executables: [],
}

function mountSimulator(activePort = 9224) {
  const pinia = createPinia()
  setActivePinia(pinia)
  const store = useQuestsStore()
  // Global default stays 9223; the active account's port is explicit.
  store.cdpPort = 9223
  store.setActiveAccount('acct-b', activePort)

  const wrapper = mount(GameSimulator, {
    global: { plugins: [i18n, pinia], stubs },
  })
  return { wrapper, store }
}

describe('GameSimulator account CDP port scoping', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    localStorage.clear()
    quietQuestsStoreStartup()
    mockedGetManualCdpGameSimulation.mockResolvedValue(null)
    mockedCheckCdpStatus.mockResolvedValue({ connected: true } as unknown as CdpStatus)
    mockedGetPlatformCapabilities.mockResolvedValue({
      os: 'win32',
      executableOsPriority: ['win32'],
      launcherEntry: false,
    } as unknown as PlatformCapabilities)
    mockedStartManualCdpGameSimulation.mockResolvedValue({ appName: 'Test Game' } as unknown as ManualCdpGameSimulation)
  })

  it('starts the manual CDP game simulation on the active account port, not the global default', async () => {
    const { wrapper, store } = mountSimulator()
    await flushPromises()

    wrapper.findComponent({ name: 'GameSelector' }).vm.$emit('select', testGame)
    await nextTick()
    await flushPromises()

    const cdpButton = wrapper.findAll('button').find(button => button.text().includes('Run with CDP'))
    expect(cdpButton, 'CDP run button').toBeTruthy()

    await cdpButton!.trigger('click')
    await flushPromises()

    expect(mockedStartManualCdpGameSimulation).toHaveBeenCalledTimes(1)
    expect(mockedStartManualCdpGameSimulation).toHaveBeenCalledWith(testGame.id, testGame.name, 9224)
    // Starting for the active account must never mutate the global default.
    expect(store.cdpPort).toBe(9223)
    expect(store.activeCdpPort).toBe(9224)
  })

  it('blocks manual spoof IPC at port 0 and uses a valid account port after reassignment', async () => {
    const { wrapper, store } = mountSimulator(0)
    await flushPromises()

    // Exercise the mutation handler even if an earlier/stale UI status says the
    // connection is available; the operation guard must still reject port 0.
    store.cdpAvailable = true
    wrapper.findComponent({ name: 'GameSelector' }).vm.$emit('select', testGame)
    await flushPromises()
    const cdpButton = wrapper.findAll('button').find(button => button.text().includes('Run with CDP'))
    expect(cdpButton, 'CDP run button').toBeTruthy()
    await cdpButton!.trigger('click')
    await flushPromises()

    expect(mockedStartManualCdpGameSimulation).not.toHaveBeenCalled()
    expect(mockedCheckCdpStatus).not.toHaveBeenCalled()

    store.setActiveAccount('acct-b', 9224)
    await cdpButton!.trigger('click')
    await flushPromises()

    expect(mockedCheckCdpStatus).toHaveBeenCalledWith(9224)
    expect(mockedStartManualCdpGameSimulation).toHaveBeenCalledWith(testGame.id, testGame.name, 9224)
    expect(store.cdpPort).toBe(9223)
  })
})
