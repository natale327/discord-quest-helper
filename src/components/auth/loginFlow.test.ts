import { afterEach, describe, expect, it, vi } from 'vitest'
import type {
  AuthProgress,
  CdpStatus,
  DesktopClientInventory,
  DesktopClientState,
  RunningDesktopCdpSession,
} from '@/api/tauri'
import {
  canBeginLogin,
  classifyCdpAvailability,
  findCurrentCdpOwnerSession,
  hasUnchanneledOfficialMacInstallation,
  installedCdpLaunchTargets,
  presentAuthProgress,
  selectionForCdpLaunchTarget,
  selectionForCurrentCdpOwner,
  shouldAskCdpLaunchTarget,
  shouldPollCdp,
  startCdpPolling,
  usesVesktopForCdpLogin,
} from './loginFlow'

function inventory(overrides: Partial<DesktopClientInventory> = {}): DesktopClientInventory {
  return {
    officialInstalled: true,
    vesktopInstalled: true,
    officialRunning: false,
    vesktopRunning: false,
    cdpOwner: 'none',
    stableInstalled: true,
    ptbInstalled: false,
    canaryInstalled: false,
    stableRunning: false,
    ptbRunning: false,
    canaryRunning: false,
    ...overrides,
  }
}

function progress(phase: AuthProgress['phase']): AuthProgress {
  return {
    phase,
    current: null,
    total: null,
    valid_accounts: null,
  }
}

const offline: CdpStatus = {
  available: false,
  connected: false,
  target_title: null,
  error: 'connection refused',
}

describe('login progress presentation', () => {
  it('maps each surviving CDP phase to a running presentation', () => {
    expect(presentAuthProgress(progress('capturing_cdp_session'))).toEqual({
      key: 'auth.progress.capturing_cdp_session',
      state: 'running',
    })
    expect(presentAuthProgress(progress('validating_cdp_session'))).toEqual({
      key: 'auth.progress.validating_cdp_session',
      state: 'running',
    })
    expect(presentAuthProgress(progress('preparing_session'))).toEqual({
      key: 'auth.progress.preparing_session',
      state: 'running',
    })
  })

  it('marks completion as a successful terminal state', () => {
    expect(presentAuthProgress(progress('complete'))).toEqual({
      key: 'auth.progress.complete',
      state: 'success',
    })
  })
})

describe('CDP status and polling', () => {
  afterEach(() => vi.useRealTimers())

  it('distinguishes ready, starting, offline, and probe failure states', () => {
    expect(classifyCdpAvailability(true, null, false)).toBe('checking')
    expect(classifyCdpAvailability(false, { ...offline, available: true }, false)).toBe('starting')
    expect(classifyCdpAvailability(false, { ...offline, available: true, connected: true }, false)).toBe('ready')
    expect(classifyCdpAvailability(false, offline, false)).toBe('offline')
    expect(classifyCdpAvailability(false, null, true)).toBe('error')
  })

  it('pauses polling while busy, authenticated, or hidden', () => {
    expect(shouldPollCdp({ busy: false, authenticated: false, visible: true })).toBe(true)
    expect(shouldPollCdp({ busy: true, authenticated: false, visible: true })).toBe(false)
    expect(shouldPollCdp({ busy: false, authenticated: true, visible: true })).toBe(false)
    expect(shouldPollCdp({ busy: false, authenticated: false, visible: false })).toBe(false)
  })

  it('runs every five seconds and can be disposed', () => {
    vi.useFakeTimers()
    const callback = vi.fn()
    const stop = startCdpPolling(callback)

    vi.advanceTimersByTime(15_000)
    expect(callback).toHaveBeenCalledTimes(3)

    stop()
    vi.advanceTimersByTime(5_000)
    expect(callback).toHaveBeenCalledTimes(3)
  })
})

describe('CDP login client copy', () => {
  it('uses Discord copy unless Vesktop is connected or the only available client', () => {
    expect(usesVesktopForCdpLogin(null)).toBe(false)
    expect(usesVesktopForCdpLogin(inventory())).toBe(false)
    expect(usesVesktopForCdpLogin(inventory({ officialRunning: true, vesktopRunning: true }))).toBe(false)
    expect(usesVesktopForCdpLogin(inventory({ officialInstalled: false, stableInstalled: false }))).toBe(true)
    expect(usesVesktopForCdpLogin(inventory({ vesktopRunning: true }))).toBe(false)
    expect(usesVesktopForCdpLogin(inventory({ officialInstalled: false, stableInstalled: false, vesktopRunning: true }))).toBe(true)
    expect(usesVesktopForCdpLogin(inventory({ cdpOwner: 'vesktop' }))).toBe(true)
  })
})

describe('CDP launch target selection', () => {
  it('lists only installed official channels and Vesktop', () => {
    expect(installedCdpLaunchTargets(null)).toEqual([])
    expect(installedCdpLaunchTargets(inventory())).toEqual(['stable', 'vesktop'])
    expect(installedCdpLaunchTargets(inventory({
      ptbInstalled: true,
      canaryInstalled: true,
    }))).toEqual(['stable', 'ptb', 'canary', 'vesktop'])
  })

  it('keeps a valid unchanneled custom macOS Discord installation launchable', () => {
    const installation = {
      id: 'discord.official:custom-mac',
      providerId: 'discord.official',
      variantId: null,
      displayName: 'Discord (custom)',
      source: 'user',
      launchTarget: {
        kind: 'macBundle',
        bundlePath: '/Applications/My Discord.app',
        executablePath: '/Applications/My Discord.app/Contents/MacOS/Discord',
      },
      capabilities: { cdp: true, localToken: true, restoreNormal: true },
      validation: 'valid',
    } as DesktopClientState['installations'][number]
    const snapshot = {
      installations: [installation],
    } as DesktopClientState

    expect(hasUnchanneledOfficialMacInstallation(snapshot.installations)).toBe(true)
    expect(selectionForCdpLaunchTarget(snapshot, 'stable')).toEqual({
      kind: 'installation',
      installationId: installation.id,
    })
  })

  it('prefers the standard Stable installation when it coexists with a custom bundle', () => {
    const snapshot = {
      installations: [
        {
          id: 'discord.official:stable',
          providerId: 'discord.official',
          variantId: 'stable',
          displayName: 'Discord',
          source: 'standardPath',
          launchTarget: {
            kind: 'executable',
            path: '/Applications/Discord.app/Contents/MacOS/Discord',
            workingDir: '/Applications/Discord.app/Contents/MacOS',
            prefixArgs: [],
          },
          capabilities: { cdp: true, localToken: true, restoreNormal: true },
          validation: 'valid',
        },
        {
          id: 'discord.official:custom-mac',
          providerId: 'discord.official',
          variantId: null,
          displayName: 'Discord (custom)',
          source: 'user',
          launchTarget: {
            kind: 'macBundle',
            bundlePath: '/Applications/My Discord.app',
            executablePath: '/Applications/My Discord.app/Contents/MacOS/Discord',
          },
          capabilities: { cdp: true, localToken: true, restoreNormal: true },
          validation: 'valid',
        },
      ],
    } as DesktopClientState

    expect(selectionForCdpLaunchTarget(snapshot, 'stable')).toEqual({
      kind: 'provider',
      providerId: 'discord.official',
      variantId: 'stable',
    })
  })

  it('ignores user-added Stable executables when choosing the custom bundle fallback', () => {
    const snapshot = {
      installations: [
        {
          id: 'discord.official:user-stable',
          providerId: 'discord.official',
          variantId: 'stable',
          source: 'user',
          validation: 'valid',
        },
        {
          id: 'discord.official:custom-mac',
          providerId: 'discord.official',
          variantId: null,
          launchTarget: { kind: 'macBundle' },
          source: 'user',
          validation: 'valid',
        },
      ],
    } as DesktopClientState

    expect(selectionForCdpLaunchTarget(snapshot, 'stable')).toEqual({
      kind: 'installation',
      installationId: 'discord.official:custom-mac',
    })
  })

  it('preserves the exact installation for the current CDP owner', () => {
    const installation = {
      id: 'discord.official:custom-ptb',
    } as DesktopClientState['installations'][number]
    const snapshot = {
      installations: [installation],
      processes: [{
        providerId: 'discord.official',
        installationId: installation.id,
        variantId: 'ptb',
        executablePath: '/Applications/Discord PTB.app/Contents/MacOS/Discord PTB',
        running: true,
      }],
    } as DesktopClientState

    expect(selectionForCurrentCdpOwner(snapshot, {
      providerId: 'discord.official',
      installationId: installation.id,
      variantId: 'ptb',
    })).toEqual({
      kind: 'installation',
      installationId: installation.id,
    })
  })

  it('does not choose an owner when multiple sessions claim the same port', () => {
    const sessions = [
      {
        providerId: 'discord.official',
        installationId: 'discord.official:stable',
        variantId: 'stable',
        port: 9223,
        ownership: 'externalAttached',
        executablePath: '/Applications/Discord.app/Contents/MacOS/Discord',
      },
      {
        providerId: 'discord.official',
        installationId: 'discord.official:ptb',
        variantId: 'ptb',
        port: 9223,
        ownership: 'externalAttached',
        executablePath: '/Applications/Discord PTB.app/Contents/MacOS/Discord PTB',
      },
    ] as RunningDesktopCdpSession[]

    expect(findCurrentCdpOwnerSession(sessions, 9223, 'discord.official')).toBeNull()
  })

  it('asks only when CDP is down and more than one client is installed', () => {
    expect(shouldAskCdpLaunchTarget(true, ['stable', 'vesktop'])).toBe(false)
    expect(shouldAskCdpLaunchTarget(false, ['stable'])).toBe(false)
    expect(shouldAskCdpLaunchTarget(false, ['stable', 'canary', 'vesktop'])).toBe(true)
  })

})

describe('login operation gate', () => {
  it('rejects repeated actions until the active operation is released', () => {
    expect(canBeginLogin(null, false)).toBe(true)
    expect(canBeginLogin('cdp', false)).toBe(false)
    expect(canBeginLogin(null, true)).toBe(false)
    expect(canBeginLogin(null, false)).toBe(true)
  })
})
