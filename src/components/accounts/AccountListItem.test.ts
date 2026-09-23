import { describe, expect, it } from 'vitest'
import type { AccountSummary } from './AccountListItem.vue'

// Test helper functions that would be used by the components
function getDisplayName(account: AccountSummary): string {
  return account.globalName || account.username
}

function getAvatarUrl(account: AccountSummary): string | null {
  if (!account.avatar) return null
  return `https://cdn.discordapp.com/avatars/${account.id}/${account.avatar}.png?size=128`
}

function getInitials(account: AccountSummary): string {
  const name = getDisplayName(account)
  return name ? name[0].toUpperCase() : '?'
}

function getTagline(account: AccountSummary): string {
  if (account.discriminator && account.discriminator !== '0') {
    return `${account.username}#${account.discriminator}`
  }
  return `@${account.username}`
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

describe('AccountListItem helpers', () => {
  describe('getDisplayName', () => {
    it('returns globalName when available', () => {
      const account = createAccount({ username: 'alice', globalName: 'Alice W' })
      expect(getDisplayName(account)).toBe('Alice W')
    })

    it('returns username when globalName is null', () => {
      const account = createAccount({ username: 'bob', globalName: null })
      expect(getDisplayName(account)).toBe('bob')
    })

    it('returns username when globalName is empty', () => {
      const account = createAccount({ username: 'charlie', globalName: '' })
      expect(getDisplayName(account)).toBe('charlie')
    })
  })

  describe('getAvatarUrl', () => {
    it('returns null when avatar is null', () => {
      const account = createAccount({ id: '123', avatar: null })
      expect(getAvatarUrl(account)).toBeNull()
    })

    it('returns CDN URL when avatar is provided', () => {
      const account = createAccount({ id: '456', avatar: 'abc123' })
      expect(getAvatarUrl(account)).toBe(
        'https://cdn.discordapp.com/avatars/456/abc123.png?size=128'
      )
    })
  })

  describe('getInitials', () => {
    it('returns first letter of globalName when available', () => {
      const account = createAccount({ globalName: 'Alice' })
      expect(getInitials(account)).toBe('A')
    })

    it('returns first letter of username when globalName is null', () => {
      const account = createAccount({ username: 'bob', globalName: null })
      expect(getInitials(account)).toBe('B')
    })

    it('returns uppercase initial', () => {
      const account = createAccount({ username: 'charlie' })
      expect(getInitials(account)).toBe('C')
    })

    it('returns ? when no name available', () => {
      const account = createAccount({ username: '', globalName: null })
      expect(getInitials(account)).toBe('?')
    })
  })

  describe('getTagline', () => {
    it('returns username#discriminator when discriminator is present', () => {
      const account = createAccount({ username: 'alice', discriminator: '0001' })
      expect(getTagline(account)).toBe('alice#0001')
    })

    it('returns @username when discriminator is "0"', () => {
      const account = createAccount({ username: 'bob', discriminator: '0' })
      expect(getTagline(account)).toBe('@bob')
    })

    it('returns @username when discriminator is null', () => {
      const account = createAccount({ username: 'charlie', discriminator: null })
      expect(getTagline(account)).toBe('@charlie')
    })

    it('returns @username when discriminator is empty string', () => {
      const account = createAccount({ username: 'dave', discriminator: '' })
      expect(getTagline(account)).toBe('@dave')
    })
  })

  describe('isOffline determination', () => {
    it('returns false when isAuthenticated is true', () => {
      const account = createAccount({ isAuthenticated: true })
      const isOffline = !account.isAuthenticated
      expect(isOffline).toBe(false)
    })

    it('returns true when isAuthenticated is false', () => {
      const account = createAccount({ isAuthenticated: false })
      const isOffline = !account.isAuthenticated
      expect(isOffline).toBe(true)
    })

    it('returns undefined when isAuthenticated is not set', () => {
      const account = createAccount({})
      expect(account.isAuthenticated).toBeUndefined()
    })
  })
})

describe('AccountSummary type', () => {
  it('accepts valid account object', () => {
    const account: AccountSummary = {
      id: '123',
      username: 'testuser',
      discriminator: '1234',
      avatar: 'abc',
      globalName: 'Test User',
      lastCdpPort: 9223,
      lastUsedAtMs: Date.now(),
      isAuthenticated: true,
    }
    expect(account.id).toBe('123')
  })

  it('accepts minimal account object', () => {
    const account: AccountSummary = {
      id: '123',
      username: 'testuser',
    }
    expect(account.id).toBe('123')
  })

  it('accepts null values for optional fields', () => {
    const account: AccountSummary = {
      id: '123',
      username: 'testuser',
      discriminator: null,
      avatar: null,
      globalName: null,
      lastCdpPort: null,
      lastUsedAtMs: null,
      isAuthenticated: false,
    }
    expect(account.discriminator).toBeNull()
  })

  it('accepts isAuthenticated as optional', () => {
    const account: AccountSummary = {
      id: '123',
      username: 'testuser',
    }
    expect(account.isAuthenticated).toBeUndefined()
  })
})
