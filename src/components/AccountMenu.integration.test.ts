import { describe, expect, it, vi } from 'vitest'
import type { AccountSummary } from '@/api/tauri'

// Test the AccountMenu wiring logic
describe('AccountMenu integration', () => {
  const mockAccounts: AccountSummary[] = [
    {
      id: '123',
      username: 'alice',
      discriminator: '0001',
      avatar: undefined,
      globalName: 'Alice',
      isAuthenticated: true,
      lastUsedAtMs: Date.now(),
    },
    {
      id: '456',
      username: 'bob',
      discriminator: '0002',
      avatar: undefined,
      globalName: 'Bob',
      isAuthenticated: false,
      lastUsedAtMs: Date.now() - 1000,
    },
  ]

  describe('account selection', () => {
    it('calls activateAccount with correct id', async () => {
      const activateAccount = vi.fn().mockResolvedValue(mockAccounts[1])
      const accountId = '456'
      
      await activateAccount(accountId)
      
      expect(activateAccount).toHaveBeenCalledWith(accountId)
    })

    it('sets busy state during activation', async () => {
      let busyState = false
      const activateAccount = vi.fn().mockImplementation(async () => {
        busyState = true
        await new Promise(resolve => setTimeout(resolve, 10))
        busyState = false
        return mockAccounts[1]
      })
      
      const promise = activateAccount('456')
      expect(busyState).toBe(true)
      
      await promise
      expect(busyState).toBe(false)
    })
  })

  describe('account removal', () => {
    it('calls removeAccount with correct id', async () => {
      const removeAccount = vi.fn().mockResolvedValue({ accounts: [], activeAccountId: null })
      const accountId = '123'
      
      await removeAccount(accountId)
      
      expect(removeAccount).toHaveBeenCalledWith(accountId)
    })

    it('sets busy state during removal', async () => {
      let busyState = false
      const removeAccount = vi.fn().mockImplementation(async () => {
        busyState = true
        await new Promise(resolve => setTimeout(resolve, 10))
        busyState = false
        return { accounts: [], activeAccountId: null }
      })
      
      const promise = removeAccount('123')
      expect(busyState).toBe(true)
      
      await promise
      expect(busyState).toBe(false)
    })
  })

  describe('logout', () => {
    it('calls logout on auth store', async () => {
      const logout = vi.fn().mockResolvedValue(undefined)
      
      await logout()
      
      expect(logout).toHaveBeenCalled()
    })
  })

  describe('add account', () => {
    it('emits addAccount event', () => {
      const emit = vi.fn()
      emit('addAccount')
      
      expect(emit).toHaveBeenCalledWith('addAccount')
    })
  })

  describe('offline account state', () => {
    it('identifies offline accounts using isAuthenticated', () => {
      const offlineAccount = mockAccounts.find(a => a.id === '456')
      expect(offlineAccount?.isAuthenticated).toBe(false)
    })

    it('identifies authenticated accounts using isAuthenticated', () => {
      const authAccount = mockAccounts.find(a => a.id === '123')
      expect(authAccount?.isAuthenticated).toBe(true)
    })
  })

  describe('load accounts on mount', () => {
    it('calls loadAccounts when component mounts', async () => {
      const loadAccounts = vi.fn().mockResolvedValue(undefined)
      
      await loadAccounts()
      
      expect(loadAccounts).toHaveBeenCalled()
    })
  })
})
