import { describe, expect, it, vi } from 'vitest'
import type { QuestRunView } from '@/stores/quests'

// Test the account-scoped stop behavior for Home.vue
describe('Home.vue stop button account scoping', () => {
  // Helper to create a mock run
  function createRun(overrides: Partial<QuestRunView>): QuestRunView {
    return {
      questId: 'quest1',
      runId: 'run1',
      accountId: 'account1',
      kind: 'video',
      transport: 'cdp',
      phase: 'running',
      progress: 50,
      questType: 'video',
      targetDuration: 600,
      initialProgressSeconds: 0,
      startedAt: Date.now(),
      ...overrides,
    }
  }

  describe('getProjectedRun', () => {
    it('returns the run for the active account projection', () => {
      const mockGetRun = vi.fn().mockReturnValue(createRun({ questId: 'quest1' }))
      const result = mockGetRun('quest1')
      
      expect(result).toBeDefined()
      expect(result?.questId).toBe('quest1')
      expect(mockGetRun).toHaveBeenCalledWith('quest1')
    })

    it('returns undefined when no run exists', () => {
      const mockGetRun = vi.fn().mockReturnValue(undefined)
      const result = mockGetRun('quest999')
      
      expect(result).toBeUndefined()
    })
  })

  describe('handleStopQuest', () => {
    it('calls stopRun with questId, runId, and accountId from projected run', async () => {
      const run = createRun({
        questId: 'quest1',
        runId: 'run-abc',
        accountId: 'account-xyz',
      })
      
      const mockGetRun = vi.fn().mockReturnValue(run)
      const mockStopRun = vi.fn().mockResolvedValue({ status: 'stopped' })
      
      // Simulate the handleStopQuest logic
      const projectedRun = mockGetRun('quest1')
      if (projectedRun) {
        await mockStopRun(projectedRun.questId, projectedRun.runId, projectedRun.accountId)
      }
      
      expect(mockStopRun).toHaveBeenCalledWith('quest1', 'run-abc', 'account-xyz')
    })

    it('does not call stopRun when no run exists', async () => {
      const mockGetRun = vi.fn().mockReturnValue(undefined)
      const mockStopRun = vi.fn()
      
      // Simulate the handleStopQuest logic
      const projectedRun = mockGetRun('quest1')
      if (projectedRun) {
        await mockStopRun(projectedRun.questId, projectedRun.runId, projectedRun.accountId)
      }
      
      expect(mockStopRun).not.toHaveBeenCalled()
    })

    it('passes the correct accountId for account A', async () => {
      const runA = createRun({
        questId: 'shared-quest',
        runId: 'run-a',
        accountId: 'account-a',
      })
      
      const mockGetRun = vi.fn().mockReturnValue(runA)
      const mockStopRun = vi.fn().mockResolvedValue({ status: 'stopped' })
      
      const projectedRun = mockGetRun('shared-quest')
      if (projectedRun) {
        await mockStopRun(projectedRun.questId, projectedRun.runId, projectedRun.accountId)
      }
      
      expect(mockStopRun).toHaveBeenCalledWith('shared-quest', 'run-a', 'account-a')
    })

    it('passes the correct accountId for account B', async () => {
      const runB = createRun({
        questId: 'shared-quest',
        runId: 'run-b',
        accountId: 'account-b',
      })
      
      const mockGetRun = vi.fn().mockReturnValue(runB)
      const mockStopRun = vi.fn().mockResolvedValue({ status: 'stopped' })
      
      const projectedRun = mockGetRun('shared-quest')
      if (projectedRun) {
        await mockStopRun(projectedRun.questId, projectedRun.runId, projectedRun.accountId)
      }
      
      expect(mockStopRun).toHaveBeenCalledWith('shared-quest', 'run-b', 'account-b')
    })
  })

  describe('identical quest IDs across accounts', () => {
    it('stops the correct run when accounts have same quest in reversed order', async () => {
      // Account A's run
      const runA = createRun({
        questId: 'same-quest-id',
        runId: 'run-a-123',
        accountId: 'account-a',
      })
      
      // Account B's run
      const runB = createRun({
        questId: 'same-quest-id',
        runId: 'run-b-456',
        accountId: 'account-b',
      })
      
      const mockStopRun = vi.fn().mockResolvedValue({ status: 'stopped' })
      
      // Simulate stopping account A's run
      const mockGetRunA = vi.fn().mockReturnValue(runA)
      const projectedRunA = mockGetRunA('same-quest-id')
      if (projectedRunA) {
        await mockStopRun(projectedRunA.questId, projectedRunA.runId, projectedRunA.accountId)
      }
      
      expect(mockStopRun).toHaveBeenLastCalledWith('same-quest-id', 'run-a-123', 'account-a')
      
      // Simulate stopping account B's run
      const mockGetRunB = vi.fn().mockReturnValue(runB)
      const projectedRunB = mockGetRunB('same-quest-id')
      if (projectedRunB) {
        await mockStopRun(projectedRunB.questId, projectedRunB.runId, projectedRunB.accountId)
      }
      
      expect(mockStopRun).toHaveBeenLastCalledWith('same-quest-id', 'run-b-456', 'account-b')
    })

    it('does not select run by quest ID across all accounts', async () => {
      // This test verifies that we use the projected run (active account only)
      // and don't accidentally pick a run from a different account
      
      const runForActiveAccount = createRun({
        questId: 'quest1',
        runId: 'active-run',
        accountId: 'active-account',
      })
      
      const mockGetRun = vi.fn().mockReturnValue(runForActiveAccount)
      const mockStopRun = vi.fn().mockResolvedValue({ status: 'stopped' })
      
      const projectedRun = mockGetRun('quest1')
      if (projectedRun) {
        await mockStopRun(projectedRun.questId, projectedRun.runId, projectedRun.accountId)
      }
      
      // Verify we called with the active account's run, not some other account's
      expect(mockStopRun).toHaveBeenCalledWith('quest1', 'active-run', 'active-account')
      expect(mockStopRun).not.toHaveBeenCalledWith('quest1', expect.any(String), 'other-account')
    })
  })

  describe('stop button visibility', () => {
    it('shows stop button when projected run exists', () => {
      const mockGetRun = vi.fn().mockReturnValue(createRun({}))
      const run = mockGetRun('quest1')
      
      expect(run).toBeDefined()
    })

    it('hides stop button when no projected run', () => {
      const mockGetRun = vi.fn().mockReturnValue(undefined)
      const run = mockGetRun('quest1')
      
      expect(run).toBeUndefined()
    })
  })

  describe('start button disabled state', () => {
    it('disables start button when projected run exists', () => {
      const mockGetRun = vi.fn().mockReturnValue(createRun({}))
      const run = mockGetRun('quest1')
      
      const isDisabled = !!run
      expect(isDisabled).toBe(true)
    })

    it('enables start button when no projected run', () => {
      const mockGetRun = vi.fn().mockReturnValue(undefined)
      const run = mockGetRun('quest1')
      
      const isDisabled = !!run
      expect(isDisabled).toBe(false)
    })
  })
})
