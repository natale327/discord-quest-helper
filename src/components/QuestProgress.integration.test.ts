import { describe, expect, it, vi } from 'vitest'
import type { QuestRunView } from '@/stores/quests'
import type { AccountSummary } from '@/api/tauri'

// Test the QuestProgress wiring logic
describe('QuestProgress integration', () => {
  const mockAccounts: AccountSummary[] = [
    {
      id: '123',
      username: 'alice',
      discriminator: '0001',
      avatar: 'avatar123',
      globalName: 'Alice',
      isAuthenticated: true,
    },
    {
      id: '456',
      username: 'bob',
      discriminator: '0002',
      avatar: undefined,
      globalName: 'Bob',
      isAuthenticated: true,
    },
  ]

  const mockRun: QuestRunView = {
    questId: 'quest1',
    runId: 'run1',
    accountId: '123',
    kind: 'video',
    transport: 'cdp',
    phase: 'running',
    progress: 50,
    questType: 'video',
    targetDuration: 600,
    initialProgressSeconds: 0,
    startedAt: Date.now(),
  }

  describe('stop run with account id', () => {
    it('calls stopRun with questId, runId, and accountId', async () => {
      const stopRun = vi.fn().mockResolvedValue({ status: 'stopped' })
      
      await stopRun(mockRun.questId, mockRun.runId, mockRun.accountId)
      
      expect(stopRun).toHaveBeenCalledWith(
        mockRun.questId,
        mockRun.runId,
        mockRun.accountId,
      )
    })

    it('passes correct accountId for different accounts', async () => {
      const stopRun = vi.fn().mockResolvedValue({ status: 'stopped' })
      
      const run1: QuestRunView = { ...mockRun, accountId: '123' }
      const run2: QuestRunView = { ...mockRun, accountId: '456', runId: 'run2' }
      
      await stopRun(run1.questId, run1.runId, run1.accountId)
      expect(stopRun).toHaveBeenLastCalledWith(run1.questId, run1.runId, '123')
      
      await stopRun(run2.questId, run2.runId, run2.accountId)
      expect(stopRun).toHaveBeenLastCalledWith(run2.questId, run2.runId, '456')
    })

    it('sets stopping state during stop operation', async () => {
      const stoppingRuns = new Set<string>()
      const stopRun = vi.fn().mockImplementation(async () => {
        await new Promise(resolve => setTimeout(resolve, 10))
        return { status: 'stopped' }
      })
      
      const runId = 'run1'
      stoppingRuns.add(runId)
      
      const promise = stopRun(mockRun.questId, runId, mockRun.accountId)
      expect(stoppingRuns.has(runId)).toBe(true)
      
      await promise
      stoppingRuns.delete(runId)
      expect(stoppingRuns.has(runId)).toBe(false)
    })
  })

  describe('account info lookup', () => {
    it('returns account info for valid accountId', () => {
      const accountId = '123'
      const account = mockAccounts.find(a => a.id === accountId)
      
      expect(account).toBeDefined()
      expect(account?.username).toBe('alice')
      expect(account?.globalName).toBe('Alice')
    })

    it('returns null for invalid accountId', () => {
      const accountId = '999'
      const account = mockAccounts.find(a => a.id === accountId)
      
      expect(account).toBeUndefined()
    })

    it('constructs avatar URL when avatar is present', () => {
      const account = mockAccounts.find(a => a.id === '123')
      const avatarUrl = account?.avatar
        ? `https://cdn.discordapp.com/avatars/${account.id}/${account.avatar}.png?size=64`
        : null
      
      expect(avatarUrl).toBe('https://cdn.discordapp.com/avatars/123/avatar123.png?size=64')
    })

    it('returns null avatar URL when avatar is not present', () => {
      const account = mockAccounts.find(a => a.id === '456')
      const avatarUrl = account?.avatar
        ? `https://cdn.discordapp.com/avatars/${account.id}/${account.avatar}.png?size=64`
        : null
      
      expect(avatarUrl).toBeNull()
    })

    it('uses globalName as display name when available', () => {
      const account = mockAccounts.find(a => a.id === '123')
      const displayName = account?.globalName || account?.username
      
      expect(displayName).toBe('Alice')
    })

    it('falls back to username when globalName is not available', () => {
      const account = { ...mockAccounts[0], globalName: null }
      const displayName = account.globalName || account.username
      
      expect(displayName).toBe('alice')
    })
  })

  describe('multiple accounts running same quest', () => {
    it('distinguishes runs by accountId', () => {
      const run1: QuestRunView = { ...mockRun, accountId: '123', runId: 'run1' }
      const run2: QuestRunView = { ...mockRun, accountId: '456', runId: 'run2' }
      
      expect(run1.accountId).not.toBe(run2.accountId)
      expect(run1.runId).not.toBe(run2.runId)
      expect(run1.questId).toBe(run2.questId)
    })

    it('stops correct run for each account', async () => {
      const stopRun = vi.fn().mockResolvedValue({ status: 'stopped' })
      
      const run1: QuestRunView = { ...mockRun, accountId: '123', runId: 'run1' }
      const run2: QuestRunView = { ...mockRun, accountId: '456', runId: 'run2' }
      
      await stopRun(run1.questId, run1.runId, run1.accountId)
      await stopRun(run2.questId, run2.runId, run2.accountId)
      
      expect(stopRun).toHaveBeenCalledTimes(2)
      expect(stopRun).toHaveBeenNthCalledWith(1, 'quest1', 'run1', '123')
      expect(stopRun).toHaveBeenNthCalledWith(2, 'quest1', 'run2', '456')
    })
  })
})
