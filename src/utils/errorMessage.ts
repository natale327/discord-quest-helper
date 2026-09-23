const DEFAULT_ERROR_MESSAGE = 'An unexpected error occurred.'

export function toErrorMessage(error: unknown, fallback = DEFAULT_ERROR_MESSAGE): string {
  if (typeof error === 'string') return error
  if (error instanceof Error) return error.message
  if (typeof error !== 'object' || error === null) return fallback

  try {
    const structuredError = error as Record<string, unknown>
    if (typeof structuredError.message === 'string' && structuredError.message.length > 0) {
      return structuredError.message
    }
    if (typeof structuredError.code === 'string' && structuredError.code.length > 0) {
      return structuredError.code
    }
  } catch {
    // Unreadable objects should fall back rather than exposing or stringifying data.
  }

  return fallback
}
