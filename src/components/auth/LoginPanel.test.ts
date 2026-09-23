// @vitest-environment happy-dom
import { describe, it, expect, vi, beforeEach } from 'vitest'
import { mount, flushPromises, type VueWrapper } from '@vue/test-utils'
import { defineComponent, h, nextTick } from 'vue'
import { createI18n } from 'vue-i18n'
// The panel's static `<img src="/icons/logo.png">` is transformed into an asset
// import by the Vue SFC compiler; under happy-dom there is no dev server to
// serve the public file, so the asset module is mocked.
vi.mock('/icons/logo.png', () => ({ default: '/icons/logo.png' }))

import LoginPanel from './LoginPanel.vue'
import type {
  AuthProgress,
  CdpStatus,
  ClientInstallation,
  ClientSelection,
  DesktopClientState,
} from '@/api/tauri'

// Keep every real API export so the import graph resolves, and neutralise the
// IPC the panel drives. The desktop-client composable is mocked below and
// delegates its scan to `getDesktopClientState`, so the exact port is observable.
vi.mock('@/api/tauri', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/api/tauri')>()
  return {
    ...actual,
    checkCdpStatus: vi.fn(),
    getDesktopClientState: vi.fn(),
    setDesktopClientSelection: vi.fn(),
    launchDesktopClientCdp: vi.fn(),
    listRunningDesktopCdpSessions: vi.fn(),
  }
})

// Lightweight store mocks. The factories wrap the shared hoisted objects so the
// component's writes (e.g. a duplicate result) are observed on the next render.
const { authMock, questsMock } = vi.hoisted(() => ({
  authMock: {
    user: null as unknown,
    loading: false,
    error: null as string | null,
    accounts: [] as unknown[],
    activeAccountId: null as string | null,
    accountPorts: {} as Record<string, number>,
    portForAccount: vi.fn(),
    duplicateLoginAccountId: null as string | null,
    loginViaCdp: vi.fn(),
    addAccountViaCdp: vi.fn(),
  },
  questsMock: {
    cdpPort: 9223,
    cdpAvailable: false,
    desktopClient: 'auto' as string,
    activeCdpPort: 9223,
    setActiveAccount: vi.fn(),
    resetForLogout: vi.fn(),
  },
}))

vi.mock('@/stores/auth', async () => {
  const { reactive } = await import('vue')
  return { useAuthStore: () => reactive(authMock) }
})
vi.mock('@/stores/quests', async () => {
  const { reactive } = await import('vue')
  return { useQuestsStore: () => reactive(questsMock) }
})

// Mock the desktop-client composable so the panel's scan call sites are driven
// by the mocked Tauri API and their ports are observable.
const { clientsMock, clientsState, refreshMock, selectMock } = vi.hoisted(() => {
  const state = { value: null as unknown }
  const refresh = vi.fn()
  const select = vi.fn()
  return {
    clientsState: state,
    refreshMock: refresh,
    selectMock: select,
    clientsMock: {
      state,
      loading: { value: false },
      error: { value: null },
      selectedInstallation: { value: null },
      selectedProviderId: { value: null },
      selectedIsRunning: { value: false },
      refresh,
      select,
      addInstallation: vi.fn(),
      removeInstallation: vi.fn(),
      migrateLegacySelection: vi.fn().mockResolvedValue(undefined),
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
  checkCdpStatus,
  getDesktopClientState,
  launchDesktopClientCdp,
  setDesktopClientSelection,
} from '@/api/tauri'

const mockedCheckCdpStatus = vi.mocked(checkCdpStatus)
const mockedGetDesktopClientState = vi.mocked(getDesktopClientState)
const mockedLaunchDesktopClientCdp = vi.mocked(launchDesktopClientCdp)
const mockedSetDesktopClientSelection = vi.mocked(setDesktopClientSelection)

const LOGIN_ACTION = 'Sign in with Discord'

function createDeferred<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((res) => {
    resolve = res
  })
  return { promise, resolve }
}

const ButtonStub = defineComponent({
  name: 'Button',
  inheritAttrs: false,
  props: { disabled: Boolean, variant: String, size: String, type: String },
  emits: ['click'],
  setup(props, { attrs, slots, emit }) {
    return () =>
      h(
        'button',
        {
          id: attrs.id as string | undefined,
          class: attrs.class as string | undefined,
          type: props.type ?? 'button',
          disabled: props.disabled,
          onClick: (event: MouseEvent) => emit('click', event),
        },
        slots.default?.(),
      )
  },
})

// Mirrors the production `Input`: a native input that casts to a number when the
// element type is `number`, so `v-model.number` receives a real number.
const NumberInputStub = defineComponent({
  name: 'Input',
  inheritAttrs: false,
  props: {
    modelValue: { type: [String, Number], default: '' },
    type: String,
    placeholder: String,
    disabled: Boolean,
  },
  emits: ['update:modelValue'],
  setup(props, { attrs, emit }) {
    return () =>
      h('input', {
        id: attrs.id as string | undefined,
        type: props.type,
        min: attrs.min as string | undefined,
        max: attrs.max as string | undefined,
        placeholder: props.placeholder,
        disabled: props.disabled,
        value: props.modelValue,
        onInput: (event: Event) => {
          const element = event.target as HTMLInputElement
          const raw = element.value
          emit('update:modelValue', element.type === 'number' && raw !== '' ? Number(raw) : raw)
        },
      })
  },
})

const PassthroughStub = (name: string) =>
  defineComponent({
    name,
    inheritAttrs: false,
    setup(_, { slots }) {
      return () => h('div', slots.default?.())
    },
  })

// Mirrors the production dialog: its slot only exists while `open` is true, so a
// closed dialog never contributes stray copy or buttons to the mounted tree.
const AlertDialogStub = defineComponent({
  name: 'AlertDialog',
  inheritAttrs: false,
  props: { open: Boolean },
  setup(props, { slots }) {
    return () => (props.open ? h('div', slots.default?.()) : null)
  },
})

// The confirm action is a clickable control (not a passive wrapper), so the
// restart dialog path can be exercised exactly as rendered.
const AlertDialogActionStub = defineComponent({
  name: 'AlertDialogAction',
  inheritAttrs: false,
  emits: ['click'],
  setup(_, { slots, emit }) {
    return () => h('button', { onClick: (event: MouseEvent) => emit('click', event) }, slots.default?.())
  },
})

const stubs = {
  Button: ButtonStub,
  Input: NumberInputStub,
  Label: PassthroughStub('Label'),
  AlertDialog: AlertDialogStub,
  AlertDialogContent: PassthroughStub('AlertDialogContent'),
  AlertDialogHeader: PassthroughStub('AlertDialogHeader'),
  AlertDialogTitle: PassthroughStub('AlertDialogTitle'),
  AlertDialogDescription: PassthroughStub('AlertDialogDescription'),
  AlertDialogFooter: PassthroughStub('AlertDialogFooter'),
  AlertDialogAction: AlertDialogActionStub,
  AlertDialogCancel: PassthroughStub('AlertDialogCancel'),
}

const i18n = createI18n({
  legacy: false,
  locale: 'en',
  missingWarn: false,
  fallbackWarn: false,
  messages: {
    en: {
      general: { title: 'DQH', welcome: 'Welcome', login_prompt: 'Log in to continue' },
      dialog: { cancel: 'Cancel' },
      settings: {
        cdp_checking: 'Checking...',
        cdp_connected: 'Connected',
        cdp_disconnected_short: 'Disconnected',
        cdp_dialog_confirm: 'Restart',
      },
      auth: {
        cdp_login: 'Discord CDP login',
        cdp_login_action: LOGIN_ACTION,
        cdp_status_starting: 'Starting',
        cdp_status_error: 'Error',
        cdp_port_label: 'CDP port',
        cdp_port_placeholder: 'Port',
        cdp_port_detect: 'Detect',
        cdp_port_hint: 'Port hint',
        cdp_detected_ports: 'Detected ports',
        cdp_multi_port_hint: 'Multiple ports hint',
        port_invalid_integer: 'Port must be a whole number',
        port_invalid_range: 'Port must be between 1024 and 65535',
        duplicate_account_title: 'This account is already linked',
        duplicate_account_desc: 'Account {account} is already linked',
        progress: {
          checking_cdp: 'Checking CDP',
          complete: 'Login complete',
          failed: 'Login failed',
        },
      },
    },
  },
})

function endpoint(port: number, status: DesktopClientState['endpoint']['status']): DesktopClientState['endpoint'] {
  return { port, status, owner: 'none', ownerProviderId: null, targetTitle: null }
}

function offlineSnapshot(port: number): DesktopClientState {
  return {
    installations: [],
    processes: [],
    endpoint: endpoint(port, 'unreachable'),
    selection: { kind: 'auto' },
    discoveryIssues: [],
    port,
    revision: 1,
  }
}

function readySnapshot(port: number): DesktopClientState {
  return {
    installations: [],
    processes: [],
    endpoint: endpoint(port, 'discordReady'),
    selection: { kind: 'auto' },
    discoveryIssues: [],
    port,
    revision: 1,
  }
}

const stableInstallation: ClientInstallation = {
  id: 'discord.official:stable',
  providerId: 'discord.official',
  variantId: 'stable',
  displayName: 'Discord',
  source: 'standardPath',
  launchTarget: { kind: 'executable', path: '/opt/Discord', workingDir: '/opt', prefixArgs: [] },
  capabilities: { cdp: true, localToken: true, restoreNormal: true },
  validation: 'valid',
}

function installedSnapshot(port: number): DesktopClientState {
  return {
    installations: [stableInstallation],
    processes: [],
    endpoint: endpoint(port, 'unreachable'),
    selection: { kind: 'auto' },
    discoveryIssues: [],
    port,
    revision: 1,
  }
}

function cdp(available: boolean): CdpStatus {
  return { available, connected: available, target_title: null, error: available ? null : 'unreachable' }
}

function mountPanel(props: { allowPortSelection?: boolean } = {}) {
  return mount(LoginPanel, { props, global: { plugins: [i18n], stubs } })
}

function buttonByText(wrapper: VueWrapper, text: string) {
  return wrapper.findAll('button').find(button => button.text().includes(text))
}

function detectedPortButton(wrapper: VueWrapper, port: number) {
  return wrapper.findAll('button').find(button => button.text().includes(String(port)))
}

describe('LoginPanel Phase 6.5 port integration', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    localStorage.clear()

    authMock.user = null
    authMock.loading = false
    authMock.error = null
    authMock.accounts = []
    authMock.activeAccountId = null
    authMock.accountPorts = {}
    authMock.portForAccount.mockImplementation(
      (id: string) => authMock.accountPorts[id] ?? questsMock.cdpPort,
    )
    authMock.duplicateLoginAccountId = null
    authMock.addAccountViaCdp.mockResolvedValue(true)
    questsMock.cdpPort = 9223
    questsMock.cdpAvailable = false
    questsMock.desktopClient = 'auto'

    refreshMock.mockImplementation(async (port: number) => {
      const snapshot = await mockedGetDesktopClientState(port)
      if (snapshot && snapshot.port === port) clientsState.value = snapshot
      return snapshot
    })
    selectMock.mockImplementation((selection: ClientSelection, port: number) =>
      mockedSetDesktopClientSelection(selection, port))
    mockedGetDesktopClientState.mockImplementation(async (port?: number) => offlineSnapshot(port ?? 9223))
    mockedCheckCdpStatus.mockResolvedValue(cdp(false))
    mockedLaunchDesktopClientCdp.mockResolvedValue({} as never)
  })

  it('defaults to the global port, detects 9224, and updates the value from the detected button', async () => {
    mockedCheckCdpStatus.mockImplementation(async (port?: number) => cdp(port === 9224))
    mockedGetDesktopClientState.mockImplementation(async (port?: number) =>
      (port === 9224 ? offlineSnapshot(9224) : offlineSnapshot(port ?? 9223)))

    const wrapper = mountPanel({ allowPortSelection: true })
    await flushPromises()

    // The add dialog starts on the global default.
    expect((wrapper.find('#cdp-port').element as HTMLInputElement).value).toBe('9223')

    // 9224 was detected and its button selects it.
    const port9224 = detectedPortButton(wrapper, 9224)
    expect(port9224, 'detected 9224 button').toBeTruthy()
    await port9224!.trigger('click')
    await flushPromises()
    expect((wrapper.find('#cdp-port').element as HTMLInputElement).value).toBe('9224')

    // An out-of-range port blocks login with an inline hint and no IPC.
    await wrapper.find('#cdp-port').setValue('80')
    await nextTick()
    await buttonByText(wrapper, LOGIN_ACTION)!.trigger('click')
    await flushPromises()

    expect(authMock.loginViaCdp).not.toHaveBeenCalled()
    expect(authMock.addAccountViaCdp).not.toHaveBeenCalled()
    expect(wrapper.text()).toContain('between 1024 and 65535')
    expect(wrapper.emitted('navigateToHome')).toBeUndefined()

    wrapper.unmount()
  })

  it('rejects fraction, out-of-range, and empty ports before any login IPC', async () => {
    const wrapper = mountPanel({ allowPortSelection: true })
    await flushPromises()

    for (const raw of ['9223.5', '80', '70000', '']) {
      await wrapper.find('#cdp-port').setValue(raw)
      await nextTick()
      await buttonByText(wrapper, LOGIN_ACTION)!.trigger('click')
      await flushPromises()

      expect(authMock.loginViaCdp).not.toHaveBeenCalled()
      expect(authMock.addAccountViaCdp).not.toHaveBeenCalled()
      expect(wrapper.text()).toContain('Port must be')
    }

    wrapper.unmount()
  })

  it('keeps the ready status when the concurrent 9223-9226 detection runs', async () => {
    mockedGetDesktopClientState.mockResolvedValue(readySnapshot(9223))
    mockedCheckCdpStatus.mockImplementation(async () => cdp(false))

    const wrapper = mountPanel({ allowPortSelection: true })
    await flushPromises()

    // The concurrent detection must not have cancelled the ready probe.
    expect(mockedGetDesktopClientState).toHaveBeenCalledWith(9223)
    expect(wrapper.text()).toContain('Connected')

    wrapper.unmount()
  })

  it('probes the newly selected port and lets no late 9223 status override it', async () => {
    let release9223: (snapshot: DesktopClientState) => void = () => {}
    const pending9223 = new Promise<DesktopClientState>((resolve) => {
      release9223 = resolve
    })
    mockedGetDesktopClientState.mockImplementation((port?: number) => {
      if (port === 9223) return pending9223
      if (port === 9224) return Promise.resolve(readySnapshot(9224))
      return Promise.resolve(offlineSnapshot(port ?? 0))
    })
    mockedCheckCdpStatus.mockImplementation(async (port?: number) => cdp(port === 9224))

    const wrapper = mountPanel({ allowPortSelection: true })
    await flushPromises()

    // Switch port while the 9223 probe is still in flight.
    await detectedPortButton(wrapper, 9224)!.trigger('click')
    await flushPromises()

    expect(mockedGetDesktopClientState).toHaveBeenCalledWith(9224)
    expect(wrapper.text()).toContain('Connected')

    // A late offline 9223 response must not overwrite the 9224 status.
    release9223(offlineSnapshot(9223))
    await flushPromises()

    expect(wrapper.text()).toContain('Connected')
    expect(wrapper.text()).not.toContain('Disconnected')

    wrapper.unmount()
  })

  it('uses the add-only command and selected port for CDP login in add mode', async () => {
    mockedGetDesktopClientState.mockImplementation(async (port?: number) =>
      (port === 9224 ? installedSnapshot(9224) : offlineSnapshot(port ?? 9223)))
    mockedLaunchDesktopClientCdp.mockResolvedValue({} as never)

    const wrapper = mountPanel({ allowPortSelection: true })
    await flushPromises()

    await wrapper.find('#cdp-port').setValue('9224')
    await flushPromises()
    await buttonByText(wrapper, LOGIN_ACTION)!.trigger('click')
    await flushPromises()

    expect(mockedGetDesktopClientState).toHaveBeenCalledWith(9224)
    expect(mockedLaunchDesktopClientCdp).toHaveBeenCalledTimes(1)
    expect(mockedLaunchDesktopClientCdp.mock.calls[0][0]).toBe(9224)
    expect(authMock.addAccountViaCdp).toHaveBeenCalledTimes(1)
    expect(authMock.addAccountViaCdp.mock.calls[0][1]).toEqual({ port: 9224 })
    expect(authMock.loginViaCdp).not.toHaveBeenCalled()

    wrapper.unmount()
  })

  it('keeps the standalone path on the global default port', async () => {
    questsMock.cdpPort = 9223
    mockedGetDesktopClientState.mockImplementation(async (port?: number) => readySnapshot(port ?? 9223))
    authMock.loginViaCdp.mockResolvedValue(true)

    const wrapper = mountPanel({ allowPortSelection: false })
    await flushPromises()

    await buttonByText(wrapper, LOGIN_ACTION)!.trigger('click')
    await flushPromises()

    expect(mockedGetDesktopClientState).toHaveBeenCalledWith(9223)
    expect(mockedLaunchDesktopClientCdp).not.toHaveBeenCalled()
    expect(authMock.loginViaCdp).toHaveBeenCalledTimes(1)
    expect(authMock.loginViaCdp.mock.calls[0][1]).toEqual({ port: 9223 })

    wrapper.unmount()
  })

  it('uses the selected offline account saved port for normal CDP reauthentication', async () => {
    questsMock.cdpPort = 9223
    authMock.activeAccountId = 'B'
    authMock.accountPorts = { B: 9224 }
    authMock.accounts = [{ id: 'B', username: 'bob', isAuthenticated: false }]
    mockedGetDesktopClientState.mockImplementation(async (port?: number) => offlineSnapshot(port ?? 9223))
    authMock.loginViaCdp.mockResolvedValue(true)

    const wrapper = mountPanel({ allowPortSelection: false })
    await flushPromises()

    await buttonByText(wrapper, LOGIN_ACTION)!.trigger('click')
    await flushPromises()

    expect(mockedGetDesktopClientState).toHaveBeenCalledWith(9224)
    expect(mockedGetDesktopClientState).not.toHaveBeenCalledWith(9223)
    expect(mockedLaunchDesktopClientCdp.mock.calls[0][0]).toBe(9224)
    expect(authMock.loginViaCdp).toHaveBeenCalledTimes(1)
    expect(authMock.loginViaCdp.mock.calls[0][1]).toEqual({ port: 9224 })
    expect(authMock.addAccountViaCdp).not.toHaveBeenCalled()

    wrapper.unmount()
  })

  it('keeps the 9224 installation and launch port when a stale 9223 scan owns shared state', async () => {
    const snapshot9224: DesktopClientState = {
      installations: [stableInstallation],
      processes: [
        {
          providerId: 'discord.official',
          installationId: stableInstallation.id,
          variantId: 'stable',
          executablePath: null,
          running: true,
        },
      ],
      endpoint: endpoint(9224, 'unreachable'),
      selection: { kind: 'installation', installationId: stableInstallation.id },
      discoveryIssues: [],
      port: 9224,
      revision: 1,
    }
    const snapshot9223: DesktopClientState = {
      ...offlineSnapshot(9223),
      selection: { kind: 'provider', providerId: 'vencord.vesktop', variantId: null },
    }

    // A concurrent global scan owns the shared composable state on 9223 for the
    // entire flow; the 9224 refresh returns its own snapshot without touching it.
    clientsState.value = snapshot9223
    refreshMock.mockImplementation(async (port: number) => (port === 9224 ? snapshot9224 : snapshot9223))
    mockedCheckCdpStatus.mockResolvedValue(cdp(false))
    mockedLaunchDesktopClientCdp.mockResolvedValue({} as never)
    authMock.addAccountViaCdp.mockResolvedValue(true)

    const wrapper = mountPanel({ allowPortSelection: true })
    await flushPromises()

    await wrapper.find('#cdp-port').setValue('9224')
    await flushPromises()

    await buttonByText(wrapper, LOGIN_ACTION)!.trigger('click')
    await flushPromises()
    await nextTick()

    // The 9224 selection is already running, so the panel opens the restart
    // dialog instead of launching the (unrelated) 9223 client.
    expect(mockedLaunchDesktopClientCdp).not.toHaveBeenCalled()
    const confirm = buttonByText(wrapper, 'Restart')
    expect(confirm, 'restart confirm button').toBeTruthy()

    await confirm!.trigger('click')
    await flushPromises()

    expect(mockedLaunchDesktopClientCdp).toHaveBeenCalledTimes(1)
    expect(mockedLaunchDesktopClientCdp.mock.calls[0][0]).toBe(9224)
    expect(mockedLaunchDesktopClientCdp.mock.calls[0][1]).toEqual({
      kind: 'installation',
      installationId: stableInstallation.id,
    })

    wrapper.unmount()
  })

  it('emits navigateToHome exactly once for a newly added account', async () => {
    mockedGetDesktopClientState.mockResolvedValue(readySnapshot(9223))
    const deferred = createDeferred<boolean>()
    authMock.accounts = [{ id: 'A', username: 'alice', isAuthenticated: true }]
    authMock.addAccountViaCdp.mockImplementation(async () => {
      const succeeded = await deferred.promise
      if (succeeded) {
        authMock.accounts = [
          ...authMock.accounts,
          { id: 'B', username: 'bob', isAuthenticated: true },
        ]
      }
      return succeeded
    })

    const wrapper = mountPanel({ allowPortSelection: true })
    await flushPromises()

    await buttonByText(wrapper, LOGIN_ACTION)!.trigger('click')
    await flushPromises()
    expect(authMock.addAccountViaCdp).toHaveBeenCalledTimes(1)
    expect(authMock.addAccountViaCdp.mock.calls[0][1]).toEqual({ port: 9223 })
    expect(authMock.loginViaCdp).not.toHaveBeenCalled()
    // Nothing is emitted while the login is still pending.
    expect(wrapper.emitted('navigateToHome')).toBeUndefined()

    deferred.resolve(true)
    await flushPromises()
    expect(authMock.accounts).toContainEqual({ id: 'B', username: 'bob', isAuthenticated: true })
    expect(wrapper.emitted('navigateToHome')).toHaveLength(1)

    wrapper.unmount()
  })

  it('keeps the add dialog open and replaces completion progress with the duplicate notice', async () => {
    mockedGetDesktopClientState.mockResolvedValue(readySnapshot(9223))
    authMock.accounts = [{ id: 'B', username: 'bob', globalName: 'Bob', isAuthenticated: true }]
    authMock.addAccountViaCdp.mockImplementation(async (onProgress?: (event: AuthProgress) => void) => {
      onProgress?.({ phase: 'complete', current: null, total: null, valid_accounts: null })
      authMock.duplicateLoginAccountId = 'B'
      return true
    })

    const wrapper = mountPanel({ allowPortSelection: true })
    await flushPromises()

    await buttonByText(wrapper, LOGIN_ACTION)!.trigger('click')
    await flushPromises()
    await nextTick()

    expect(authMock.addAccountViaCdp).toHaveBeenCalledTimes(1)
    expect(authMock.loginViaCdp).not.toHaveBeenCalled()
    expect(wrapper.emitted('navigateToHome')).toBeUndefined()
    expect(wrapper.find('.login-panel-stage').exists()).toBe(true)
    expect(wrapper.text()).toContain('already linked')
    expect(wrapper.text()).toContain('Bob')
    expect(wrapper.text()).not.toContain('Login complete')

    wrapper.unmount()
  })

  it('keeps the add panel open and shows the backend error when adding fails', async () => {
    mockedGetDesktopClientState.mockResolvedValue(readySnapshot(9223))
    authMock.addAccountViaCdp.mockImplementation(async () => {
      authMock.error = 'Discord session could not be captured'
      return false
    })

    const wrapper = mountPanel({ allowPortSelection: true })
    await flushPromises()

    await buttonByText(wrapper, LOGIN_ACTION)!.trigger('click')
    await flushPromises()

    expect(authMock.addAccountViaCdp).toHaveBeenCalledTimes(1)
    expect(authMock.loginViaCdp).not.toHaveBeenCalled()
    expect(wrapper.emitted('navigateToHome')).toBeUndefined()
    expect(wrapper.find('.login-panel-stage').exists()).toBe(true)
    expect(wrapper.text()).toContain('Discord session could not be captured')

    wrapper.unmount()
  })
})
