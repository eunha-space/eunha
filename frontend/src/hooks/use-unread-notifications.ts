import { useEffect, useState } from 'react'
import { useLocation } from 'react-router-dom'

import { getNotificationsUnreadCount } from '../api.ts'

const POLL_MS = 60_000
let cachedCount: { token: string; count: number } | null = null

/**
 * How many notifications have arrived since the reader last marked the
 * timeline, for the badge in the nav.
 *
 * The count is derived from the notifications marker, which the notifications
 * page moves when it loads — so this refetches on every navigation as well as
 * on a slow timer. Reading the page is what clears the badge, and a reader who
 * has just done that should not have to wait out the interval to see it go.
 */
export function useUnreadNotifications(token: string | null): number {
  const pathname = useLocation().pathname
  const [snapshot, setSnapshot] = useState(() =>
    cachedCount?.token === token ? cachedCount : null,
  )

  useEffect(() => {
    if (!token) {
      cachedCount = null
      setSnapshot(null)
      return
    }
    let cancelled = false
    // The marker is moved by the notifications page after it renders, so a
    // read taken the instant we land there would still see the old one.
    const delay = pathname === '/notifications' ? 1_200 : 0
    let sequence = 0
    const load = () => {
      const request = ++sequence
      return getNotificationsUnreadCount(token)
        .then((count) => {
          if (cancelled || request !== sequence) return
          cachedCount = { token, count }
          setSnapshot(cachedCount)
        })
        .catch(() => {})
    }

    const first = window.setTimeout(load, delay)
    const timer = window.setInterval(load, POLL_MS)
    return () => {
      cancelled = true
      window.clearTimeout(first)
      window.clearInterval(timer)
    }
  }, [token, pathname])

  return token && snapshot?.token === token ? snapshot.count : 0
}
