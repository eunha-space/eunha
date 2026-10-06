import { useLayoutEffect } from 'react'
import { Outlet, useLocation, useNavigationType } from 'react-router-dom'
import { getToken } from '../auth.ts'

const positions = new Map<string, number>()

export function NavigationScroll() {
  const location = useLocation()
  const navigationType = useNavigationType()
  const key = JSON.stringify([getToken(), location.key])

  useLayoutEffect(() => {
    const previous = history.scrollRestoration
    history.scrollRestoration = 'manual'
    return () => { history.scrollRestoration = previous }
  }, [])

  useLayoutEffect(() => {
    const target = navigationType === 'POP' ? positions.get(key) ?? 0 : 0
    let restoring = target > 0
    let lastPosition = target
    // Profile headers and feed pages arrive asynchronously. Retry as their
    // layout grows, and yield immediately when the reader interacts.
    const restore = () => {
      if (window.scrollY !== target) window.scrollTo({ top: target, behavior: 'instant' })
    }
    const observer = new ResizeObserver(() => { if (restoring) restore() })
    const stop = () => {
      restoring = false
      observer.disconnect()
      lastPosition = window.scrollY
    }
    const record = () => {
      if (!restoring) lastPosition = window.scrollY
    }
    restore()
    if (restoring) observer.observe(document.body)
    const timer = window.setTimeout(stop, 10_000)
    window.addEventListener('scroll', record, { passive: true })
    const events = ['wheel', 'touchstart', 'pointerdown', 'keydown'] as const
    for (const event of events) window.addEventListener(event, stop, { passive: true })
    return () => {
      positions.delete(key)
      positions.set(key, lastPosition)
      if (positions.size > 30) positions.delete(positions.keys().next().value!)
      window.clearTimeout(timer)
      observer.disconnect()
      window.removeEventListener('scroll', record)
      for (const event of events) window.removeEventListener(event, stop)
    }
  }, [key, navigationType])

  return <Outlet />
}
