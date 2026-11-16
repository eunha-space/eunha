import { useCallback, useState } from 'react'

import type { mastodon } from '../masto.ts'
import { authorizeFollowRequest, rejectFollowRequest } from '../api.ts'
import { Button } from '@/components/ui/button.tsx'
import { t } from '@/i18n.tsx'

export function FollowRequestActions({
  account,
  token,
  onResolved,
}: {
  account: mastodon.v1.Account
  token: string
  // Called after the request is authorized or rejected, with the account id.
  onResolved: (id: string) => void
}) {
  // `null` while idle; the verb string while its request is in flight.
  const [pending, setPending] = useState<'authorize' | 'reject' | null>(null)
  const [error, setError] = useState<string | null>(null)

  const run = useCallback(
    async (action: 'authorize' | 'reject') => {
      setPending(action)
      setError(null)
      try {
        if (action === 'authorize') await authorizeFollowRequest(account.id, token)
        else await rejectFollowRequest(account.id, token)
        onResolved(account.id)
      } catch (e) {
        setError(String(e))
        setPending(null)
      }
    },
    [account.id, token, onResolved],
  )

  return (
    <div className="flex items-center gap-2">
      {error && <span className="text-destructive text-xs">{error}</span>}
      <Button size="sm" disabled={pending !== null} onClick={() => run('authorize')}>
        {t({ id: 'notification_requests.accept', defaultMessage: 'Accept' })}
      </Button>
      <Button
        size="sm"
        variant="outline"
        disabled={pending !== null}
        onClick={() => run('reject')}
      >
        {t({ id: 'follow_request.reject', defaultMessage: 'Reject' })}
      </Button>
    </div>
  )
}
