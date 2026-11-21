import { useEffect, useRef, useState } from 'react'

import type { mastodon } from '../masto.ts'
import { getRelationship } from '../api.ts'

// The first check comes soon after the request is sent, since a remote server
// that accepts on its own usually answers within seconds. The checks then get
// further apart, so that a request left for days costs little.
const FIRST_CHECK_MS = 2_000
const LATEST_CHECK_MS = 60_000
// How long a request to an unlocked account counts as still being sent. Its
// server accepts on its own, so the answer is expected soon; past this, the
// request is left to wait like any other.
const SETTLING_MS = 30_000

/** Keeps a pending follow request's relationship up to date, so that a
 *  Requested button turns into Following once the request is accepted, or
 *  back into Follow once it is rejected. Nothing announces either, neither on
 *  the streaming API nor in Mastodon's, so this asks again while the request
 *  is pending: further apart each time, never while the tab is hidden, and
 *  once whenever the tab comes back into view.
 *
 *  Returns whether the request is settling: just sent to an unlocked account,
 *  whose server is expected to accept it by itself within seconds. A locked
 *  account's request waits on a person, so it never settles, and neither does
 *  one that was already pending when the page opened. */
export function usePendingFollow(
  relationship: mastodon.v1.Relationship | null | undefined,
  token: string | null | undefined,
  onChange: (relationship: mastodon.v1.Relationship) => void,
  locked: boolean | undefined,
): boolean {
  const pending = !!relationship && relationship.requested && !relationship.following
  const id = relationship?.id
  // The latest callback, so that a parent passing a new one on each render
  // does not restart the checks.
  const onChangeRef = useRef(onChange)
  useEffect(() => {
    onChangeRef.current = onChange
  })

  const [settling, setSettling] = useState(false)
  const wasPending = useRef(pending)
  useEffect(() => {
    const sent = pending && !wasPending.current
    wasPending.current = pending
    if (!pending) {
      setSettling(false)
      return
    }
    if (!sent || locked !== false) return
    setSettling(true)
    const timer = setTimeout(() => setSettling(false), SETTLING_MS)
    return () => clearTimeout(timer)
  }, [pending, locked])

  useEffect(() => {
    if (!pending || !id || !token) return
    let cancelled = false
    let timer: ReturnType<typeof setTimeout> | undefined
    let delay = FIRST_CHECK_MS
    const check = async () => {
      timer = undefined
      if (document.hidden) return
      try {
        const latest = await getRelationship(id, token)
        if (cancelled) return
        if (latest && (latest.following || !latest.requested)) {
          onChangeRef.current(latest)
          return
        }
      } catch {
        if (cancelled) return
      }
      delay = Math.min(delay * 2, LATEST_CHECK_MS)
      timer = setTimeout(check, delay)
    }
    const onVisible = () => {
      if (document.hidden) return
      clearTimeout(timer)
      void check()
    }
    timer = setTimeout(check, delay)
    document.addEventListener('visibilitychange', onVisible)
    return () => {
      cancelled = true
      clearTimeout(timer)
      document.removeEventListener('visibilitychange', onVisible)
    }
  }, [pending, id, token])

  return settling
}
