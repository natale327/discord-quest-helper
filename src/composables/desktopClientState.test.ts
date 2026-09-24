// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { effectScope, nextTick } from 'vue'
import type { DesktopClientState, RunningDesktopCdpSession } from '@/api/tauri'

const mocks = vi.hoisted(() => ({
  getDesktopClientState: vi.fn(),
  listRunningDesktopCdpSessions: vi.fn(),
  authStore: {
    activeAccountId: 'A' as string | null,
    accounts: [] as Array<{
      id: string
      username: string
      discriminator?: string
      avatar?: string
      globalName?: string
      lastCdpPort?: number
      lastUsedAtMs?: number
      isAuthenticated: boolean
    }>,
    accountPorts: {} as Record<string, number | null>,
    portForAccount: vi.fn(),
    previewClientAccount: vi.fn(),
    invalidateClientAccountPreviews: vi.fn(),
  },
  questsStore: {
    cdpPort: 9223,
  },
}))

vi.mock('@/api/tauri', async (importOriginal) => {
  const actual = await importOriginal<typeof import('@/api/tauri')>()
  return {
    ...actual,
    getDesktopClientState: mocks.getDesktopClientState,
    listRunningDesktopCdpSessions: mocks.listRunningDesktopCdpSessions,
  }
})

vi.mock('@/stores/auth', async () => {
  const { reactive } = await import('vue')
  return { useAuthStore: () => reactive(mocks.authStore) }
})

vi.mock('@/stores/quests', async () => {
  const { reactive } = await import('vue')
  return { useQuestsStore: () => reactive(mocks.questsStore) }
})

import { useAuthStore } from '@/stores/auth'
import {
  MAX_ACCOUNT_CLIENT_CANDIDATES,
  MAX_CONCURRENT_ACCOUNT_CLIENT_PROBES,
  useDesktopClientState,
} from './desktopClientState'

function createDeferred<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((res) => { resolve = res })
  return { promise, resolve }
}

function runningSession(port: number, installationId = `stable-${port}`): RunningDesktopCdpSession {
  return {
    providerId: 'discord.official',
    installationId,
    variantId: 'stable',
    port,
    ownership: 'managed',
    executablePath: null,
  }
}

function desktopState(port: number, endpointReady: boolean): DesktopClientState {
  return {
    installations: [{
      id: `stable-${port}`,
      providerId: 'discord.official',
      variantId: 'stable',
      displayName: 'Discord Stable',
      source: 'standardPath',
      launchTarget: {
        kind: 'executable',
        path: 'C:/fake/Discord.exe',
        workingDir: 'C:/fake',
        prefixArgs: [],
      },
      capabilities: { cdp: true, localToken: false, restoreNormal: true },
      validation: 'valid',
    }],
    processes: endpointReady ? [{
      providerId: 'discord.official',
      installationId: `stable-${port}`,
      variantId: 'stable',
      executablePath: null,
      running: true,
    }] : [],
    endpoint: {
      port,
      status: endpointReady ? 'discordReady' : 'unreachable',
      owner: endpointReady ? 'official' : 'none',
      ownerProviderId: endpointReady ? 'discord.official' : null,
      targetTitle: 'Discord — never use title as identity',
    },
    selection: { kind: 'installation', installationId: `stable-${port}` },
    discoveryIssues: [],
    port,
    revision: 1,
  }
}

function createComposableScope() {
  const scope = effectScope()
  const clients = scope.run(() => useDesktopClientState())!
  return { clients, stop: () => scope.stop() }
}

describe('desktop client-first account discovery', () => {
  beforeEach(() => {
    vi.clearAllMocks()
    localStorage.clear()
    const auth = useAuthStore()
    auth.activeAccountId = 'A'
    auth.accounts = [{ id: 'A', username: 'A', isAuthenticated: false, lastCdpPort: 9444 }]
    auth.accountPorts = { A: 9333, B: null }
    mocks.authStore.portForAccount.mockImplementation((id: string) => {
      if (Object.prototype.hasOwnProperty.call(mocks.authStore.accountPorts, id)) {
        return mocks.authStore.accountPorts[id] ?? 0
      }
      return mocks.authStore.accounts.find(account => account.id === id)?.lastCdpPort ?? mocks.questsStore.cdpPort
    })
    mocks.authStore.previewClientAccount.mockResolvedValue({
      port: 9224,
      user: {
        id: 'user-9224',
        username: 'preview-user',
        discriminator: '0',
        avatar: null,
        global_name: 'Preview User',
      },
    })
    mocks.questsStore.cdpPort = 9223
    mocks.listRunningDesktopCdpSessions.mockResolvedValue([runningSession(9224)])
    mocks.getDesktopClientState.mockImplementation(async (port: number) => desktopState(port, port === 9224))
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  it('aggregates known and running ports without previewing every candidate', async () => {
    const { clients, stop } = createComposableScope()

    const candidates = await clients.scanAccountClients()

    expect(candidates).toHaveLength(1)
    expect(candidates[0]).toMatchObject({
      port: 9224,
      providerId: 'discord.official',
      variantId: 'stable',
      installationId: 'stable-9224',
      displayName: 'Discord Stable',
      ready: true,
    })
    expect(mocks.listRunningDesktopCdpSessions).toHaveBeenCalledOnce()
    expect(mocks.getDesktopClientState.mock.calls.map(call => call[0])).toEqual(
      expect.arrayContaining([9223, 9224, 9225, 9226, 9333, 9444]),
    )
    expect(mocks.authStore.previewClientAccount).not.toHaveBeenCalled()
    stop()
  })

  it('bounds a large inventory and port set while keeping a high-priority ready session', async () => {
    const auth = useAuthStore()
    auth.accounts = Array.from({ length: 200 }, (_, index) => ({
      id: `saved-${index}`,
      username: `Saved ${index}`,
      isAuthenticated: false,
      lastCdpPort: 20000 + index,
    }))
    auth.activeAccountId = 'saved-0'
    auth.accountPorts = {
      'saved-0': 9333,
      ...Object.fromEntries(Array.from({ length: 200 }, (_, index) => [`saved-${index}`, 21000 + index])),
    }
    const { clients, stop } = createComposableScope()
    mocks.listRunningDesktopCdpSessions.mockResolvedValue([
      ...Array.from({ length: 200 }, (_, index) => runningSession(10000 + index)),
      runningSession(9224),
    ])
    let activeProbes = 0
    let maximumActiveProbes = 0
    mocks.getDesktopClientState.mockImplementation(async (port: number) => {
      activeProbes += 1
      maximumActiveProbes = Math.max(maximumActiveProbes, activeProbes)
      await Promise.resolve()
      activeProbes -= 1
      return desktopState(port, port === 9224)
    })

    const candidates = await clients.scanAccountClients()

    expect(mocks.getDesktopClientState).toHaveBeenCalledTimes(MAX_ACCOUNT_CLIENT_CANDIDATES)
    expect(maximumActiveProbes).toBeGreaterThan(1)
    expect(maximumActiveProbes).toBeLessThanOrEqual(MAX_CONCURRENT_ACCOUNT_CLIENT_PROBES)
    expect(candidates.length).toBeLessThanOrEqual(MAX_ACCOUNT_CLIENT_CANDIDATES)
    expect(candidates).toContainEqual(expect.objectContaining({ port: 9224, ready: true }))
    stop()
  })

  it('previews only the selected ready client card', async () => {
    const { clients, stop } = createComposableScope()
    await clients.scanAccountClients()

    const preview = await clients.selectAccountClientCandidate(clients.accountCandidates.value[0]!.id)

    expect(preview?.user.id).toBe('user-9224')
    expect(mocks.authStore.previewClientAccount).toHaveBeenCalledOnce()
    expect(mocks.authStore.previewClientAccount).toHaveBeenCalledWith(9224)
    stop()
  })

  it('discards a late client scan after the user selects a candidate', async () => {
    const { clients, stop } = createComposableScope()
    await clients.scanAccountClients()
    const originalIds = clients.accountCandidates.value.map(candidate => candidate.id)
    const lateInventory = createDeferred<RunningDesktopCdpSession[]>()
    mocks.listRunningDesktopCdpSessions.mockReturnValueOnce(lateInventory.promise)

    const staleScan = clients.scanAccountClients()
    await vi.waitFor(() => expect(mocks.listRunningDesktopCdpSessions).toHaveBeenCalledTimes(2))
    await clients.selectAccountClientCandidate(originalIds[0]!)
    lateInventory.resolve([runningSession(9225, 'late-installation')])
    await staleScan

    expect(clients.accountCandidates.value.map(candidate => candidate.id)).toEqual(originalIds)
    expect(mocks.authStore.previewClientAccount).toHaveBeenCalledWith(9224)
    stop()
  })

  it('discards an older preview after another candidate is selected', async () => {
    const { clients, stop } = createComposableScope()
    mocks.listRunningDesktopCdpSessions.mockResolvedValue([
      runningSession(9224, 'stable-9224'),
      { ...runningSession(9225, 'ptb-9225'), variantId: 'ptb' },
    ])
    mocks.getDesktopClientState.mockImplementation(async (port: number) => desktopState(port, port === 9224 || port === 9225))
    await clients.scanAccountClients()
    const candidates = clients.accountCandidates.value
    const firstPreview = createDeferred<{ port: number; user: { id: string; username: string; discriminator: string; avatar: null; global_name: string } }>()
    mocks.authStore.previewClientAccount
      .mockReturnValueOnce(firstPreview.promise)
      .mockResolvedValueOnce({
        port: 9225,
        user: { id: 'new-user', username: 'new', discriminator: '0', avatar: null, global_name: 'New' },
      })

    const oldRequest = clients.selectAccountClientCandidate(candidates.find(item => item.port === 9224)!.id)
    await vi.waitFor(() => expect(mocks.authStore.previewClientAccount).toHaveBeenCalledOnce())
    const newRequest = clients.selectAccountClientCandidate(candidates.find(item => item.port === 9225)!.id)
    await expect(newRequest).resolves.toMatchObject({ user: { id: 'new-user' } })
    firstPreview.resolve({
      port: 9224,
      user: { id: 'old-user', username: 'old', discriminator: '0', avatar: null, global_name: 'Old' },
    })
    await expect(oldRequest).resolves.toBeNull()

    expect(clients.selectedAccountCandidateId.value).toBe(candidates.find(item => item.port === 9225)!.id)
    expect(clients.selectedClientAccountPreview.value?.user.id).toBe('new-user')
    stop()
  })

  it('discards late scans and previews after selection cancellation or account switch', async () => {
    const authStore = useAuthStore()
    const { clients, stop } = createComposableScope()
    await clients.scanAccountClients()
    const candidate = clients.accountCandidates.value[0]!
    const latePreview = createDeferred<{ port: number; user: { id: string; username: string; discriminator: string; avatar: null; global_name: string } }>()
    mocks.authStore.previewClientAccount.mockReturnValueOnce(latePreview.promise)
    const pending = clients.selectAccountClientCandidate(candidate.id)
    await vi.waitFor(() => expect(mocks.authStore.previewClientAccount).toHaveBeenCalledOnce())
    clients.cancelAccountClientSelection()
    latePreview.resolve({
      port: 9224,
      user: { id: 'late', username: 'late', discriminator: '0', avatar: null, global_name: 'Late' },
    })
    await expect(pending).resolves.toBeNull()
    expect(clients.selectedClientAccountPreview.value).toBeNull()

    const secondPreview = createDeferred<{ port: number; user: { id: string; username: string; discriminator: string; avatar: null; global_name: string } }>()
    mocks.authStore.previewClientAccount.mockReturnValueOnce(secondPreview.promise)
    const pendingAfterSwitch = clients.selectAccountClientCandidate(candidate.id)
    await vi.waitFor(() => expect(mocks.authStore.previewClientAccount).toHaveBeenCalledTimes(2))
    authStore.activeAccountId = 'B'
    await nextTick()
    secondPreview.resolve({
      port: 9224,
      user: { id: 'switched-late', username: 'late', discriminator: '0', avatar: null, global_name: 'Late' },
    })
    await expect(pendingAfterSwitch).resolves.toBeNull()
    expect(clients.selectedAccountCandidateId.value).toBeNull()
    expect(clients.selectedClientAccountPreview.value).toBeNull()
    stop()
  })
})
