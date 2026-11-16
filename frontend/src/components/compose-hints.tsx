import { TriangleAlert } from 'lucide-react'

import type { mastodon } from '../masto.ts'
import { t } from '@/i18n.tsx'
import { FormattedMessage } from 'react-intl'

/**
 * Advice, never a blocker.
 *
 * Mastodon 5.0 puts these between the text and the footer: things worth
 * knowing before you post that are not errors and must not stop you posting.
 * Each one names a consequence rather than a rule.

 */
export function ComposeHints({
  replyTo,
  attachments,
}: {
  replyTo?: mastodon.v1.Status | null
  attachments: mastodon.v1.MediaAttachment[]
}) {
  const hints: { key: string; body: React.ReactNode }[] = []

  if (replyTo?.visibility === 'private') {
    hints.push({
      key: 'followers-reply',
      body: (
        <>
          <FormattedMessage
            id="eunha.compose_hints.followers_reply"
            defaultMessage="You're replying to a followers-only post. People not following <b>@{acct}</b> might see your reply without the post it answers."
            values={{
              acct: replyTo.account.acct,
              b: (chunks) => <b className="font-semibold">{chunks}</b>,
            }}
          />
        </>
      ),
    })
  }

  const missingAlt = attachments.filter((a) => !a.description).length
  if (missingAlt > 0) {
    hints.push({
      key: 'missing-alt',
      body:
        missingAlt === 1 && attachments.length === 1
          ? t({ id: 'compose.hints.missing-alt', defaultMessage: 'Your attachment is missing alt text.' })
          : t({ id: 'compose.hints.missing-alt-many', defaultMessage: 'One or more of your attachments are missing alt text.' }),
    })
  }

  if (hints.length === 0) return null

  return (
    <div className="flex flex-col gap-1.5">
      {hints.map((hint) => (
        <p
          key={hint.key}
          className="text-muted-foreground bg-muted flex items-start gap-2 rounded-md px-2.5 py-2 text-xs"
        >
          <TriangleAlert className="mt-0.5 size-3.5 shrink-0" />
          <span>{hint.body}</span>
        </p>
      ))}
    </div>
  )
}
