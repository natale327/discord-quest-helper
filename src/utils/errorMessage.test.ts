import { describe, expect, it } from 'vitest'
import { toErrorMessage } from './errorMessage'

describe('toErrorMessage', () => {
  it('returns a string error unchanged', () => {
    expect(toErrorMessage('Could not launch Discord')).toBe('Could not launch Discord')
  })

  it('returns an Error message', () => {
    expect(toErrorMessage(new Error('Installation is missing'))).toBe('Installation is missing')
  })

  it('prefers the message from a structured backend error', () => {
    expect(toErrorMessage({
      code: 'port_occupied',
      params: { port: 9223, secret: 'do not show this' },
      message: 'Port 9223 is already in use.',
    })).toBe('Port 9223 is already in use.')
  })

  it('returns the code when a structured error has no message', () => {
    expect(toErrorMessage({ code: 'installation_missing', params: { path: 'private' } }))
      .toBe('installation_missing')
  })

  it('uses a generic fallback for arbitrary objects without exposing their fields', () => {
    expect(toErrorMessage({ params: { secret: 'do not show this' } }))
      .toBe('An unexpected error occurred.')
  })
})
