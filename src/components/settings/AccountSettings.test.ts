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
    loadAccounts: vi.fn().mockResolvedValue(undefined),
    portForAccount: vi.fn(() => 9223),
    setAccountPort: vi.fn(),
    logout: vi.fn(),
    loginViaCdp: vi.fn(),
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
      auth: { authenticated_as: 'Signed in as', cdp_login: 'Discord CDP login' },
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
    authMock.accounts = [
      { id: 'acc-1', username: 'alice', globalName: 'Alice', isAuthenticated: true, lastCdpPort: 9223 },
    ]
    authMock.loadAccounts.mockResolvedValue(undefined)
    authMock.portForAccount.mockReturnValue(9223)
    authMock.setAccountPort.mockClear()
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
})
