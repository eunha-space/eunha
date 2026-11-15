import { getMeId } from '../me.ts'

// Mastodon keeps the hints and confirmations a member has said not to show
// again among the account's web settings, `dismissed_banners`, by id. eunha
// keeps them in the browser, per account, under the same ids.
const PREFIX = 'eunha:dismissed:'

function key(id: string): string {
  return `${PREFIX}${id}:${getMeId() ?? ''}`
}

const listeners = new Set<() => void>()

export function subscribeDismissed(listener: () => void): () => void {
  listeners.add(listener)
  return () => {
    listeners.delete(listener)
  }
}

export function isDismissed(id: string): boolean {
  try {
    return localStorage.getItem(key(id)) === '1'
  } catch {
    return false
  }
}

export function dismiss(id: string): void {
  if (isDismissed(id)) return
  try {
    localStorage.setItem(key(id), '1')
  } catch {
    // It comes back next time; nothing else depends on it.
  }
  for (const listener of listeners) listener()
}
