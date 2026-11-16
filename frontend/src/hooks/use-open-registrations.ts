import { useEffect, useState } from 'react'
import type { mastodon } from '../masto.ts'
import { getCachedInstance, getInstance } from '../api.ts'

// Anyone may sign up without an invite or an approval, so an invite gives no
// one anything they could not already do: the invite pages are hidden.
function isOpen(instance: mastodon.v2.Instance | null): boolean | null {
  if (!instance) return null
  return instance.registrations.enabled && !instance.registrations.approvalRequired
}

// `null` until `/api/v2/instance` answers, so callers can tell "not open" from
// "not known yet".
export function useOpenRegistrations(): boolean | null {
  const [open, setOpen] = useState(() => isOpen(getCachedInstance()))
  useEffect(() => {
    if (open !== null) return
    let cancelled = false
    getInstance()
      .then((instance) => {
        if (!cancelled) setOpen(isOpen(instance))
      })
      .catch(() => {})
    return () => {
      cancelled = true
    }
  }, [open])
  return open
}
