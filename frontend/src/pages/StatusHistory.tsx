import { useEffect, useState } from 'react'
import { Link, useParams } from 'react-router-dom'

import type { mastodon } from '../masto.ts'
import { getStatusHistory } from '../api.ts'
import { getToken } from '../auth.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { MediaAttachments } from '@/components/media-attachments.tsx'
import { RelativeTime } from '@/components/relative-time.tsx'
import { Avatar, AvatarFallback, AvatarImage } from '@/components/ui/avatar.tsx'
import { useAnimatedImage } from '@/hooks/use-animated-image.ts'
import { AnimateEmoji, DisplayName, EmojiHtml, EmojiText } from '@/components/emoji.tsx'

function Version({
  edit,
  label,
}: {
  edit: mastodon.v1.StatusEdit
  label: string
}) {
  // A version of a poll carries its options' titles (`StatusEditSerializer`),
  // which masto.js does not model.
  const poll = (edit as { poll?: { options: { title: string }[] } | null }).poll
  return (
    <li className="rounded-md border px-3 py-3 sm:px-4">
      <div className="mb-1.5 flex items-baseline gap-2">
        <span className="text-sm font-semibold">{label}</span>
        <span className="text-muted-foreground text-xs">
          <RelativeTime value={edit.createdAt} />
        </span>
      </div>
      {edit.spoilerText && (
        <AnimateEmoji className="mb-1 text-sm font-medium">
          <EmojiText text={edit.spoilerText} emojis={edit.emojis} />
        </AnimateEmoji>
      )}
      <EmojiHtml
        className="status-content text-sm [&_a]:font-medium [&_a]:text-primary [&_a]:underline"
        html={edit.content}
        emojis={edit.emojis}
      />
      {poll && (
        // Mastodon's `CompareHistoryModal` lists the version's options as
        // they read then, each beside an empty choice, with no counts.
        <AnimateEmoji className="mt-2">
          <div role="list" aria-label="Poll options" className="space-y-1">
            {poll.options.map((option, i) => (
              <div role="listitem" key={`${option.title}-${i}`} className="flex items-center gap-2 text-sm">
                <span
                  aria-hidden
                  className="border-muted-foreground size-4 shrink-0 rounded-full border"
                />
                <span>
                  <EmojiText text={option.title} emojis={edit.emojis} />
                </span>
              </div>
            ))}
          </div>
        </AnimateEmoji>
      )}
      {edit.mediaAttachments.length > 0 && (
        <div className="mt-2">
          <MediaAttachments
            attachments={edit.mediaAttachments}
            sensitive={edit.sensitive}
          />
        </div>
      )}
    </li>
  )
}

export default function StatusHistory() {
  const { acct = '', id = '' } = useParams()
  const handle = acct.replace(/^@/, '')
  const token = getToken()

  const [edits, setEdits] = useState<mastodon.v1.StatusEdit[] | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    setEdits(null)
    setError(null)
    getStatusHistory(id, token ?? undefined)
      .then((v) => !cancelled && setEdits(v))
      .catch((e) => !cancelled && setError(String(e)))
    return () => {
      cancelled = true
    }
  }, [id, token])

  // The server returns versions oldest first and appends the current one, so
  // the last entry is what the post says now. Reversing puts the newest at the
  // top, the way every other list here reads — and makes "Current version" the
  // first thing on the page rather than something to scroll for.
  const newestFirst = edits ? [...edits].reverse() : null
  const author = edits?.[0]?.account
  const name = author ? author.displayName || author.username : handle
  const avatar = useAnimatedImage(author?.avatar, author?.avatarStatic)

  return (
    <div className="page-frame">
      <TopBar />
      <h1 className="text-lg font-bold">Edit history</h1>
      <div className="text-muted-foreground mb-3 flex items-center gap-2 text-sm">
        {author && (
          <Avatar className="size-5" {...avatar.hover}>
            <AvatarImage src={avatar.src} alt="" />
            <AvatarFallback>{name.slice(0, 1).toUpperCase()}</AvatarFallback>
          </Avatar>
        )}
        <Link
          to={`/@${handle}/${id}`}
          className="text-foreground no-underline hover:underline"
        >
          {author ? <DisplayName account={author} /> : handle}’s post
        </Link>
      </div>

      {error && <p className="text-destructive text-sm">{error}</p>}
      {edits === null && !error && (
        <p className="text-muted-foreground text-sm">Loading…</p>
      )}
      {newestFirst && (
        <ol className="space-y-2">
          {newestFirst.map((edit, i) => (
            <Version
              key={`${edit.createdAt}-${i}`}
              edit={edit}
              label={
                i === 0
                  ? 'Current version'
                  : i === newestFirst.length - 1
                    ? 'Original'
                    : `Version ${newestFirst.length - i}`
              }
            />
          ))}
        </ol>
      )}
      {newestFirst?.length === 0 && (
        <p className="text-muted-foreground text-sm">
          This post has no recorded history.
        </p>
      )}
    </div>
  )
}
