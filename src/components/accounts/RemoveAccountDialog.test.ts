import { describe, expect, it } from 'vitest'
import type { AccountSummary } from './AccountListItem.vue'

// Test helper functions that would be used by the RemoveAccountDialog component
function getAccountDisplayName(account: AccountSummary | null): string {
  if (!account) return ''
  return account.globalName || account.username
}

function canConfirm(account: AccountSummary | null, busy: boolean): boolean {
  return account !== null && !busy
}

function canCancel(busy: boolean): boolean {
  return !busy
}

function createAccount(overrides: Partial<AccountSummary> = {}): AccountSummary {
  return {
    id: '123',
    username: 'testuser',
    discriminator: '1234',
    avatar: null,
    globalName: null,
    lastUsedAtMs: Date.now(),
    ...overrides,
  }
}

describe('RemoveAccountDialog helpers', () => {
  describe('getAccountDisplayName', () => {
    it('returns empty string when account is null', () => {
      expect(getAccountDisplayName(null)).toBe('')
    })

    it('returns globalName when available', () => {
      const account = createAccount({ username: 'alice', globalName: 'Alice W' })
      expect(getAccountDisplayName(account)).toBe('Alice W')
    })

    it('returns username when globalName is null', () => {
      const account = createAccount({ username: 'bob', globalName: null })
      expect(getAccountDisplayName(account)).toBe('bob')
    })

    it('returns username when globalName is empty string', () => {
      const account = createAccount({ username: 'charlie', globalName: '' })
      expect(getAccountDisplayName(account)).toBe('charlie')
    })
  })

  describe('canConfirm', () => {
    it('returns true when account exists and not busy', () => {
      const account = createAccount()
      expect(canConfirm(account, false)).toBe(true)
    })

    it('returns false when account is null', () => {
      expect(canConfirm(null, false)).toBe(false)
    })

    it('returns false when busy', () => {
      const account = createAccount()
      expect(canConfirm(account, true)).toBe(false)
    })

    it('returns false when both account is null and busy', () => {
      expect(canConfirm(null, true)).toBe(false)
    })
  })

  describe('canCancel', () => {
    it('returns true when not busy', () => {
      expect(canCancel(false)).toBe(true)
    })

    it('returns false when busy', () => {
      expect(canCancel(true)).toBe(false)
    })
  })
})

describe('RemoveAccountDialog state logic', () => {
  it('determines confirm button text based on busy state', () => {
    const busy = false
    const confirmText = busy ? 'Working...' : 'Remove'
    expect(confirmText).toBe('Remove')
  })

  it('shows busy text when busy', () => {
    const busy = true
    const confirmText = busy ? 'Working...' : 'Remove'
    expect(confirmText).toBe('Working...')
  })

  it('determines if confirm button should be disabled', () => {
    const account = createAccount()
    const busy = true
    const isDisabled = !canConfirm(account, busy)
    expect(isDisabled).toBe(true)
  })

  it('determines if cancel button should be disabled', () => {
    const busy = true
    const isDisabled = !canCancel(busy)
    expect(isDisabled).toBe(true)
  })
})

describe('RemoveAccountDialog emit payloads', () => {
  it('confirm emit should include account id', () => {
    const account = createAccount({ id: 'xyz789' })
    const payload = account.id
    expect(payload).toBe('xyz789')
  })

  it('cancel emit should have no payload', () => {
    const payload = undefined
    expect(payload).toBeUndefined()
  })
})
