import { useEffect, useId, useState, useSyncExternalStore } from 'react'
import { MoreHorizontal } from 'lucide-react'

import { dismiss, isDismissed, subscribeDismissed } from '@/lib/dismissed.ts'
import { Button } from '@/components/ui/button.tsx'

// Mastodon's `RemoveQuoteHint`: on a notification of someone quoting you, it
// says once where the quote can be taken off your post, then never again.
const HINT_ID = 'notifications/remove_quote_hint'

const wasDismissed = () => isDismissed(HINT_ID)

// Opening a post's menu dismisses the hint too, as Mastodon's
// `StatusActionMenu` does: whoever opens one has found it.
export function dismissRemoveQuoteHint() {
  dismiss(HINT_ID)
}

// Only the first hint drawn is shown, so a page of quote notifications
// carries one rather than one each.
let firstHintId: string | null = null

export function RemoveQuoteHint({ canShow }: { canShow: boolean }) {
  const dismissed = useSyncExternalStore(subscribeDismissed, wasDismissed)
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
