import { TriangleAlert } from 'lucide-react'

import type { mastodon } from '../masto.ts'

/**
 * Advice, never a blocker.
 *
 * Mastodon 5.0 puts these between the text and the footer: things worth
 * knowing before you post that are not errors and must not stop you posting.
 * Each one names a consequence rather than a rule.
 *
 * Upstream has a fourth — a guess at the post's language disagreeing with the
 * picker. Eunha's composer has no language picker to disagree with, so there
 * is nothing to offer to change.
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
          You're replying to a followers-only post. People not following{' '}
          <b className="font-semibold">@{replyTo.account.acct}</b> might see your reply
          without the post it answers.
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
          ? 'Your attachment is missing alt text.'
          : 'One or more of your attachments are missing alt text.',
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
