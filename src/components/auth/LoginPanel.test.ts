// @vitest-environment happy-dom
import { describe, it, expect, vi, beforeEach } from 'vitest'
import { mount, flushPromises, type VueWrapper } from '@vue/test-utils'
import { defineComponent, h } from 'vue'
import { createI18n } from 'vue-i18n'

vi.mock('/icons/logo.png', () => ({ default: '/icons/logo.png' }))

const { authMock, questsMock, clientsMock } = vi.hoisted(() => ({
  authMock: {
    user: null as unknown,
    loading: false,
    error: null as string | null,
    accounts: [] as unknown[],
    activeAccountId: null as string | null,
    accountPorts: {} as Record<string, number | null>,
    duplicateLoginAccountId: null as string | null,
    portForAccount: vi.fn((_id: string) => 0),
    suggestAccountPort: vi.fn(() => 9223),
    isAccountPortAvailable: vi.fn(() => true),
    loginViaCdp: vi.fn(),
    addAccountViaCdp: vi.fn(),
  },
  questsMock: {
    cdpPort: 9223,
    cdpAvailable: false,
    desktopClient: 'auto' as string,
  },
  clientsMock: {
    refresh: vi.fn(),
    migrateLegacySelection: vi.fn().mockResolvedValue(undefined),
    state: { value: null as unknown },
    loading: { value: false },
    error: { value: null as string | null },
    select: vi.fn(),
    addInstallation: vi.fn(),
    removeInstallation: vi.fn(),
    installationPath: vi.fn(() => undefined),
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
vi.mock('@/composables/desktopClientState', () => ({
  useDesktopClientState: () => clientsMock,
  desktopClientArgForProvider: () => 'official',
}))

import LoginPanel from './LoginPanel.vue'
import type { DesktopClientState } from '@/api/tauri'

const ButtonStub = defineComponent({
  name: 'Button',
  inheritAttrs: false,
  props: { disabled: Boolean, variant: String, size: String, type: String },
  emits: ['click'],
  setup(props, { attrs, slots, emit }) {
    return () => h('button', {
      id: attrs.id as string | undefined,
      class: attrs.class as string | undefined,
      type: props.type ?? 'button',
      disabled: props.disabled,
      onClick: (event: MouseEvent) => emit('click', event),
    }, slots.default?.())
  },
})

const PassthroughStub = (name: string) => defineComponent({
  name,
  inheritAttrs: false,
  setup(_, { slots }) {
    return () => h('div', slots.default?.())
  },
})

const stubs = {
  Button: ButtonStub,
  Input: PassthroughStub('Input'),
  Label: PassthroughStub('Label'),
  AlertDialog: PassthroughStub('AlertDialog'),
  AlertDialogContent: PassthroughStub('AlertDialogContent'),
  AlertDialogHeader: PassthroughStub('AlertDialogHeader'),
  AlertDialogTitle: PassthroughStub('AlertDialogTitle'),
  AlertDialogDescription: PassthroughStub('AlertDialogDescription'),
  AlertDialogFooter: PassthroughStub('AlertDialogFooter'),
  AlertDialogAction: PassthroughStub('AlertDialogAction'),
  AlertDialogCancel: PassthroughStub('AlertDialogCancel'),
}

const i18n = createI18n({
  legacy: false,
  locale: 'en',
  missingWarn: false,
  fallbackWarn: false,
  messages: {
    en: {
      general: { title: 'Quest Helper', welcome: 'Welcome', login_prompt: 'Sign in to continue' },
      settings: {
        cdp_checking: 'Checking...',
        cdp_connected: 'Connected',
        cdp_disconnected_short: 'Not connected',
      },
      auth: {
        cdp_login: 'Discord client',
        cdp_status_starting: 'Starting',
        cdp_status_error: 'Status check failed',
        cdp_login_detail: 'Use a verified Discord client.',
        cdp_login_detail_vesktop: 'Use a verified Vesktop client.',
        cdp_choose_title: 'Choose a Discord client',
        account_cdp_port_unassigned: 'Choose a client or assign a port in Settings.',
      },
      accounts: { reconnect_account_named: 'Reconnect {account}' },
    },
  },
})

function readySnapshot(port: number): DesktopClientState {
  return {
    installations: [],
    processes: [],
    endpoint: {
      port,
      status: 'discordReady',
      owner: 'none',
      ownerProviderId: null,
      targetTitle: null,
    },
    selection: { kind: 'auto' },
    discoveryIssues: [],
    port,
    revision: 1,
  }
}

function mountPanel(props: { targetAccountId?: string } = {}) {
  return mount(LoginPanel, { props, global: { plugins: [i18n], stubs } })
}

function buttonByText(wrapper: VueWrapper, text: string) {
  return wrapper.findAll('button').find(button => button.text().includes(text))
}

describe('LoginPanel client-first entry point', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    localStorage.clear()
    authMock.user = null
    authMock.loading = false
    authMock.error = null
    authMock.accounts = []
    authMock.activeAccountId = null
    authMock.accountPorts = {}
    authMock.duplicateLoginAccountId = null
    authMock.portForAccount.mockImplementation((id: string) => authMock.accountPorts[id] ?? questsMock.cdpPort)
    authMock.suggestAccountPort.mockReturnValue(9223)
    authMock.isAccountPortAvailable.mockReturnValue(true)
    questsMock.cdpPort = 9223
    questsMock.cdpAvailable = false
    questsMock.desktopClient = 'auto'
    clientsMock.refresh.mockImplementation(async (port: number) => readySnapshot(port))
    clientsMock.migrateLegacySelection.mockResolvedValue(undefined)
  })

  it('opens a B-targeted client picker without using legacy login or the active account port', async () => {
    authMock.activeAccountId = 'A'
    authMock.accounts = [
      { id: 'A', username: 'alice', isAuthenticated: true },
      { id: 'B', username: 'bob', globalName: 'Bob', isAuthenticated: false },
    ]
    authMock.accountPorts = { A: 9223, B: null }
    authMock.portForAccount.mockImplementation((id: string) => id === 'B' ? 0 : 9223)

    const wrapper = mountPanel({ targetAccountId: 'B' })
    await flushPromises()

    const reconnect = buttonByText(wrapper, 'Reconnect Bob')
    expect(reconnect).toBeTruthy()
    await reconnect!.trigger('click')
    await flushPromises()

    expect(wrapper.emitted('openClientPicker')).toEqual([[{ mode: 'reconnect', accountId: 'B' }]])
    expect(authMock.loginViaCdp).not.toHaveBeenCalled()
    expect(authMock.addAccountViaCdp).not.toHaveBeenCalled()
    expect(clientsMock.refresh).not.toHaveBeenCalledWith(0)

    wrapper.unmount()
  })

  it('opens Add for a first account instead of capturing through the legacy login action', async () => {
    const wrapper = mountPanel()
    await flushPromises()

    await buttonByText(wrapper, 'Choose a Discord client')!.trigger('click')
    await flushPromises()

    expect(wrapper.emitted('openClientPicker')).toEqual([[{ mode: 'add' }]])
    expect(authMock.loginViaCdp).not.toHaveBeenCalled()
    expect(authMock.addAccountViaCdp).not.toHaveBeenCalled()

    wrapper.unmount()
  })
})
