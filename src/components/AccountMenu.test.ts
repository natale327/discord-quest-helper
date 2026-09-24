// @vitest-environment happy-dom
import { describe, it, expect, vi, beforeEach } from 'vitest'
import { mount, flushPromises } from '@vue/test-utils'
import { defineComponent, h } from 'vue'
import { createI18n } from 'vue-i18n'
import AccountMenu from './AccountMenu.vue'

const { authMock } = vi.hoisted(() => ({
  authMock: {
    accounts: [] as unknown[],
    activeAccountId: null as string | null,
    portForAccount: vi.fn((_id: string) => 9223),
    loadAccounts: vi.fn().mockResolvedValue(undefined),
    switchOnlineAccount: vi.fn(),
    activateAccount: vi.fn(),
    removeAccount: vi.fn(),
  },
}))

vi.mock('@/stores/auth', async () => {
  const { reactive } = await import('vue')
  return { useAuthStore: () => reactive(authMock) }
})

const PassthroughStub = (name: string) => defineComponent({
  name,
  inheritAttrs: false,
  setup(_, { slots }) {
    return () => h('span', slots.default?.())
  },
})

const ButtonStub = defineComponent({
  name: 'Button',
  inheritAttrs: false,
  props: { disabled: Boolean, variant: String, size: String, type: String },
  emits: ['click'],
  setup(props, { attrs, slots, emit }) {
    return () => h('button', {
      type: props.type ?? 'button',
      disabled: props.disabled,
      class: attrs.class as string | undefined,
      onClick: (event: MouseEvent) => emit('click', event),
    }, slots.default?.())
  },
})

const i18n = createI18n({
  legacy: false,
  locale: 'en',
  missingWarn: false,
  fallbackWarn: false,
  messages: {
    en: {
      accounts: {
        switcher_title: 'Accounts',
        switcher_empty: 'No accounts',
        switcher_empty_desc: 'Add an account',
        add_account: 'Add account',
        logout: 'Log out',
        active: 'Active',
        offline: 'Not signed in',
        offline_hint: 'Saved, but needs sign-in',
        choose_client: 'Choose a client',
    reconnect_account: 'Reconnect',
    reconnect_account_named: 'Reconnect {account}',
    cdp_port_unassigned: 'Port unassigned — select a port',
      },
    },
  },
})

const stubs = {
  Button: ButtonStub,
  Avatar: PassthroughStub('Avatar'),
  AvatarFallback: PassthroughStub('AvatarFallback'),
  AvatarImage: PassthroughStub('AvatarImage'),
  RemoveAccountDialog: defineComponent({
    name: 'RemoveAccountDialog',
    template: '<div />',
  }),
}

function buttonByText(wrapper: ReturnType<typeof mount>, text: string) {
  return wrapper.findAll('button').find(button => button.text().includes(text))
}

describe('AccountMenu client-first account actions', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    authMock.accounts = [
      { id: 'A', username: 'alice', globalName: 'Alice', isAuthenticated: true },
      { id: 'B', username: 'bob', globalName: 'Bob', isAuthenticated: false },
      { id: 'C', username: 'chris', globalName: 'Chris', isAuthenticated: true },
    ]
    authMock.activeAccountId = 'A'
    authMock.portForAccount.mockReturnValue(9223)
    authMock.loadAccounts.mockResolvedValue(undefined)
    authMock.switchOnlineAccount.mockImplementation(async (id: string) => ({ id }))
  })

  it('opens a targeted Reconnect flow for an offline profile without activating it', async () => {
    const wrapper = mount(AccountMenu, { global: { plugins: [i18n], stubs } })
    await flushPromises()

    await wrapper.find('button[aria-haspopup="dialog"]').trigger('click')
    await flushPromises()
    await buttonByText(wrapper, 'Reconnect')!.trigger('click')
    await flushPromises()

    expect(wrapper.emitted('reconnectAccount')).toEqual([['B']])
    expect(authMock.activateAccount).not.toHaveBeenCalled()
    expect(authMock.switchOnlineAccount).not.toHaveBeenCalled()

    wrapper.unmount()
  })

  it('switches an online account through the online-only store action', async () => {
    const wrapper = mount(AccountMenu, { global: { plugins: [i18n], stubs } })
    await flushPromises()

    await wrapper.find('button[aria-haspopup="dialog"]').trigger('click')
    await flushPromises()
    await buttonByText(wrapper, '@chris')!.trigger('click')
    await flushPromises()

    expect(authMock.switchOnlineAccount).toHaveBeenCalledWith('C')
    expect(authMock.activateAccount).not.toHaveBeenCalled()
    expect(wrapper.emitted('reconnectAccount')).toBeUndefined()

    wrapper.unmount()
  })

  it('shows a blocked port honestly and opens Reconnect without selecting the profile', async () => {
    authMock.portForAccount.mockImplementation((id: string) => id === 'B' ? 0 : 9223)
    const wrapper = mount(AccountMenu, { global: { plugins: [i18n], stubs } })
    await flushPromises()

    await wrapper.find('button[aria-haspopup="dialog"]').trigger('click')
    await flushPromises()

    expect(wrapper.text()).toContain('Port unassigned — select a port')
    await buttonByText(wrapper, 'Choose a client')!.trigger('click')
    await flushPromises()

    expect(wrapper.emitted('reconnectAccount')).toEqual([['B']])
    expect(authMock.switchOnlineAccount).not.toHaveBeenCalled()
    expect(authMock.activateAccount).not.toHaveBeenCalled()

    wrapper.unmount()
  })
})
