// @vitest-environment happy-dom
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount, type VueWrapper } from '@vue/test-utils'
import { defineComponent, h, reactive } from 'vue'
import { createI18n } from 'vue-i18n'
import type { DiscordUser, DesktopClientState } from '@/api/tauri'
import type { ClientAccountCandidate } from '@/composables/desktopClientState'
import CdpClientPicker from './CdpClientPicker.vue'

const { authMock, discoveryState, refreshMock, launchMock } = vi.hoisted(() => ({
  authMock: {
    user: null as unknown,
    loading: false,
    error: null as string | null,
    accounts: [] as unknown[],
    activeAccountId: null as string | null,
    accountPorts: {} as Record<string, number | null>,
    portForAccount: vi.fn((_id: string) => 9223),
    isAccountPortAvailable: vi.fn((_port: number, _excludingAccountId?: string) => true),
    suggestAccountPort: vi.fn((_excludingAccountId?: string) => 9223),
    previewClientAccount: vi.fn(),
    confirmAddClientAccount: vi.fn(),
    reconnectSavedAccount: vi.fn(),
    switchOnlineAccount: vi.fn(),
    loadAccounts: vi.fn().mockResolvedValue(undefined),
    invalidateClientAccountPreviews: vi.fn(),
  },
  discoveryState: {
    candidates: [] as unknown[],
    loading: false,
    error: null as string | null,
    selectedId: null as string | null,
    preview: null as unknown,
    previewLoading: false,
    previewError: null as string | null,
    usersByPort: {} as Record<number, unknown>,
  },
  refreshMock: vi.fn(),
  launchMock: vi.fn(),
}))

vi.mock('@/stores/auth', async () => {
  const { reactive } = await import('vue')
  return { useAuthStore: () => reactive(authMock) }
})

vi.mock('@/composables/desktopClientState', async () => {
  const { computed, reactive } = await import('vue')
  const state = reactive(discoveryState)
  return {
    useDesktopClientState: () => ({
      accountCandidates: computed(() => state.candidates),
      accountCandidatesLoading: computed(() => state.loading),
      accountCandidatesError: computed(() => state.error),
      selectedAccountCandidateId: computed(() => state.selectedId),
      selectedClientAccountPreview: computed(() => state.preview),
      accountPreviewLoading: computed(() => state.previewLoading),
      accountPreviewError: computed(() => state.previewError),
      scanAccountClients: vi.fn(async () => {
        state.loading = true
        await Promise.resolve()
        state.loading = false
        return state.candidates
      }),
      selectAccountClientCandidate: vi.fn(async (id: string) => {
        const candidate = (state.candidates as ClientAccountCandidate[]).find(item => item.id === id)
        if (!candidate || !candidate.ready) {
          state.previewError = 'Client is not ready'
          return null
        }
        state.selectedId = id
        state.previewLoading = true
        state.previewError = null
        try {
          const result = await authMock.previewClientAccount(candidate.port)
          state.preview = result
          if (!result) state.previewError = 'No verified account'
          return result
        } finally {
          state.previewLoading = false
        }
      }),
      cancelAccountClientSelection: vi.fn(() => {
        state.selectedId = null
        state.preview = null
        state.previewLoading = false
        state.previewError = null
      }),
      refresh: refreshMock,
    }),
  }
})

vi.mock('@/api/tauri', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/api/tauri')>()
  return { ...actual, launchDesktopClientCdp: launchMock }
})

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
      'aria-label': attrs['aria-label'] as string | undefined,
      onClick: (event: MouseEvent) => emit('click', event),
    }, slots.default?.())
  },
})

const InputStub = defineComponent({
  name: 'Input',
  inheritAttrs: false,
  props: { modelValue: { type: [Number, String], default: '' }, type: String, placeholder: String, disabled: Boolean },
  emits: ['update:modelValue'],
  setup(props, { attrs, emit }) {
    return () => h('input', {
      id: attrs.id as string | undefined,
      type: props.type,
      min: attrs.min as string | undefined,
      max: attrs.max as string | undefined,
      placeholder: props.placeholder,
      disabled: props.disabled,
      value: props.modelValue,
      onInput: (event: Event) => {
        const input = event.target as HTMLInputElement
        emit('update:modelValue', input.value === '' ? '' : Number(input.value))
      },
      onKeydown: attrs.onKeydown as ((event: KeyboardEvent) => void) | undefined,
    })
  },
})

const PassthroughStub = (name: string) => defineComponent({
  name,
  inheritAttrs: false,
  setup(_, { slots }) {
    return () => h('div', slots.default?.())
  },
})

const AlertDialogStub = defineComponent({
  name: 'AlertDialog',
  props: { open: Boolean },
  setup(props, { slots }) {
    return () => props.open ? h('div', { role: 'dialog' }, slots.default?.()) : null
  },
})

const AlertDialogActionStub = defineComponent({
  name: 'AlertDialogAction',
  emits: ['click'],
  setup(_, { slots, emit }) {
    return () => h('button', { onClick: (event: MouseEvent) => emit('click', event) }, slots.default?.())
  },
})

const stubs = {
  Button: ButtonStub,
  Input: InputStub,
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
      general: { loading: 'Loading...' },
      dialog: { cancel: 'Cancel' },
      settings: {
        cdp_connected: 'Ready',
        cdp_disconnected_short: 'Unavailable',
        cdp_launch: 'Start client',
        cdp_dialog_title_disconnected: 'Restart client?',
        cdp_dialog_desc_disconnected: 'Restart this client with CDP enabled?',
        cdp_dialog_confirm: 'Restart',
        title: 'Settings',
      },
      accounts: {
        active: 'Active',
        add_account: 'Add account',
        cdp_port_conflict: 'Port conflict',
        reconnect_account_named: 'Reconnect {account}',
      },
      auth: {
        cdp_choose_title: 'Choose a Discord client',
        cdp_choose_desc: 'Choose an installed client or scan again.',
        cdp_client_stable: 'Discord',
        cdp_client_ptb: 'Discord PTB',
        cdp_client_canary: 'Discord Canary',
        cdp_client_vesktop: 'Vesktop',
        cdp_login: 'Discord client',
        cdp_port_placeholder: '9223',
        cdp_port_label: 'CDP port',
        cdp_port_detect: 'Check port',
        cdp_port_already_assigned: 'Already assigned to another account',
        cdp_port_no_available_port: 'No free CDP port is available.',
        cdp_status_starting: 'Starting',
        rescan: 'Scan again',
        authenticated_as: 'Signed in as',
        client_picker_add_desc: 'Choose the client signed in to the account you want to add.',
        client_picker_reconnect_desc: 'Choose a client for {account}.',
        client_picker_candidates: 'Running Discord clients',
        client_picker_ready: 'Ready',
        client_picker_starting: 'Starting',
        client_picker_unavailable: 'Unavailable',
        client_picker_no_ready: 'No ready clients found.',
        client_picker_port_assigned: 'Assigned to {account}',
        client_picker_manual_client: 'Client on this port',
        client_picker_switch_account: 'Switch to {account}',
        client_picker_preview_failed: 'Could not verify this account.',
        client_picker_already_saved: '{account} is already saved.',
        client_picker_identity_changed: 'The client account changed. Check it again.',
        client_picker_identity_mismatch: 'This client is signed in as {actual}, not {expected}.',
        client_picker_port_conflict: 'Port {port} is assigned to {account}.',
        client_picker_port_change_warning: 'Use {client} for {account}? This changes the port from {oldPort} to {newPort}.',
        client_picker_unassigned_reconnect_warning: 'Assign {client} to {account} and reconnect? CDP port {newPort} will be used.',
        client_picker_reassign_reconnect: 'Assign and reconnect {account}',
        client_picker_saved_account_missing: 'This saved account is no longer available.',
        client_picker_start_or_restart: '{client} is not ready yet.',
        client_picker_connection_details: 'Connection details',
        client_picker_verified_id: 'Verified account ID: {id}',
        client_picker_add_account: 'Add {account}',
        progress: {
          checking_cdp: 'Checking CDP clients',
          validating_cdp_session: 'Verifying the selected account',
          failed: 'Action failed',
        },
      },
    },
  },
})

function createUser(id: string, username: string, globalName = username): DiscordUser {
  return {
    id,
    username,
    discriminator: '0',
    avatar: null,
    global_name: globalName,
  }
}

function createCandidate(
  port: number,
  variantId: string,
  overrides: Partial<ClientAccountCandidate> = {},
): ClientAccountCandidate {
  return {
    id: `${port}:discord.official:${variantId}:install-${variantId}`,
    port,
    providerId: 'discord.official',
    variantId,
    installationId: `install-${variantId}`,
    displayName: `Discord ${variantId}`,
    ready: true,
    endpointState: 'discordReady',
    ownership: null,
    ...overrides,
  }
}

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

function setCandidates(items: ClientAccountCandidate[]) {
  reactive(discoveryState).candidates = items
  reactive(discoveryState).loading = false
  reactive(discoveryState).error = null
}

function mountPicker(props: { mode?: 'add' | 'reconnect'; targetAccountId?: string } = {}) {
  return mount(CdpClientPicker, {
    props: { mode: 'add', ...props },
    global: { plugins: [i18n], stubs },
  })
}

function radioForClient(wrapper: VueWrapper, clientName: string) {
  return wrapper.findAll('[role="radio"]').find(row => row.text().includes(clientName))
}

function buttonByText(wrapper: VueWrapper, text: string) {
  return wrapper.findAll('button').find(button => button.text().includes(text))
}

describe('CdpClientPicker mounted client-first flow', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    localStorage.clear()
    authMock.user = null
    authMock.loading = false
    authMock.error = null
    authMock.accounts = []
    authMock.activeAccountId = null
    authMock.accountPorts = {}
    authMock.portForAccount.mockImplementation((id: string) => {
      if (Object.prototype.hasOwnProperty.call(authMock.accountPorts, id)) return authMock.accountPorts[id] ?? 0
      return 9223
    })
    authMock.isAccountPortAvailable.mockImplementation((port: number, excludingAccountId?: string) => (
      Number.isInteger(port) && port >= 1024 && port <= 65535
      && (authMock.accounts as Array<{ id: string }>).every(account => (
        account.id === excludingAccountId || authMock.portForAccount(account.id) !== port
      ))
    ))
    authMock.suggestAccountPort.mockReturnValue(9223)
    authMock.previewClientAccount.mockImplementation(async (port: number) => {
      const user = (discoveryState.usersByPort as Record<number, DiscordUser>)[port]
      return user ? { port, user } : null
    })
    authMock.confirmAddClientAccount.mockReset()
    authMock.reconnectSavedAccount.mockReset()
    authMock.switchOnlineAccount.mockReset()
    authMock.loadAccounts.mockResolvedValue(undefined)
    reactive(discoveryState).candidates = []
    reactive(discoveryState).loading = false
    reactive(discoveryState).error = null
    reactive(discoveryState).selectedId = null
    reactive(discoveryState).preview = null
    reactive(discoveryState).previewLoading = false
    reactive(discoveryState).previewError = null
    reactive(discoveryState).usersByPort = {}
    refreshMock.mockResolvedValue(null)
    launchMock.mockResolvedValue({} as never)
  })

  it('shows a no-ready state with rescan and launch choices', async () => {
    const wrapper = mountPicker()
    await flushPromises()

    expect(wrapper.text()).toContain('No ready clients found')
    expect(buttonByText(wrapper, 'Scan again')).toBeTruthy()
    expect(buttonByText(wrapper, 'Discord')).toBeTruthy()
    expect(authMock.previewClientAccount).not.toHaveBeenCalled()
    expect(authMock.confirmAddClientAccount).not.toHaveBeenCalled()

    wrapper.unmount()
  })

  it('starts a selected installed client through the existing CDP launch flow', async () => {
    launchMock.mockResolvedValue({ cdp_connected: true } as never)

    const wrapper = mountPicker()
    await flushPromises()
    await buttonByText(wrapper, 'Discord')!.trigger('click')
    await flushPromises()

    expect(launchMock).toHaveBeenCalledWith(9223, {
      kind: 'provider',
      providerId: 'discord.official',
      variantId: 'stable',
    }, false)
    expect(authMock.confirmAddClientAccount).not.toHaveBeenCalled()
    expect(wrapper.emitted('complete')).toBeUndefined()

    wrapper.unmount()
  })

  it('offers the existing restart confirmation for a running but unready client', async () => {
    setCandidates([createCandidate(9223, 'stable', { ready: false, endpointState: 'unreachable' })])
    const snapshot = readySnapshot(9223)
    snapshot.endpoint.status = 'unreachable'
    snapshot.processes = [{
      providerId: 'discord.official',
      installationId: 'install-stable',
      variantId: 'stable',
      executablePath: null,
      running: true,
    }]
    refreshMock.mockResolvedValue(snapshot)
    launchMock.mockResolvedValue({ cdp_connected: true } as never)

    const wrapper = mountPicker()
    await flushPromises()
    await radioForClient(wrapper, 'Discord Stable')!.trigger('click')
    await flushPromises()
    await buttonByText(wrapper, 'Start client')!.trigger('click')
    await flushPromises()

    expect(wrapper.text()).toContain('Restart client?')
    await buttonByText(wrapper, 'Restart')!.trigger('click')
    await flushPromises()

    expect(launchMock).toHaveBeenCalledWith(9223, {
      kind: 'installation',
      installationId: 'install-stable',
    }, true)
    expect(wrapper.emitted('complete')).toBeUndefined()

    wrapper.unmount()
  })

  it('shows client names, selects a port, and previews only that selected client', async () => {
    setCandidates([createCandidate(9223, 'stable'), createCandidate(9224, 'ptb')])
    const user = createUser('B', 'bob', 'Bob')
    reactive(discoveryState).usersByPort = { 9223: createUser('A', 'alice', 'Alice'), 9224: user }

    const wrapper = mountPicker()
    await flushPromises()

    expect(wrapper.text()).toContain('Discord Stable')
    expect(wrapper.text()).toContain('Discord PTB')
    const details = wrapper.find('details')
    expect((details.element as HTMLDetailsElement).open).toBe(false)

    await radioForClient(wrapper, 'Discord PTB')!.trigger('click')
    await flushPromises()

    expect(authMock.previewClientAccount).toHaveBeenCalledTimes(1)
    expect(authMock.previewClientAccount).toHaveBeenCalledWith(9224)
    expect(authMock.confirmAddClientAccount).not.toHaveBeenCalled()
    expect(authMock.reconnectSavedAccount).not.toHaveBeenCalled()
    expect(wrapper.text()).toContain('Bob')

    await details.find('summary').trigger('click')
    await flushPromises()
    expect((details.element as HTMLDetailsElement).open).toBe(true)
    expect(wrapper.text()).toContain('CDP port: 9224')
    expect(wrapper.find('#client-picker-manual-port').exists()).toBe(true)

    wrapper.unmount()
  })

  it('adds a new verified account and closes only after the added result', async () => {
    const user = createUser('N', 'nina', 'Nina')
    setCandidates([createCandidate(9223, 'stable')])
    reactive(discoveryState).usersByPort = { 9223: user }
    authMock.confirmAddClientAccount.mockResolvedValue({ status: 'added', user, port: 9223 })

    const wrapper = mountPicker()
    await flushPromises()
    await radioForClient(wrapper, 'Discord Stable')!.trigger('click')
    await flushPromises()
    await buttonByText(wrapper, 'Add Nina')!.trigger('click')
    await flushPromises()

    expect(authMock.confirmAddClientAccount).toHaveBeenCalledWith(9223, 'N', expect.any(Function))
    expect(wrapper.emitted('complete')).toHaveLength(1)
    expect(authMock.reconnectSavedAccount).not.toHaveBeenCalled()

    wrapper.unmount()
  })

  it('keeps the picker open while a confirmed Add is still publishing', async () => {
    const user = createUser('N', 'nina', 'Nina')
    setCandidates([createCandidate(9223, 'stable')])
    reactive(discoveryState).usersByPort = { 9223: user }
    let resolveConfirm!: (result: { status: 'added'; user: DiscordUser; port: number }) => void
    authMock.confirmAddClientAccount.mockReturnValue(new Promise(resolve => {
      resolveConfirm = resolve
    }))

    const wrapper = mountPicker()
    await flushPromises()
    await radioForClient(wrapper, 'Discord Stable')!.trigger('click')
    await flushPromises()
    await buttonByText(wrapper, 'Add Nina')!.trigger('click')
    await flushPromises()

    expect(wrapper.emitted('mutationBusy')?.slice(-1)[0]?.[0]).toBe(true)
    const cancel = buttonByText(wrapper, 'Cancel')!
    expect(cancel.attributes('disabled')).toBeDefined()
    await cancel.trigger('click')
    expect(wrapper.emitted('cancel')).toBeUndefined()
    expect(wrapper.emitted('complete')).toBeUndefined()

    resolveConfirm({ status: 'added', user, port: 9223 })
    await flushPromises()
    expect(wrapper.emitted('mutationBusy')?.slice(-1)[0]?.[0]).toBe(false)
    expect(wrapper.emitted('complete')).toHaveLength(1)
    wrapper.unmount()
  })

  it('keeps the picker open when Add discovers the account is already saved', async () => {
    const user = createUser('N', 'nina', 'Nina')
    setCandidates([createCandidate(9223, 'stable')])
    reactive(discoveryState).usersByPort = { 9223: user }
    authMock.confirmAddClientAccount.mockResolvedValue({ status: 'alreadySaved', user, port: 9223 })
    let loadCount = 0
    authMock.loadAccounts.mockImplementation(async () => {
      loadCount += 1
      if (loadCount > 1) {
        const store = reactive(authMock)
        store.accounts = [{ id: 'N', username: 'nina', globalName: 'Nina', isAuthenticated: false }]
        store.accountPorts = { N: null }
      }
    })

    const wrapper = mountPicker()
    await flushPromises()
    await radioForClient(wrapper, 'Discord Stable')!.trigger('click')
    await flushPromises()
    await buttonByText(wrapper, 'Add Nina')!.trigger('click')
    await flushPromises()

    expect(wrapper.emitted('complete')).toBeUndefined()
    expect(wrapper.text()).toContain('Nina is already saved')
    expect(buttonByText(wrapper, 'Assign and reconnect Nina')).toBeTruthy()

    wrapper.unmount()
  })

  it('refuses to reconnect B from a client verified as A without calling a mutation', async () => {
    authMock.accounts = [{ id: 'B', username: 'bob', globalName: 'Bob', isAuthenticated: false }]
    authMock.accountPorts = { B: null }
    setCandidates([createCandidate(9223, 'stable')])
    reactive(discoveryState).usersByPort = { 9223: createUser('A', 'alice', 'Alice') }

    const wrapper = mountPicker({ mode: 'reconnect', targetAccountId: 'B' })
    await flushPromises()
    await radioForClient(wrapper, 'Discord Stable')!.trigger('click')
    await flushPromises()

    expect(wrapper.text()).toContain('Alice')
    expect(wrapper.text()).toContain('not Bob')
    expect(buttonByText(wrapper, 'Reconnect Bob')).toBeUndefined()
    expect(authMock.reconnectSavedAccount).not.toHaveBeenCalled()
    expect(authMock.confirmAddClientAccount).not.toHaveBeenCalled()
    expect(authMock.switchOnlineAccount).not.toHaveBeenCalled()
    expect(wrapper.emitted('complete')).toBeUndefined()

    wrapper.unmount()
  })

  it('reconnects the verified saved account B without activating another account first', async () => {
    const user = createUser('B', 'bob', 'Bob')
    authMock.accounts = [{ id: 'B', username: 'bob', globalName: 'Bob', isAuthenticated: false }]
    authMock.accountPorts = { B: null }
    setCandidates([createCandidate(9224, 'ptb')])
    reactive(discoveryState).usersByPort = { 9224: user }
    authMock.reconnectSavedAccount.mockImplementation(async (accountId: string, port: number) => {
      const store = reactive(authMock)
      store.activeAccountId = accountId
      store.user = user
      return { status: 'reconnected', user, port }
    })

    const wrapper = mountPicker({ mode: 'reconnect', targetAccountId: 'B' })
    await flushPromises()
    await radioForClient(wrapper, 'Discord PTB')!.trigger('click')
    await flushPromises()
    expect(wrapper.text()).toContain('Assign Discord PTB to Bob and reconnect?')
    await buttonByText(wrapper, 'Assign and reconnect Bob')!.trigger('click')
    await flushPromises()

    expect(authMock.reconnectSavedAccount).toHaveBeenCalledWith('B', 9224, expect.any(Function))
    expect(authMock.activeAccountId).toBe('B')
    expect(authMock.confirmAddClientAccount).not.toHaveBeenCalled()
    expect(wrapper.emitted('complete')).toHaveLength(1)

    wrapper.unmount()
  })

  it('invalidates preview and remains open when identity changes during Add', async () => {
    const user = createUser('N', 'nina', 'Nina')
    setCandidates([createCandidate(9223, 'stable')])
    reactive(discoveryState).usersByPort = { 9223: user }
    authMock.confirmAddClientAccount.mockResolvedValue({ status: 'identityChanged', user, port: 9223 })

    const wrapper = mountPicker()
    await flushPromises()
    await radioForClient(wrapper, 'Discord Stable')!.trigger('click')
    await flushPromises()
    await buttonByText(wrapper, 'Add Nina')!.trigger('click')
    await flushPromises()

    expect(wrapper.emitted('complete')).toBeUndefined()
    expect(wrapper.text()).toContain('The client account changed')
    expect(buttonByText(wrapper, 'Scan again')).toBeTruthy()
    expect(wrapper.find('section[aria-labelledby="client-picker-title"]').exists()).toBe(true)

    wrapper.unmount()
  })

  it('switches a verified online account without Add or reconnect capture', async () => {
    const user = createUser('B', 'bob', 'Bob')
    authMock.accounts = [{ id: 'B', username: 'bob', globalName: 'Bob', isAuthenticated: true }]
    authMock.accountPorts = { B: 9224 }
    setCandidates([createCandidate(9224, 'ptb')])
    reactive(discoveryState).usersByPort = { 9224: user }
    authMock.switchOnlineAccount.mockImplementation(async (accountId: string) => {
      const store = reactive(authMock)
      store.activeAccountId = accountId
      store.user = user
      return { id: accountId }
    })

    const wrapper = mountPicker()
    await flushPromises()
    await radioForClient(wrapper, 'Discord PTB')!.trigger('click')
    await flushPromises()
    await buttonByText(wrapper, 'Switch to Bob')!.trigger('click')
    await flushPromises()

    expect(authMock.switchOnlineAccount).toHaveBeenCalledWith('B')
    expect(authMock.confirmAddClientAccount).not.toHaveBeenCalled()
    expect(authMock.reconnectSavedAccount).not.toHaveBeenCalled()
    expect(wrapper.emitted('complete')).toHaveLength(1)

    wrapper.unmount()
  })

  it('keeps manual port entry collapsed under Connection details', async () => {
    setCandidates([createCandidate(9223, 'stable')])
    reactive(discoveryState).usersByPort = { 9223: createUser('A', 'alice', 'Alice') }

    const wrapper = mountPicker()
    await flushPromises()
    const details = wrapper.find('details')
    expect((details.element as HTMLDetailsElement).open).toBe(false)

    await details.find('summary').trigger('click')
    await flushPromises()
    expect((details.element as HTMLDetailsElement).open).toBe(true)
    expect(wrapper.find('#client-picker-manual-port').exists()).toBe(true)

    wrapper.unmount()
  })

  it('checks a manual port only from the collapsed Connection details section', async () => {
    const user = createUser('M', 'manual', 'Manual User')
    reactive(discoveryState).usersByPort = { 9226: user }
    refreshMock.mockResolvedValue(readySnapshot(9226))

    const wrapper = mountPicker()
    await flushPromises()
    const details = wrapper.find('details')
    expect((details.element as HTMLDetailsElement).open).toBe(false)

    await details.find('summary').trigger('click')
    await flushPromises()
    await wrapper.find('#client-picker-manual-port').setValue('9226')
    await buttonByText(wrapper, 'Check port')!.trigger('click')
    await flushPromises()

    expect(refreshMock).toHaveBeenCalledWith(9226)
    expect(authMock.previewClientAccount).toHaveBeenCalledWith(9226)
    expect(wrapper.text()).toContain('Manual User')
    expect(authMock.confirmAddClientAccount).not.toHaveBeenCalled()

    wrapper.unmount()
  })

  it('moves keyboard selection through the client radio group', async () => {
    setCandidates([createCandidate(9223, 'stable'), createCandidate(9224, 'ptb')])
    reactive(discoveryState).usersByPort = {
      9223: createUser('A', 'alice', 'Alice'),
      9224: createUser('B', 'bob', 'Bob'),
    }

    const wrapper = mountPicker()
    await flushPromises()
    const stable = radioForClient(wrapper, 'Discord Stable')!
    const ptb = radioForClient(wrapper, 'Discord PTB')!
    expect(stable.attributes('tabindex')).toBe('0')

    await stable.trigger('keydown', { key: 'ArrowDown' })
    await flushPromises()

    expect(ptb.attributes('aria-checked')).toBe('true')
    expect(authMock.previewClientAccount).toHaveBeenCalledWith(9224)

    wrapper.unmount()
  })
})
