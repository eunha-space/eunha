import { useEffect, useId, useState, useSyncExternalStore } from 'react'
import { MoreHorizontal } from 'lucide-react'

import { getMeId } from '../me.ts'
import { Button } from '@/components/ui/button.tsx'

// Mastodon's `RemoveQuoteHint`: on a notification of someone quoting you, it
// says once where the quote can be taken off your post, then never again.
// Mastodon keeps the dismissal among the account's web settings
// (`dismissed_banners`); eunha keeps it in the browser, per account.
const KEY = 'eunha:dismissed:notifications/remove_quote_hint'

function storageKey(): string {
  return `${KEY}:${getMeId() ?? ''}`
}

function wasDismissed(): boolean {
  try {
    return localStorage.getItem(storageKey()) === '1'
  } catch {
    return false
  }
}

const listeners = new Set<() => void>()

function subscribe(listener: () => void) {
  listeners.add(listener)
  return () => {
    listeners.delete(listener)
  }
}

// Opening a post's menu dismisses the hint too, as Mastodon's
// `StatusActionMenu` does: whoever opens one has found it.
export function dismissRemoveQuoteHint() {
  if (wasDismissed()) return
  try {
    localStorage.setItem(storageKey(), '1')
  } catch {
    // The hint comes back next time; nothing else depends on it.
  }
  for (const listener of listeners) listener()
}

// Only the first hint drawn is shown, so a page of quote notifications
// carries one rather than one each.
let firstHintId: string | null = null

export function RemoveQuoteHint({ canShow }: { canShow: boolean }) {
  const dismissed = useSyncExternalStore(subscribe, wasDismissed)
  const id = useId()
  const [isOnlyHint, setIsOnlyHint] = useState(false)
  const shouldShow = canShow && !dismissed

  useEffect(() => {
    if (!shouldShow) return
    if (!firstHintId) {
      firstHintId = id
      setIsOnlyHint(true)
    }
    return () => {
      if (firstHintId === id) {
        firstHintId = null
        setIsOnlyHint(false)
      }
    }
  }, [shouldShow, id])

  if (!shouldShow || !isOnlyHint) return null

  return (
    <div role="note" className="bg-muted/50 space-y-1 rounded-md border p-2.5 text-sm">
      <p className="font-medium">Want to remove your quoted post?</p>
      <p className="text-muted-foreground">
        You can do so from the{' '}
        <MoreHorizontal aria-label="More" className="inline size-4 align-text-bottom" />{' '}
        options menu.
      </p>
      <Button variant="ghost" size="sm" onClick={dismissRemoveQuoteHint}>
        Got it
      </Button>
    </div>
  )
}
