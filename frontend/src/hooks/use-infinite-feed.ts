import { useCallback, useEffect, useRef, useState, type DependencyList } from 'react'
import { useLocation, useNavigationType } from 'react-router-dom'
import { getToken } from '../auth.ts'

const savedFeeds = new Map<string, { items: { id: string }[]; done: boolean }>()

/**
 * Cursor-paginated feed backed by Mastodon's `max_id` pagination. `fetchPage`
 * returns one page; passing the last item's id fetches older items. The feed
 * resets and reloads whenever `deps` change.
 */
export function useInfiniteFeed<T extends { id: string }>(
  fetchPage: (maxId?: string) => Promise<T[]>,
  deps: DependencyList,
  restorationId?: string,
) {
  const location = useLocation()
  const navigationType = useNavigationType()
  const cacheKey = restorationId
    ? JSON.stringify([location.key, getToken(), restorationId, deps])
    : null
  const restored = navigationType === 'POP' && cacheKey ? savedFeeds.get(cacheKey) : undefined
  const [items, setItems] = useState<T[] | null>(() => (restored?.items as T[] | undefined) ?? null)
  const [error, setError] = useState<string | null>(null)
  const [loadingMore, setLoadingMore] = useState(false)
  const [done, setDone] = useState(restored?.done ?? false)

  const fetchRef = useRef(fetchPage)
  fetchRef.current = fetchPage
  const itemsRef = useRef<T[] | null>(null)
  itemsRef.current = items
  const busyRef = useRef(false)
  const runRef = useRef(0)

  const latest = useRef({ items, done })
  latest.current = { items, done }
  useEffect(() => () => {
    if (!cacheKey || !latest.current.items) return
    savedFeeds.delete(cacheKey)
    savedFeeds.set(cacheKey, { items: latest.current.items, done: latest.current.done })
    if (savedFeeds.size > 30) savedFeeds.delete(savedFeeds.keys().next().value!)
  }, [cacheKey])

  useEffect(() => {
    const run = ++runRef.current
    setLoadingMore(false)
    const saved = navigationType === 'POP' && cacheKey ? savedFeeds.get(cacheKey) : undefined
    if (saved) {
      setItems(saved.items as T[])
      setError(null)
      setDone(saved.done)
      busyRef.current = false
      return () => { runRef.current++ }
    }
    let cancelled = false
    setItems(null)
    setError(null)
    setDone(false)
    busyRef.current = true
    fetchRef
      .current()
      .then((page) => {
        if (cancelled) return
        setItems(page)
        setDone(page.length === 0)
      })
      .catch((e) => {
        if (!cancelled) setError(String(e))
      })
      .finally(() => {
        if (!cancelled) busyRef.current = false
      })
    return () => {
      if (run === runRef.current) runRef.current++
      cancelled = true
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...deps, cacheKey])

  const loadMore = useCallback(async () => {
    const cur = itemsRef.current
    if (busyRef.current || done || !cur || cur.length === 0) return
    const run = runRef.current
    busyRef.current = true
    setLoadingMore(true)
    try {
      const page = await fetchRef.current(cur[cur.length - 1].id)
      if (run !== runRef.current) return
      if (page.length === 0) setDone(true)
      else setItems((prev) => [...(prev ?? []), ...page])
    } catch (e) {
      if (run === runRef.current) setError(String(e))
    } finally {
      if (run === runRef.current) {
        busyRef.current = false
        setLoadingMore(false)
      }
    }
  }, [done])

  const prepend = useCallback(
    (item: T) =>
      setItems((prev) =>
        prev?.some((existing) => existing.id === item.id)
          ? prev
          : [item, ...(prev ?? [])],
      ),
    [],
  )

  const mutate = useCallback((fn: (items: T[]) => T[]) => {
    setItems((prev) => (prev ? fn(prev) : prev))
  }, [])

  return { items, error, loadingMore, done, loadMore, prepend, mutate }
}
