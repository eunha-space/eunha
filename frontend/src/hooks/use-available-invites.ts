import { useEffect, useState } from 'react'
import { useLocation } from 'react-router-dom'
import { getInvites, INVITES_CHANGED, type Invite } from '../eunha-api.ts'
import { availableInviteCount, inviteDate, inviteStatus } from '../lib/invites.ts'

export function useAvailableInvites(token: string | null): { count: number | null; hasAvailable: boolean } {
  const pathname = useLocation().pathname
  const [snapshot, setSnapshot] = useState<{
    token: string
    invites: Invite[]
  } | null>(null)
  const [, tick] = useState(0)
  useEffect(() => {
    if (!token) return
    let cancelled = false
    let sequence = 0
    const load = async () => {
      const request = ++sequence
      try {
        const invites = await getInvites(token)
        if (!cancelled && request === sequence) setSnapshot({ token, invites })
      } catch {
        if (!cancelled && request === sequence) setSnapshot(null)
      }
    }
    void load()
    const timer = window.setInterval(load, 60_000)
    window.addEventListener(INVITES_CHANGED, load)
    window.addEventListener('focus', load)
    return () => {
      cancelled = true
      window.clearInterval(timer)
      window.removeEventListener(INVITES_CHANGED, load)
      window.removeEventListener('focus', load)
    }
  }, [token, pathname])
  useEffect(() => {
    if (snapshot?.token !== token) return
    const expiries = snapshot.invites
      .flatMap((i) =>
        i.expires_at ? [inviteDate(i.expires_at).getTime()] : [],
      )
      .filter((t) => t > Date.now())
    if (!expiries.length) return
    const timer = window.setTimeout(
      () => tick((n) => n + 1),
      Math.min(
        2_147_483_647,
        Math.max(1, Math.min(...expiries) - Date.now() + 1),
      ),
    )
    return () => window.clearTimeout(timer)
  })
  const invites = token && snapshot?.token === token ? snapshot.invites : []
  return {
    count: availableInviteCount(invites),
    hasAvailable: invites.some(i => inviteStatus(i) === 'Available'),
  }
}
