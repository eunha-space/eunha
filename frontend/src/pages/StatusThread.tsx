import { useCallback, useEffect, useState } from 'react'
import { useParams } from 'react-router-dom'

import type { mastodon } from '../masto.ts'
import { getStatus, getStatusContext } from '../api.ts'
import { getToken } from '../auth.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { StatusCard } from '@/components/status-card.tsx'
import { TimelineStack } from '@/components/timeline-stack.tsx'
import { useComposeModal } from '@/components/compose-modal.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Pencil } from 'lucide-react'

export default function StatusThread() {
  const { id = '' } = useParams()
  const token = getToken()
  const { openCompose } = useComposeModal()

  const [status, setStatus] = useState<mastodon.v1.Status | null>(null)
  const [context, setContext] = useState<mastodon.v1.Context | null>(null)
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(() => {
    getStatus(id, token ?? undefined).then(setStatus).catch((e) => setError(String(e)))
    getStatusContext(id, token ?? undefined).then(setContext).catch(() => {})
  }, [id, token])

  useEffect(() => {
    setStatus(null)
    setContext(null)
    setError(null)
    load()
  }, [load])

  const render = (s: mastodon.v1.Status, detailed = false) => (
    <StatusCard
      key={s.id}
      status={s.reblog ?? s}
      token={token ?? ''}
      filterContext="thread"
      boostedBy={s.reblog ? s.account : undefined}
      detailed={detailed}
      onReply={(replyTo) =>
        openCompose({
          replyTo,
          onPosted: () => load(),
        })
      }
    />
  )

  return (
    <div className="page-frame">
      <TopBar />
      {error && <p className="text-destructive text-sm">{error}</p>}

      <div className="space-y-2">
        {!!context?.ancestors.length && (
          <TimelineStack>{context.ancestors.map((s) => render(s))}</TimelineStack>
        )}

        {status && (
          <div className="ring-primary/40 overflow-hidden rounded-md border ring-2">
            {render(status, true)}
          </div>
        )}

        {token && status && (
          <Button
            className="w-full"
            variant="outline"
            onClick={() => openCompose({ replyTo: status, onPosted: () => load() })}
          >
            <Pencil /> Reply
          </Button>
        )}

        {!!context?.descendants.length && (
          <TimelineStack>{context.descendants.map((s) => render(s))}</TimelineStack>
        )}
      </div>
    </div>
  )
}
