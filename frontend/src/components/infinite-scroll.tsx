import { useEffect, useRef } from 'react'

import { LinesSkeleton } from '@/components/skeletons.tsx'

// A sentinel that calls `onLoadMore` when scrolled near the viewport.
export function InfiniteScroll({
  onLoadMore,
  loading,
  done,
  hasItems,
}: {
  onLoadMore: () => void
  loading: boolean
  done: boolean
  hasItems: boolean
}) {
  const ref = useRef<HTMLDivElement>(null)
  const cb = useRef(onLoadMore)
  cb.current = onLoadMore

  useEffect(() => {
    const el = ref.current
    if (!el) return
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries[0]?.isIntersecting) cb.current()
      },
      { rootMargin: '400px' },
    )
    observer.observe(el)
    return () => observer.disconnect()
  }, [])

  return (
    <div ref={ref} className="text-muted-foreground py-4 text-center text-sm">
      {loading ? <LinesSkeleton count={2} className="px-3 sm:px-4" /> : done && hasItems ? 'End' : ''}
    </div>
  )
}
