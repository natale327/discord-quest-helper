// @vitest-environment happy-dom
import { describe, it, expect, vi, beforeEach } from 'vitest'
import { mount, flushPromises, type VueWrapper } from '@vue/test-utils'
import { defineComponent, h } from 'vue'
import { createI18n } from 'vue-i18n'
import AccountProxyPanel from './AccountProxyPanel.vue'
import type { AccountProxySettingsDto } from '@/api/tauri'

// Mock the API module.
vi.mock('@/api/tauri', () => ({
  getAccountProxySettings: vi.fn(),
  setAccountProxyOverride: vi.fn(),
  clearAccountProxyOverride: vi.fn()
}))

import {
  getAccountProxySettings,
  setAccountProxyOverride,
  clearAccountProxyOverride
} from '@/api/tauri'

const mockedGetAccountProxySettings = vi.mocked(getAccountProxySettings)
const mockedSetAccountProxyOverride = vi.mocked(setAccountProxyOverride)
const mockedClearAccountProxyOverride = vi.mocked(clearAccountProxyOverride)

/** A fully-typed DTO; every field is present unless overridden. */
function createDto(overrides: Partial<AccountProxySettingsDto> = {}): AccountProxySettingsDto {
  return {
    accountId: 'account-1',
    hasOverride: false,
    overrideMode: null,
    overrideEndpoint: null,
    overrideNoProxy: null,
    overrideHasCredentials: false,
    effectiveMode: 'system',
    effectiveEndpoint: null,
    effectiveNoProxy: null,
    effectiveHasCredentials: false,
    ...overrides
  }
}

/** A DTO for a saved custom override. */
function customDto(overrides: Partial<AccountProxySettingsDto> = {}): AccountProxySettingsDto {
  return createDto({
    hasOverride: true,
    overrideMode: 'custom',
    overrideEndpoint: 'http://proxy:8080',
    overrideNoProxy: '',
    overrideHasCredentials: false,
    effectiveMode: 'custom',
    effectiveEndpoint: 'http://proxy:8080',
    effectiveNoProxy: '',
    effectiveHasCredentials: false,
    ...overrides
  })
}

function createDeferred<T>() {
  let resolve!: (value: T) => void
  let reject!: (reason?: unknown) => void
  const promise = new Promise<T>((res, rej) => {
    resolve = res
    reject = rej
  })
  return { promise, resolve, reject }
}

// Render-function stubs (no runtime template compiler needed). Each carries a
// `name` so `findComponent({ name })` works, and forwards `disabled` properly.
const ButtonStub = defineComponent({
  name: 'Button',
  props: { disabled: Boolean, variant: String, size: String },
  emits: ['click'],
  setup(props, { slots, emit }) {
    return () =>
      h(
        'button',
        {
          disabled: props.disabled,
          onClick: (event: MouseEvent) => emit('click', event)
        },
        slots.default?.()
      )
  }
})

const InputStub = defineComponent({
  name: 'Input',
  inheritAttrs: false,
  props: {
    modelValue: { type: [String, Number], default: '' },
    type: String,
    placeholder: String,
    disabled: Boolean
  },
  emits: ['update:modelValue', 'input'],
  setup(props, { attrs, emit }) {
    return () =>
      h('input', {
        id: attrs.id as string | undefined,
        type: props.type,
        placeholder: props.placeholder,
        disabled: props.disabled,
        value: props.modelValue,
        onInput: (event: Event) => {
          const value = (event.target as HTMLInputElement).value
          emit('update:modelValue', value)
          // Also forward the native event so `@input` handlers run.
          emit('input', event)
        }
      })
  }
})

const LabelStub = defineComponent({
  name: 'Label',
  props: { for: String },
  setup(props, { slots }) {
    return () => h('label', { for: props.for }, slots.default?.())
  }
})

const BadgeStub = defineComponent({
  name: 'Badge',
  props: { variant: String },
  setup(_, { slots }) {
    return () => h('span', { class: 'badge' }, slots.default?.())
  }
})

const i18n = createI18n({
  legacy: false,
  locale: 'en',
  messages: {
    en: {
      accounts: {
        proxy_status: 'Status',
        proxy_custom: 'Custom',
        proxy_inherit: 'Inherit global',
        proxy_effective_mode: 'Effective mode: {mode}',
        proxy_has_credentials: 'Credentials saved',
        proxy_mode_label: 'Mode',
        proxy_mode_inherit: 'Inherit global',
        proxy_endpoint_required: 'Endpoint is required',
        proxy_credentials_both_required: 'Both username and password are required',
        proxy_clear_override: 'Clear override'
      },
      settings: {
        proxy_mode_system: 'System',
        proxy_mode_direct: 'Direct',
        proxy_mode_custom: 'Custom',
        proxy_endpoint: 'Endpoint',
        proxy_endpoint_hint: 'Proxy endpoint hint',
        proxy_no_proxy: 'No proxy',
        proxy_no_proxy_hint: 'No proxy hint',
        proxy_credentials: 'Credentials',
        proxy_credentials_saved: 'Credentials saved',
        proxy_credentials_desc: 'Leave empty to use existing credentials',
        proxy_username: 'Username',
        proxy_password: 'Password',
        proxy_save: 'Save',
        proxy_saving: 'Saving...'
      },
      general: {
        retry: 'Retry'
      }
    }
  }
})

function mountPanel(props: Record<string, unknown> = {}) {
  return mount(AccountProxyPanel, {
    props: {
      accountId: 'account-1',
      accountName: 'Test Account',
      ...props
    },
    global: {
      plugins: [i18n],
      stubs: {
        Button: ButtonStub,
        Input: InputStub,
        Label: LabelStub,
        Badge: BadgeStub
      }
    }
  })
}

type Wrapper = VueWrapper

function buttons(wrapper: Wrapper) {
  return wrapper.findAllComponents({ name: 'Button' })
}

function buttonByText(wrapper: Wrapper, text: string) {
  return buttons(wrapper).find(button => button.text().includes(text))
}

function saveButton(wrapper: Wrapper) {
  return buttonByText(wrapper, 'Save')
}

function clearButton(wrapper: Wrapper) {
  return buttonByText(wrapper, 'Clear')
}

async function selectMode(wrapper: Wrapper, text: string) {
  const modeButton = wrapper.findAll('button').find(button => button.text() === text)
  expect(modeButton, `mode button ${text}`).toBeTruthy()
  await modeButton!.trigger('click')
}

function isDisabled(wrapper: Wrapper | undefined): boolean {
  return wrapper?.attributes('disabled') !== undefined
}

describe('AccountProxyPanel', () => {
  beforeEach(() => {
    vi.clearAllMocks()
  })

  describe('Basic rendering', () => {
    it('shows loading state initially', async () => {
      const deferred = createDeferred<AccountProxySettingsDto>()
      mockedGetAccountProxySettings.mockReturnValue(deferred.promise)

      const wrapper = mountPanel()
      await flushPromises()

      expect(wrapper.find('.animate-spin').exists()).toBe(true)

      deferred.resolve(createDto())
      await flushPromises()

      expect(wrapper.find('.animate-spin').exists()).toBe(false)
    })

    it('renders inherited state correctly', async () => {
      mockedGetAccountProxySettings.mockResolvedValue(createDto())

      const wrapper = mountPanel()
      await flushPromises()

      const badges = wrapper.findAllComponents({ name: 'Badge' })
      expect(badges.some(badge => badge.text().includes('Inherit global'))).toBe(true)
      expect(wrapper.text()).toContain('Effective mode: System')
      expect(wrapper.find('#endpoint').exists()).toBe(false)
    })

    it('renders custom override state correctly', async () => {
      mockedGetAccountProxySettings.mockResolvedValue(
        customDto({ overrideHasCredentials: true, effectiveHasCredentials: true })
      )

      const wrapper = mountPanel()
      await flushPromises()

      const badges = wrapper.findAllComponents({ name: 'Badge' })
      expect(badges.some(badge => badge.text().includes('Custom'))).toBe(true)
      expect(wrapper.text()).toContain('Credentials saved')
      expect(clearButton(wrapper)).toBeTruthy()
    })

    it('shows error state when load fails', async () => {
      mockedGetAccountProxySettings.mockRejectedValue(new Error('Network error'))

      const wrapper = mountPanel()
      await flushPromises()

      expect(wrapper.text()).toContain('Network error')
      expect(buttonByText(wrapper, 'Retry')).toBeTruthy()
    })
  })

  describe('Mode switching', () => {
    it('shows custom fields when custom mode is selected', async () => {
      mockedGetAccountProxySettings.mockResolvedValue(createDto())

      const wrapper = mountPanel()
      await flushPromises()
      expect(wrapper.find('#endpoint').exists()).toBe(false)

      await selectMode(wrapper, 'Custom')

      expect(wrapper.find('#endpoint').exists()).toBe(true)
      expect(wrapper.find('#noProxy').exists()).toBe(true)
      expect(wrapper.find('#username').exists()).toBe(true)
      expect(wrapper.find('#password').exists()).toBe(true)
    })

    it('hides custom fields when switching to inherit mode', async () => {
      mockedGetAccountProxySettings.mockResolvedValue(customDto())

      const wrapper = mountPanel()
      await flushPromises()
      expect(wrapper.find('#endpoint').exists()).toBe(true)

      await selectMode(wrapper, 'Inherit global')

      expect(wrapper.find('#endpoint').exists()).toBe(false)
    })
  })

  describe('Validation', () => {
    it('prevents save when custom mode has no endpoint', async () => {
      mockedGetAccountProxySettings.mockResolvedValue(createDto())

      const wrapper = mountPanel()
      await flushPromises()
      await selectMode(wrapper, 'Custom')

      // With no endpoint the Save button is disabled (a no-op can never be sent).
      expect(isDisabled(saveButton(wrapper))).toBe(true)
      await saveButton(wrapper)!.trigger('click')
      await flushPromises()

      expect(mockedSetAccountProxyOverride).not.toHaveBeenCalled()
    })

    it('requires both username and password when either is provided', async () => {
      mockedGetAccountProxySettings.mockResolvedValue(createDto())

      const wrapper = mountPanel()
      await flushPromises()
      await selectMode(wrapper, 'Custom')
      await wrapper.find('#endpoint').setValue('http://proxy:8080')
      await wrapper.find('#username').setValue('user')

      await saveButton(wrapper)!.trigger('click')
      await flushPromises()

      expect(mockedSetAccountProxyOverride).not.toHaveBeenCalled()
      expect(wrapper.text()).toContain('Both username and password are required')
    })

    it('allows save with both username and password', async () => {
      mockedGetAccountProxySettings.mockResolvedValue(createDto())
      mockedSetAccountProxyOverride.mockResolvedValue(
        customDto({ overrideHasCredentials: true, effectiveHasCredentials: true })
      )

      const wrapper = mountPanel()
      await flushPromises()
      await selectMode(wrapper, 'Custom')
      await wrapper.find('#endpoint').setValue('http://proxy:8080')
      await wrapper.find('#username').setValue('user')
      await wrapper.find('#password').setValue('pass')

      await saveButton(wrapper)!.trigger('click')
      await flushPromises()

      expect(mockedSetAccountProxyOverride).toHaveBeenCalledWith('account-1', {
        mode: 'custom',
        endpoint: 'http://proxy:8080',
        username: 'user',
        password: 'pass'
      })
    })
  })

  describe('Save operations', () => {
    it('saves custom proxy settings', async () => {
      mockedGetAccountProxySettings.mockResolvedValue(createDto())
      mockedSetAccountProxyOverride.mockResolvedValue(customDto({ overrideNoProxy: 'localhost' }))

      const wrapper = mountPanel()
      await flushPromises()
      await selectMode(wrapper, 'Custom')
      await wrapper.find('#endpoint').setValue('http://proxy:8080')
      await wrapper.find('#noProxy').setValue('localhost')

      await saveButton(wrapper)!.trigger('click')
      await flushPromises()

      expect(mockedSetAccountProxyOverride).toHaveBeenCalledWith('account-1', {
        mode: 'custom',
        endpoint: 'http://proxy:8080',
        noProxy: 'localhost'
      })
    })

    it('clears override when saving in inherit mode', async () => {
      mockedGetAccountProxySettings.mockResolvedValue(customDto())
      mockedClearAccountProxyOverride.mockResolvedValue(createDto())

      const wrapper = mountPanel()
      await flushPromises()
      await selectMode(wrapper, 'Inherit global')

      await saveButton(wrapper)!.trigger('click')
      await flushPromises()

      expect(mockedClearAccountProxyOverride).toHaveBeenCalledWith('account-1')
    })

    it('shows error when save fails', async () => {
      mockedGetAccountProxySettings.mockResolvedValue(createDto())
      mockedSetAccountProxyOverride.mockRejectedValue(new Error('Save failed'))

      const wrapper = mountPanel()
      await flushPromises()
      await selectMode(wrapper, 'Custom')
      await wrapper.find('#endpoint').setValue('http://proxy:8080')

      await saveButton(wrapper)!.trigger('click')
      await flushPromises()

      expect(wrapper.text()).toContain('Save failed')
    })
  })

  describe('Clear operations', () => {
    it('clears override', async () => {
      mockedGetAccountProxySettings.mockResolvedValue(customDto())
      mockedClearAccountProxyOverride.mockResolvedValue(createDto())

      const wrapper = mountPanel()
      await flushPromises()

      await clearButton(wrapper)!.trigger('click')
      await flushPromises()

      expect(mockedClearAccountProxyOverride).toHaveBeenCalledWith('account-1')
    })

    it('shows error when clear fails', async () => {
      mockedGetAccountProxySettings.mockResolvedValue(customDto())
      mockedClearAccountProxyOverride.mockRejectedValue(new Error('Clear failed'))

      const wrapper = mountPanel()
      await flushPromises()

      await clearButton(wrapper)!.trigger('click')
      await flushPromises()

      expect(wrapper.text()).toContain('Clear failed')
    })
  })

  describe('Busy state', () => {
    it('disables buttons during save and re-enables after', async () => {
      const deferred = createDeferred<AccountProxySettingsDto>()
      mockedGetAccountProxySettings.mockResolvedValue(createDto())
      mockedSetAccountProxyOverride.mockReturnValue(deferred.promise)

      const wrapper = mountPanel()
      await flushPromises()
      await selectMode(wrapper, 'Custom')
      await wrapper.find('#endpoint').setValue('http://proxy:8080')

      await saveButton(wrapper)!.trigger('click')
      await flushPromises()
      expect(isDisabled(saveButton(wrapper))).toBe(true)

      deferred.resolve(customDto())
      await flushPromises()
      expect(isDisabled(saveButton(wrapper))).toBe(false)
    })

    it('makes Save and Clear mutually exclusive while one is pending', async () => {
      const saveDeferred = createDeferred<AccountProxySettingsDto>()
      mockedGetAccountProxySettings.mockResolvedValue(customDto())
      mockedSetAccountProxyOverride.mockReturnValue(saveDeferred.promise)

      const wrapper = mountPanel()
      await flushPromises()

      await saveButton(wrapper)!.trigger('click')
      await flushPromises()
      expect(isDisabled(clearButton(wrapper))).toBe(true)

      saveDeferred.resolve(customDto())
      await flushPromises()
      expect(isDisabled(clearButton(wrapper))).toBe(false)
    })

    it('disables Save while a Clear is pending', async () => {
      const clearDeferred = createDeferred<AccountProxySettingsDto>()
      mockedGetAccountProxySettings.mockResolvedValue(customDto())
      mockedClearAccountProxyOverride.mockReturnValue(clearDeferred.promise)

      const wrapper = mountPanel()
      await flushPromises()

      await clearButton(wrapper)!.trigger('click')
      await flushPromises()
      expect(isDisabled(saveButton(wrapper))).toBe(true)

      clearDeferred.resolve(createDto())
      await flushPromises()

      // The busy state is cleared (no spinner) and the panel is interactive:
      // after the override is gone a fresh Custom selection with an endpoint is
      // savable, proving `mutating` is not stuck.
      expect(wrapper.findAll('.animate-spin')).toHaveLength(0)
      await selectMode(wrapper, 'Custom')
      await wrapper.find('#endpoint').setValue('http://proxy:8080')
      expect(isDisabled(saveButton(wrapper))).toBe(false)
    })
  })

  describe('Credential handling', () => {
    it('does not send credentials when not touched', async () => {
      mockedGetAccountProxySettings.mockResolvedValue(
        customDto({ overrideHasCredentials: true, effectiveHasCredentials: true })
      )
      mockedSetAccountProxyOverride.mockResolvedValue(
        customDto({ overrideHasCredentials: true, effectiveHasCredentials: true })
      )

      const wrapper = mountPanel()
      await flushPromises()

      await saveButton(wrapper)!.trigger('click')
      await flushPromises()

      expect(mockedSetAccountProxyOverride).toHaveBeenCalledWith('account-1', {
        mode: 'custom',
        endpoint: 'http://proxy:8080'
      })
    })

    it('shows credentials saved indicator', async () => {
      mockedGetAccountProxySettings.mockResolvedValue(
        customDto({ overrideHasCredentials: true, effectiveHasCredentials: true })
      )

      const wrapper = mountPanel()
      await flushPromises()

      expect(wrapper.text()).toContain('Credentials saved')
    })

    it('never renders credential values', async () => {
      mockedGetAccountProxySettings.mockResolvedValue(
        customDto({ overrideHasCredentials: true, effectiveHasCredentials: true })
      )

      const wrapper = mountPanel()
      await flushPromises()

      expect(wrapper.text()).not.toContain('password123')
      expect(wrapper.text()).not.toContain('user123')
    })
  })

  describe('Endpoint hint', () => {
    it('shows hint when custom mode has empty endpoint', async () => {
      mockedGetAccountProxySettings.mockResolvedValue(createDto())

      const wrapper = mountPanel()
      await flushPromises()
      await selectMode(wrapper, 'Custom')

      expect(wrapper.text()).toContain('Endpoint is required')
    })

    it('hides hint once endpoint is typed', async () => {
      mockedGetAccountProxySettings.mockResolvedValue(createDto())

      const wrapper = mountPanel()
      await flushPromises()
      await selectMode(wrapper, 'Custom')

      expect(wrapper.text()).toContain('Endpoint is required')

      await wrapper.find('#endpoint').setValue('http://proxy:8080')

      expect(wrapper.text()).not.toContain('Endpoint is required')
    })

    it('does not show hint in inherit mode', async () => {
      mockedGetAccountProxySettings.mockResolvedValue(createDto())

      const wrapper = mountPanel()
      await flushPromises()

      expect(wrapper.text()).not.toContain('Endpoint is required')
    })

    it('does not show hint in system mode', async () => {
      mockedGetAccountProxySettings.mockResolvedValue(createDto())

      const wrapper = mountPanel()
      await flushPromises()
      await selectMode(wrapper, 'System')

      expect(wrapper.text()).not.toContain('Endpoint is required')
    })

    it('does not show hint in direct mode', async () => {
      mockedGetAccountProxySettings.mockResolvedValue(createDto())

      const wrapper = mountPanel()
      await flushPromises()
      await selectMode(wrapper, 'Direct')

      expect(wrapper.text()).not.toContain('Endpoint is required')
    })

    it('keeps Save disabled when hint is shown', async () => {
      mockedGetAccountProxySettings.mockResolvedValue(createDto())

      const wrapper = mountPanel()
      await flushPromises()
      await selectMode(wrapper, 'Custom')

      expect(wrapper.text()).toContain('Endpoint is required')
      expect(isDisabled(saveButton(wrapper))).toBe(true)
    })

    it('does not call IPC when Save is clicked with hint visible', async () => {
      mockedGetAccountProxySettings.mockResolvedValue(createDto())

      const wrapper = mountPanel()
      await flushPromises()
      await selectMode(wrapper, 'Custom')

      expect(wrapper.text()).toContain('Endpoint is required')

      await saveButton(wrapper)!.trigger('click')
      await flushPromises()

      expect(mockedSetAccountProxyOverride).not.toHaveBeenCalled()
    })
  })

  describe('Account switching and races', () => {
    it('clears the form when the account changes', async () => {
      const deferredA = createDeferred<AccountProxySettingsDto>()
      const deferredB = createDeferred<AccountProxySettingsDto>()
      mockedGetAccountProxySettings
        .mockReturnValueOnce(deferredA.promise)
        .mockReturnValueOnce(deferredB.promise)

      const wrapper = mountPanel({ accountId: 'account-1' })
      await flushPromises()

      deferredA.resolve(customDto())
      await flushPromises()
      expect(wrapper.find('#endpoint').exists()).toBe(true)

      await wrapper.setProps({ accountId: 'account-2' })
      await flushPromises()
      // The previous account's form is cleared immediately.
      expect(wrapper.find('#endpoint').exists()).toBe(false)
      expect(wrapper.find('.animate-spin').exists()).toBe(true)

      deferredB.resolve(createDto())
      await flushPromises()
      const badges = wrapper.findAllComponents({ name: 'Badge' })
      expect(badges.some(badge => badge.text().includes('Inherit global'))).toBe(true)
    })

    it('ignores stale load responses from the previous account', async () => {
      const deferredA = createDeferred<AccountProxySettingsDto>()
      const deferredB = createDeferred<AccountProxySettingsDto>()
      mockedGetAccountProxySettings
        .mockReturnValueOnce(deferredA.promise)
        .mockReturnValueOnce(deferredB.promise)

      const wrapper = mountPanel({ accountId: 'account-1' })
      await flushPromises()

      await wrapper.setProps({ accountId: 'account-2' })
      await flushPromises()

      deferredB.resolve(createDto())
      await flushPromises()
      expect(wrapper.text()).toContain('Effective mode: System')

      // Stale A response must not replace B.
      deferredA.resolve(customDto({ overrideEndpoint: 'http://proxy1:8080' }))
      await flushPromises()
      const badges = wrapper.findAllComponents({ name: 'Badge' })
      expect(badges.some(badge => badge.text().includes('Inherit global'))).toBe(true)
      expect(wrapper.text()).not.toContain('Effective mode: Custom')
    })

    it('a pending save for A cannot leave B busy when it settles', async () => {
      const saveDeferred = createDeferred<AccountProxySettingsDto>()
      mockedGetAccountProxySettings
        .mockResolvedValueOnce(createDto()) // A inherits
        .mockResolvedValueOnce(customDto()) // B has an override
      mockedSetAccountProxyOverride.mockReturnValue(saveDeferred.promise)

      const wrapper = mountPanel({ accountId: 'account-1' })
      await flushPromises()
      await selectMode(wrapper, 'Custom')
      await wrapper.find('#endpoint').setValue('http://proxy-a:8080')
      await saveButton(wrapper)!.trigger('click')
      await flushPromises()
      expect(isDisabled(saveButton(wrapper))).toBe(true)

      // Switch to B while A's save is still pending.
      await wrapper.setProps({ accountId: 'account-2' })
      await flushPromises()
      expect(wrapper.find('#endpoint').attributes('value')).toBe('http://proxy:8080')

      // A's stale save settles; B must remain usable (not stuck disabled).
      saveDeferred.resolve(customDto({ accountId: 'account-1' }))
      await flushPromises()

      expect(isDisabled(saveButton(wrapper))).toBe(false)
      expect(wrapper.text()).toContain('Effective mode: Custom')
      expect(wrapper.text()).not.toContain('proxy-a:8080')
    })

    it('a pending clear for A cannot leave B busy when it settles', async () => {
      const clearDeferred = createDeferred<AccountProxySettingsDto>()
      mockedGetAccountProxySettings
        .mockResolvedValueOnce(customDto()) // A has an override
        .mockResolvedValueOnce(customDto()) // B has an override
      mockedClearAccountProxyOverride.mockReturnValue(clearDeferred.promise)

      const wrapper = mountPanel({ accountId: 'account-1' })
      await flushPromises()
      await clearButton(wrapper)!.trigger('click')
      await flushPromises()
      expect(isDisabled(clearButton(wrapper))).toBe(true)

      await wrapper.setProps({ accountId: 'account-2' })
      await flushPromises()

      clearDeferred.resolve(createDto({ accountId: 'account-1' }))
      await flushPromises()

      // B is not busy and still shows its own override.
      expect(isDisabled(saveButton(wrapper))).toBe(false)
      expect(isDisabled(clearButton(wrapper))).toBe(false)
      expect(wrapper.text()).toContain('Effective mode: Custom')
    })

    it('switching accounts clears credential inputs and mutation errors', async () => {
      mockedGetAccountProxySettings
        .mockResolvedValueOnce(customDto())
        .mockResolvedValueOnce(customDto())
      mockedSetAccountProxyOverride.mockRejectedValue(new Error('A failure'))

      const wrapper = mountPanel({ accountId: 'account-1' })
      await flushPromises()
      await selectMode(wrapper, 'Custom')
      await wrapper.find('#endpoint').setValue('http://proxy:8080')
      await wrapper.find('#username').setValue('a-user')
      await wrapper.find('#password').setValue('a-pass')
      await saveButton(wrapper)!.trigger('click')
      await flushPromises()
      expect(wrapper.text()).toContain('A failure')

      await wrapper.setProps({ accountId: 'account-2' })
      await flushPromises()

      // No A error and no A credentials survive into B.
      expect(wrapper.text()).not.toContain('A failure')
      expect(wrapper.find('#username').attributes('value')).toBe('')
      expect(wrapper.find('#password').attributes('value')).toBe('')
    })
  })
})
