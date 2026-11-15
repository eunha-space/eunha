import { useEffect, useState } from 'react'
import { Navigate, useLocation } from 'react-router-dom'

import { getCachedInstance, getInstance } from '../api.ts'
import { getToken } from '../auth.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { ColumnHeader } from '@/components/column-header.tsx'
import { StatusFeed } from '@/components/status-feed.tsx'

/**
 * `remote_live_feed_access` from the instance, or `undefined` until it has
 * loaded. A failed load answers `public`, leaving the server to refuse the
 * feed itself.
 */
function useRemoteFeedAccess(): string | undefined {
  const read = () =>
    (getCachedInstance()?.configuration as
      | { timelinesAccess?: { liveFeeds?: { remote?: string } } }
      | undefined)?.timelinesAccess?.liveFeeds?.remote
  const [access, setAccess] = useState(() => (getCachedInstance() ? read() ?? 'public' : undefined))
  useEffect(() => {
    if (access !== undefined) return
    let cancelled = false
    getInstance()
      .then(() => !cancelled && setAccess(read() ?? 'public'))
      .catch(() => !cancelled && setAccess('public'))
    return () => {
      cancelled = true
    }
  }, [access])
  return access
}

// `/local` and `/public` are the same page over two feeds, chosen by the path.
// `local` is also accepted as a prop, because "/" renders this page for a
// signed-out visitor and there is no path to read it from there.
export default function PublicTimeline({ local }: { local?: boolean } = {}) {
  const pathname = useLocation().pathname
  const isLocal = local ?? pathname === '/local'
  const token = getToken()
  const visitorOnFederated = !isLocal && !token
  const remoteAccess = useRemoteFeedAccess()

  // A visitor may read the federated timeline only when it is `public`
  // (eunha's default is `authenticated`); otherwise "/" is their front door.
  if (visitorOnFederated && remoteAccess !== undefined && remoteAccess !== 'public') {
    return <Navigate to="/" replace />
  }

  return (
    <>
      <TopBar />
      <div className="column-frame">
        <ColumnHeader title={isLocal ? 'Local' : 'Federated'} />
        <div className="space-y-2 p-3">
          {!(visitorOnFederated && remoteAccess === undefined) && (
            <StatusFeed kind={isLocal ? 'local' : 'public'} token={token} />
          )}
        </div>
      </div>
    </>
  )
}
