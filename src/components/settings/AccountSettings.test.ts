// @vitest-environment happy-dom
import { describe, it, expect, vi, beforeEach } from 'vitest'
import { mount, flushPromises, type VueWrapper } from '@vue/test-utils'
import { defineComponent, h, nextTick } from 'vue'
import { createI18n } from 'vue-i18n'
import AccountSettings from './AccountSettings.vue'

// Keep the import graph resolvable without running real IPC.
vi.mock('@/api/tauri', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/api/tauri')>()
  return { ...actual, listAccounts: vi.fn() }
})

const { authMock, questsMock } = vi.hoisted(() => ({
  authMock: {
    user: null as unknown,
    loading: false,
    error: null as string | null,
    accounts: [] as unknown[],
    activeAccountId: null as string | null,
    accountPorts: {} as Record<string, number | null>,
    loadAccounts: vi.fn().mockResolvedValue(undefined),
    portForAccount: vi.fn((_id: string) => 9223),
    isAccountPortAvailable: vi.fn((_port: number, _excludingAccountId?: string) => true),
    suggestAccountPort: vi.fn((_excludingAccountId?: string) => 9224),
    setAccountPort: vi.fn((_accountId: string, _port: number) => true),
    logout: vi.fn(),
    loginViaCdp: vi.fn(),
    switchOnlineAccount: vi.fn(),
    removeAccount: vi.fn(),
    activateAccount: vi.fn(),
  },
  questsMock: {
    cdpPort: 9223,
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

// Mirrors the production `Input`: casts to a number for `type="number"`.
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

// A no-attrs passthrough so component-valued props (e.g. `:icon`) never leak
// onto the DOM.
const PassthroughStub = (name: string) =>
  defineComponent({
    name,
    inheritAttrs: false,
    setup(_, { slots }) {
      return () => h('div', slots.default?.())
    },
  })

const stubs = {
  Button: ButtonStub,
  Input: NumberInputStub,
  Badge: PassthroughStub('Badge'),
  SettingsSectionCard: PassthroughStub('SettingsSectionCard'),
  SettingsStatusPanel: PassthroughStub('SettingsStatusPanel'),
  AccountListItem: PassthroughStub('AccountListItem'),
  RemoveAccountDialog: PassthroughStub('RemoveAccountDialog'),
  AccountProxyPanel: PassthroughStub('AccountProxyPanel'),
}

const i18n = createI18n({
  legacy: false,
  locale: 'en',
  missingWarn: false,
  fallbackWarn: false,
  messages: {
    en: {
      general: { logout: 'Log out', save: 'Save', cancel: 'Cancel', close: 'Close' },
      auth: {
        authenticated_as: 'Signed in as',
        cdp_login: 'Discord CDP login',
        cdp_choose_title: 'Choose a Discord client',
      },
      settings: { account_title: 'Account', account_desc: 'Account description' },
      accounts: {
        manage_title: 'Manage accounts',
        manage_desc: 'Manage description',
        manage_empty: 'No accounts',
        manage_empty_desc: 'No accounts description',
        add_account: 'Add account',
        cdp_port_title: 'CDP ports',
        cdp_port_desc: 'CDP ports description',
        cdp_port_label: 'CDP port',
        cdp_port_override: 'Override',
        cdp_port_placeholder: 'Port',
        cdp_port_edit: 'Edit port',
        cdp_port_global_hint: 'Global default: {port}',
        port_invalid_range: 'Port must be between 1024 and 65535',
        cdp_port_conflict: 'This port is assigned to another account. Choose a different port.',
        cdp_port_save_failed: 'The port could not be saved. Try again.',
        cdp_port_unassigned: 'Port unassigned — select a port',
        cdp_port_recovery_hint: 'This account was saved, but its CDP port could not be assigned. Edit this account and save an available port.',
        choose_client: 'Choose a client',
        reconnect_account: 'Reconnect',
        reconnect_account_named: 'Reconnect {account}',
        proxy_title: 'Proxy',
        proxy_desc: 'Proxy for {account}',
        proxy_configure_title: 'Configure proxy',
        proxy_configure_desc: 'Configure proxy description',
        proxy_select_account: 'Select an account',
      },
    },
  },
})

function mountSettings() {
  return mount(AccountSettings, { global: { plugins: [i18n], stubs } })
}

function buttonByText(wrapper: VueWrapper, text: string) {
  return wrapper.findAll('button').find(button => button.text().includes(text))
}

describe('AccountSettings Phase 6.5 port editor', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    localStorage.clear()
    authMock.user = null
    authMock.loading = false
    authMock.error = null
    authMock.activeAccountId = 'acc-1'
    authMock.accountPorts = { 'acc-1': 9223 }
    authMock.accounts = [
      { id: 'acc-1', username: 'alice', globalName: 'Alice', isAuthenticated: true, lastCdpPort: 9223 },
    ]
    authMock.loadAccounts.mockResolvedValue(undefined)
    authMock.loginViaCdp.mockReset()
    authMock.switchOnlineAccount.mockReset()
    authMock.portForAccount.mockImplementation((id: string) => {
      if (Object.prototype.hasOwnProperty.call(authMock.accountPorts, id)) {
        return authMock.accountPorts[id] ?? 0
      }
      const account = (authMock.accounts as Array<{ id: string; lastCdpPort?: number }>).find(item => item.id === id)
      return account?.lastCdpPort || questsMock.cdpPort
    })
    authMock.isAccountPortAvailable.mockImplementation((port: number, excludingAccountId?: string) => {
      if (!Number.isInteger(port) || port < 1024 || port > 65535) return false
      return (authMock.accounts as Array<{ id: string }>).every(account => (
        account.id === excludingAccountId || authMock.portForAccount(account.id) !== port
      ))
    })
    authMock.suggestAccountPort.mockImplementation((excludingAccountId?: string) => {
      for (let port = questsMock.cdpPort; port <= 65535; port += 1) {
        if (authMock.isAccountPortAvailable(port, excludingAccountId)) return port
      }
      return 0
    })
    authMock.setAccountPort.mockClear()
    authMock.setAccountPort.mockImplementation((accountId: string, port: number) => {
      if (!authMock.isAccountPortAvailable(port, accountId)) return false
      authMock.accountPorts = { ...authMock.accountPorts, [accountId]: port }
      return true
    })
  })

  it('rejects NaN, fractional, and out-of-range ports, then saves a valid one', async () => {
    const wrapper = mountSettings()
    await flushPromises()

    const edit = buttonByText(wrapper, 'Edit port')
    expect(edit, 'edit port button').toBeTruthy()
    await edit!.trigger('click')
    await nextTick()

    const input = wrapper.find('input[type="number"]')
    expect(input.exists()).toBe(true)

    // Fractional value is rejected and never reaches the store.
    await input.setValue('9223.5')
    await buttonByText(wrapper, 'Save')!.trigger('click')
    await flushPromises()
    expect(authMock.setAccountPort).not.toHaveBeenCalled()
    expect(wrapper.text()).toContain('between 1024 and 65535')

    // Below the valid range is rejected.
    await input.setValue('80')
    await buttonByText(wrapper, 'Save')!.trigger('click')
    await flushPromises()
    expect(authMock.setAccountPort).not.toHaveBeenCalled()

    // Above the valid range is rejected.
    await input.setValue('70000')
    await buttonByText(wrapper, 'Save')!.trigger('click')
    await flushPromises()
    expect(authMock.setAccountPort).not.toHaveBeenCalled()

    // A valid port is persisted.
    await input.setValue('9224')
    await buttonByText(wrapper, 'Save')!.trigger('click')
    await flushPromises()
    expect(authMock.setAccountPort).toHaveBeenCalledTimes(1)
    expect(authMock.setAccountPort).toHaveBeenCalledWith('acc-1', 9224)

    wrapper.unmount()
  })

  it('opens targeted reconnect for the active offline account instead of legacy login', async () => {
    authMock.accounts = [
      { id: 'acc-1', username: 'alice', globalName: 'Alice', isAuthenticated: false },
    ]
    const requestHandler = vi.fn()
    window.addEventListener('app:open-client-picker', requestHandler)
    const wrapper = mountSettings()
    await flushPromises()

    await buttonByText(wrapper, 'Reconnect Alice')!.trigger('click')
    await flushPromises()

    expect(authMock.loginViaCdp).not.toHaveBeenCalled()
    expect(authMock.switchOnlineAccount).not.toHaveBeenCalled()
    expect(requestHandler).toHaveBeenCalledTimes(1)
    expect((requestHandler.mock.calls[0][0] as CustomEvent).detail).toEqual({
      mode: 'reconnect',
      accountId: 'acc-1',
    })

    window.removeEventListener('app:open-client-picker', requestHandler)
    wrapper.unmount()
  })

  it('offers client-first recovery for an unassigned account without changing its port', async () => {
    authMock.accountPorts = { 'acc-1': null }
    authMock.suggestAccountPort.mockReturnValue(9224)
    const requestHandler = vi.fn()
    window.addEventListener('app:open-client-picker', requestHandler)
    const wrapper = mountSettings()
    await flushPromises()

    expect(wrapper.text()).toContain('Port unassigned — select a port')
    await buttonByText(wrapper, 'Choose a client')!.trigger('click')
    await flushPromises()

    expect((requestHandler.mock.calls[0][0] as CustomEvent).detail).toEqual({
      mode: 'reconnect',
      accountId: 'acc-1',
    })
    expect(authMock.setAccountPort).not.toHaveBeenCalled()
    expect(authMock.loginViaCdp).not.toHaveBeenCalled()

    window.removeEventListener('app:open-client-picker', requestHandler)
    wrapper.unmount()
  })

  it('closes the editor without persisting when cancelled', async () => {
    const wrapper = mountSettings()
    await flushPromises()

    await buttonByText(wrapper, 'Edit port')!.trigger('click')
    await nextTick()
    await wrapper.find('input[type="number"]').setValue('9224')
    await buttonByText(wrapper, 'Cancel')!.trigger('click')
    await nextTick()

    expect(authMock.setAccountPort).not.toHaveBeenCalled()
    expect(wrapper.find('input[type="number"]').exists()).toBe(false)

    wrapper.unmount()
  })

  it('shows an unassigned account, suggests an edit port, and clears the block only after saving', async () => {
    authMock.accountPorts = { 'acc-1': null }
    authMock.suggestAccountPort.mockReturnValue(9224)

    const wrapper = mountSettings()
    await flushPromises()

    expect(wrapper.text()).toContain('Port unassigned — select a port')
    expect(wrapper.text()).toContain('This account was saved, but its CDP port could not be assigned')
    expect(wrapper.text()).not.toContain('Port: 0')
    expect(wrapper.text()).not.toContain('Override')

    await buttonByText(wrapper, 'Edit port')!.trigger('click')
    expect((wrapper.find('input[type="number"]').element as HTMLInputElement).value).toBe('9224')
    expect(authMock.suggestAccountPort).toHaveBeenCalledWith('acc-1')
    expect(authMock.setAccountPort).not.toHaveBeenCalled()
    expect(wrapper.text()).toContain('Port unassigned — select a port')

    // The suggested value is only a starting point; the account stays blocked
    // until the user explicitly saves an available port.
    await wrapper.find('input[type="number"]').setValue('9225')
    await buttonByText(wrapper, 'Save')!.trigger('click')
    await flushPromises()

    expect(authMock.setAccountPort).toHaveBeenCalledWith('acc-1', 9225)
    expect(wrapper.text()).toContain('CDP port: 9225')
    expect(wrapper.text()).not.toContain('Port unassigned — select a port')
    expect(wrapper.text()).not.toContain('could not be assigned')

    wrapper.unmount()
  })

  it('leaves the blocked-port editor blank when no free suggestion exists', async () => {
    authMock.accountPorts = { 'acc-1': null }
    authMock.suggestAccountPort.mockReturnValue(0)

    const wrapper = mountSettings()
    await flushPromises()
    await buttonByText(wrapper, 'Edit port')!.trigger('click')

    expect((wrapper.find('input[type="number"]').element as HTMLInputElement).value).toBe('')
    expect(wrapper.text()).toContain('Port unassigned — select a port')

    wrapper.unmount()
  })

  it('rejects another account’s port and accepts a distinct port', async () => {
    authMock.accounts = [
      { id: 'acc-1', username: 'alice', globalName: 'Alice', isAuthenticated: true },
      { id: 'acc-2', username: 'bob', globalName: 'Bob', isAuthenticated: true },
    ]
    authMock.accountPorts = { 'acc-1': 9223, 'acc-2': 9224 }

    const wrapper = mountSettings()
    await flushPromises()

    await buttonByText(wrapper, 'Edit port')!.trigger('click')
    const input = wrapper.find('input[type="number"]')
    await input.setValue('9224')
    await buttonByText(wrapper, 'Save')!.trigger('click')
    await nextTick()

    expect(authMock.isAccountPortAvailable).toHaveBeenCalledWith(9224, 'acc-1')
    expect(authMock.setAccountPort).not.toHaveBeenCalled()
    expect(wrapper.text()).toContain('This port is assigned to another account')
    expect(wrapper.find('input[type="number"]').exists()).toBe(true)

    await wrapper.find('input[type="number"]').setValue('9225')
    await buttonByText(wrapper, 'Save')!.trigger('click')
    await flushPromises()

    expect(authMock.setAccountPort).toHaveBeenCalledWith('acc-1', 9225)
    expect(wrapper.find('input[type="number"]').exists()).toBe(false)

    wrapper.unmount()
  })

  it('allows an account to keep its own port and keeps the editor open if saving fails', async () => {
    const wrapper = mountSettings()
    await flushPromises()

    await buttonByText(wrapper, 'Edit port')!.trigger('click')
    await wrapper.find('input[type="number"]').setValue('9223')
    await buttonByText(wrapper, 'Save')!.trigger('click')
    await flushPromises()

    expect(authMock.isAccountPortAvailable).toHaveBeenCalledWith(9223, 'acc-1')
    expect(authMock.setAccountPort).toHaveBeenCalledWith('acc-1', 9223)
    expect(wrapper.find('input[type="number"]').exists()).toBe(false)

    await buttonByText(wrapper, 'Edit port')!.trigger('click')
    authMock.setAccountPort.mockReturnValue(false)
    await wrapper.find('input[type="number"]').setValue('9224')
    await buttonByText(wrapper, 'Save')!.trigger('click')
    await nextTick()

    expect(wrapper.find('input[type="number"]').exists()).toBe(true)
    expect(wrapper.text()).toContain('The port could not be saved')

    wrapper.unmount()
  })
})
