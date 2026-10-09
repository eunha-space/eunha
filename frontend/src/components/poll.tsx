import { useState } from 'react'

import type { mastodon } from '../masto.ts'
import { votePoll } from '../api.ts'
import { Button } from '@/components/ui/button.tsx'
import { AnimateEmoji, EmojiText } from '@/components/emoji.tsx'

export function Poll({
  poll: initial,
  token,
  titles,
}: {
  poll: mastodon.v1.Poll
  token: string
  // The options as translated, shown in place of their own titles.
  titles?: string[]
}) {
  const [poll, setPoll] = useState(initial)
  const [choices, setChoices] = useState<number[]>([])
  const [busy, setBusy] = useState(false)

  const showResults = !!poll.voted || poll.expired || !token
  const total = poll.votesCount || 0
  // The poll's own emoji, which Mastodon's `Poll` draws its options with;
  // masto.js does not model them.
  const pollEmojis = (poll as { emojis?: mastodon.v1.CustomEmoji[] }).emojis

  const toggle = (i: number) => {
    setChoices((c) =>
      poll.multiple
        ? c.includes(i)
          ? c.filter((x) => x !== i)
          : [...c, i]
        : [i],
    )
  }

  const submit = async () => {
    if (!choices.length || busy) return
    setBusy(true)
    try {
      setPoll(await votePoll(poll.id, choices, token))
    } catch {
      // ignore
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="mt-2 space-y-2 text-sm">
      {poll.options.map((opt, i) => {
        const pct = total ? Math.round(((opt.votesCount ?? 0) / total) * 100) : 0
        const mine = poll.ownVotes?.includes(i)
        return showResults ? (
          <div
            key={i}
            className="relative overflow-hidden rounded-md border px-2 py-1"
          >
            <div
              className="bg-primary/25 absolute inset-y-0 left-0"
              style={{ width: `${pct}%` }}
            />
            <div className="relative flex justify-between gap-2">
              <AnimateEmoji as="span">
                {mine ? '✓ ' : ''}
                <EmojiText text={titles?.[i] ?? opt.title} emojis={pollEmojis} />
              </AnimateEmoji>
              <span className="text-muted-foreground">{pct}%</span>
            </div>
          </div>
        ) : (
          <label
            key={i}
            className="hover:bg-muted/50 flex cursor-pointer items-center gap-2 rounded-md border px-2 py-1"
          >
            <input
              type={poll.multiple ? 'checkbox' : 'radio'}
              name={`poll-${poll.id}`}
              checked={choices.includes(i)}
              onChange={() => toggle(i)}
            />
            <AnimateEmoji as="span">
              <EmojiText text={titles?.[i] ?? opt.title} emojis={pollEmojis} />
            </AnimateEmoji>
          </label>
        )
      })}
      <div className="text-muted-foreground flex items-center gap-3 text-xs">
        {!showResults && (
          <Button size="sm" disabled={busy || !choices.length} onClick={submit}>
            Vote
          </Button>
        )}
        <span>
          {poll.votesCount} vote{poll.votesCount === 1 ? '' : 's'}
          {poll.expired
            ? ' · closed'
            : poll.expiresAt
              ? ` · ends ${new Date(poll.expiresAt).toLocaleString()}`
              : ''}
        </span>
      </div>
    </div>
  )
}
