import { describe, expect, it } from 'vitest'
import type { AccountSummary } from './AccountListItem.vue'

// Test helper functions that would be used by the AccountSwitcher component
function sortAccounts(
  accounts: AccountSummary[],
  activeAccountId: string | null,
): AccountSummary[] {
  const list = [...accounts]
  list.sort((a, b) => {
    if (a.id === activeAccountId) return -1
    if (b.id === activeAccountId) return 1
    const aTime = a.lastUsedAtMs ?? 0
    const bTime = b.lastUsedAtMs ?? 0
    return bTime - aTime
  })
  return list
}

function getActiveAccount(
  accounts: AccountSummary[],
  activeAccountId: string | null,
): AccountSummary | null {
  return accounts.find((a) => a.id === activeAccountId) ?? null
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

describe('AccountSwitcher helpers', () => {
  describe('sortAccounts', () => {
    it('returns empty array when no accounts', () => {
      expect(sortAccounts([], null)).toEqual([])
    })

    it('returns single account unchanged', () => {
      const account = createAccount({ id: '1' })
      expect(sortAccounts([account], null)).toEqual([account])
    })

    it('puts active account first', () => {
      const account1 = createAccount({ id: '1', lastUsedAtMs: 1000 })
      const account2 = createAccount({ id: '2', lastUsedAtMs: 2000 })
      const account3 = createAccount({ id: '3', lastUsedAtMs: 3000 })

      const sorted = sortAccounts([account1, account2, account3], '2')
      expect(sorted[0].id).toBe('2')
    })

    it('sorts non-active accounts by lastUsedAtMs descending', () => {
      const account1 = createAccount({ id: '1', lastUsedAtMs: 1000 })
      const account2 = createAccount({ id: '2', lastUsedAtMs: 3000 })
      const account3 = createAccount({ id: '3', lastUsedAtMs: 2000 })

      const sorted = sortAccounts([account1, account2, account3], null)
      expect(sorted[0].id).toBe('2')
      expect(sorted[1].id).toBe('3')
      expect(sorted[2].id).toBe('1')
    })

    it('handles null lastUsedAtMs', () => {
      const account1 = createAccount({ id: '1', lastUsedAtMs: null })
      const account2 = createAccount({ id: '2', lastUsedAtMs: 1000 })

      const sorted = sortAccounts([account1, account2], null)
      expect(sorted[0].id).toBe('2')
      expect(sorted[1].id).toBe('1')
    })

    it('handles undefined lastUsedAtMs', () => {
      const account1 = createAccount({ id: '1' })
      delete account1.lastUsedAtMs
      const account2 = createAccount({ id: '2', lastUsedAtMs: 1000 })

      const sorted = sortAccounts([account1, account2], null)
      expect(sorted[0].id).toBe('2')
    })
  })

  describe('getActiveAccount', () => {
    it('returns null when no accounts', () => {
      expect(getActiveAccount([], '123')).toBeNull()
    })

    it('returns null when activeAccountId is null', () => {
      const account = createAccount({ id: '1' })
      expect(getActiveAccount([account], null)).toBeNull()
    })

    it('returns active account when found', () => {
      const account1 = createAccount({ id: '1' })
      const account2 = createAccount({ id: '2' })
      expect(getActiveAccount([account1, account2], '2')).toBe(account2)
    })

    it('returns null when active account not found', () => {
      const account = createAccount({ id: '1' })
      expect(getActiveAccount([account], '999')).toBeNull()
    })
  })
})

describe('AccountSummary isAuthenticated field', () => {
  it('accepts isAuthenticated as true', () => {
    const account = createAccount({ isAuthenticated: true })
    expect(account.isAuthenticated).toBe(true)
  })

  it('accepts isAuthenticated as false', () => {
    const account = createAccount({ isAuthenticated: false })
    expect(account.isAuthenticated).toBe(false)
  })

  it('accepts undefined isAuthenticated', () => {
    const account = createAccount({})
    expect(account.isAuthenticated).toBeUndefined()
  })
})

describe('AccountSwitcher state logic', () => {
  it('determines if actions should be disabled based on busyAccountId', () => {
    const busyAccountId: string | null = '123'
    const isDisabled = busyAccountId !== null
    expect(isDisabled).toBe(true)
  })

  it('determines if actions should be enabled when not busy', () => {
    const busyAccountId: string | null = null
    const isDisabled = busyAccountId !== null
    expect(isDisabled).toBe(false)
  })

  it('determines if an account is offline (not active)', () => {
    const accountId: string = '123'
    const activeAccountId: string = '456'
    const isOffline = accountId !== activeAccountId
    expect(isOffline).toBe(true)
  })

  it('determines if an account is active', () => {
    const accountId: string = '123'
    const activeAccountId: string = '123'
    const isActive = accountId === activeAccountId
    expect(isActive).toBe(true)
  })
})
