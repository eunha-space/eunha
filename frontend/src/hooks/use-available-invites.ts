import { useEffect, useState } from 'react'
import { getInvites, INVITES_CHANGED, type Invite } from '../eunha-api.ts'

function expiry(invite: Invite): number {
  if (!invite.expires_at) return Infinity
  const value = invite.expires_at
  return new Date(/(?:Z|[+-]\d{2}:\d{2})$/.test(value) ? value : `${value}Z`).getTime()
}

/** One shared count for the desktop rail and mobile drawer, refreshed on mutations. */
export function useAvailableInvites(token: string | null): number {
  const [loaded, setLoaded] = useState<{ token: string; invites: Invite[] } | null>(null)
  const [now, setNow] = useState(Date.now)

  useEffect(() => {
    if (!token) return
    let active = true
    let revision = 0
    const refresh = () => {
      const request = ++revision
      getInvites(token).then(invites => {
        if (active && request === revision) {
          setLoaded({ token, invites })
          setNow(Date.now())
        }
      }).catch(() => {
        if (active && request === revision) setLoaded(null)
      })
    }
    refresh()
    window.addEventListener(INVITES_CHANGED, refresh)
    return () => {
      active = false
      window.removeEventListener(INVITES_CHANGED, refresh)
    }
  }, [token])

  useEffect(() => {
    if (loaded?.token !== token) return
    const next = loaded.invites.reduce((soonest, invite) => {
      const time = expiry(invite)
      return invite.valid_for_use && time > now ? Math.min(soonest, time) : soonest
    }, Infinity)
    if (!Number.isFinite(next)) return
    const timer = setTimeout(() => setNow(Date.now()), Math.min(next - Date.now() + 1, 2_147_483_647))
    return () => clearTimeout(timer)
  }, [loaded, token, now])

  if (loaded?.token !== token) return 0
  return loaded.invites.filter(invite => invite.valid_for_use && !invite.expired
    && (invite.max_uses === null || invite.uses < invite.max_uses) && expiry(invite) > now).length
}
