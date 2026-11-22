import { useState } from 'react'
import { Link } from 'react-router-dom'
import { toast } from 'sonner'
import { ExternalLink, Flag, MoreHorizontal, Pencil, Reply, Star, Trash2 } from 'lucide-react'

import type { mastodon } from '../masto.ts'
import { deleteStatus, getStatusSource, setFavourite, updateStatus } from '../api.ts'
import { getMeId } from '../me.ts'
import { getActiveAccountId } from '../auth.ts'
import { useReadingPreferences } from '../reading-preferences.ts'
import { useAnimatedImage } from '@/hooks/use-animated-image.ts'
import { Avatar, AvatarFallback, AvatarImage } from '@/components/ui/avatar.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Textarea } from '@/components/ui/textarea.tsx'
import { PostLanguagePicker } from '@/components/post-language-picker.tsx'
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu.tsx'
import { MediaAttachments } from '@/components/media-attachments.tsx'
import { Poll } from '@/components/poll.tsx'
import { PreviewCard } from '@/components/preview-card.tsx'
import { QuotedStatus } from '@/components/quoted-post.tsx'
import { ReportDialog } from '@/components/report-dialog.tsx'
import { AnimateEmoji, EmojiText } from '@/components/emoji.tsx'
import { stripQuoteFallback } from '@/lib/content.ts'
import { cn, errorMessage } from '@/lib/utils.ts'
import { PostContent } from '@/components/post-content.tsx'
import { currentLocale, t } from '@/i18n.tsx'

// The reader's account. `getMeId` is cached from the last `loadMe`; on a first
// visit it is not there yet, and the active account's id is the same id.
export const readerId = () => getMeId() ?? getActiveAccountId()

// A private mention opens with the handles of everyone it goes to, which in a
// chat bubble only repeats the header. The mentions at the very start of the
// first paragraph come off; one in the middle of a sentence is part of what
// was said, and stays. A message that is nothing but mentions keeps them,
// rather than becoming an empty bubble.
export function stripLeadingMentions(html: string): string {
  const root = document.createElement('div')
  root.innerHTML = html
  const first = root.firstElementChild
  if (!first || first.tagName !== 'P') return html
  const isMention = (node: ChildNode) =>
    node instanceof HTMLElement &&
    (node.classList.contains('h-card') || (node.tagName === 'A' && node.classList.contains('mention') && !node.classList.contains('hashtag')))
  let stripped = false
  while (first.firstChild) {
    const node = first.firstChild
    if (node.nodeType === Node.TEXT_NODE && !node.textContent?.trim()) {
      node.remove()
    } else if (isMention(node)) {
      node.remove()
      stripped = true
    } else {
      break
    }
  }
  if (!stripped) return html
  if (first.firstChild?.nodeType === Node.TEXT_NODE) {
    first.firstChild.textContent = first.firstChild.textContent!.replace(/^\s+/, '')
  }
  if (!first.textContent?.trim() && !first.querySelector('img')) first.remove()
  if (!root.textContent?.trim() && !root.querySelector('img')) return html
  return root.innerHTML
}

/**
 * One message in a private-mention thread drawn as a chat bubble: yours on
 * the right in the primary colour, everyone else's on the left.
 *
 * A bubble carries what a status card's body does — content warning, media,
 * poll, link card, quote — but its actions sit beside it on hover (always, on
 * a touch screen) instead of in a bar under every message. Boosting is not
 * among them: a private mention cannot be boosted.
 */
export function MessageBubble({
  status: initial,
  token,
  focused,
  onReply,
}: {
  status: mastodon.v1.Status
  token: string
  // The post the thread's URL names, when it is not simply the newest.
  focused?: boolean
  onReply: (status: mastodon.v1.Status) => void
}) {
  const [status, setStatus] = useState(initial)
  const [deleted, setDeleted] = useState(false)
  const [busy, setBusy] = useState(false)
  const [reporting, setReporting] = useState(false)
  const { expandSpoilers } = useReadingPreferences()
  const [expanded, setExpanded] = useState(expandSpoilers || !initial.spoilerText)
  const [editing, setEditing] = useState(false)
  const [editText, setEditText] = useState('')
  const [editSpoiler, setEditSpoiler] = useState('')
  const [editLanguage, setEditLanguage] = useState('')
  const [saving, setSaving] = useState(false)
  const [statusBefore, setStatusBefore] = useState(initial)
  if (statusBefore !== initial) {
    setStatusBefore(initial)
    setStatus(initial)
  }

  if (deleted) return null

  const mine = !!token && status.account.id === readerId()
  const remoteUrl = status.account.acct.includes('@') ? status.url : null
  const threadPath = `/@${status.account.acct}/${status.id}`
  const content = stripLeadingMentions(status.quote ? stripQuoteFallback(status.content) : status.content)
  const hasText = !!content.replace(/<[^>]*>/g, '').trim()

  const favourite = async () => {
    if (busy || !token) return
    setBusy(true)
    try {
      setStatus(await setFavourite(token, status.id, !status.favourited))
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  // Edits the source text, leading mentions included: they are who the
  // message goes to, and dropping one would drop that person. Media and the
  // poll are left alone — an edit that names neither keeps both.
  const startEdit = async () => {
    try {
      const src = await getStatusSource(status.id, token)
      setEditText(src.text)
      setEditSpoiler(src.spoilerText)
      setEditLanguage(status.language ?? '')
      setEditing(true)
    } catch (e) {
      toast.error(errorMessage(e))
    }
  }

  const saveEdit = async () => {
    if (saving) return
    setSaving(true)
    try {
      const updated = await updateStatus(
        status.id,
        { status: editText, spoilerText: editSpoiler, language: editLanguage || undefined },
        token,
      )
      setStatus(updated)
      setExpanded(expandSpoilers || !updated.spoilerText)
      setEditing(false)
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setSaving(false)
    }
  }

  const remove = async () => {
    if (!window.confirm(t({ id: 'eunha.status_card.delete_this_post', defaultMessage: 'Delete this post?' }))) return
    try {
      await deleteStatus(status.id, token)
      setDeleted(true)
    } catch (e) {
      toast.error(errorMessage(e))
    }
  }

  const actions = token && (
    <div
      className={cn(
        'flex shrink-0 items-center self-center opacity-0 transition-opacity',
        'group-focus-within:opacity-100 group-hover:opacity-100 pointer-coarse:opacity-100',
        mine && 'flex-row-reverse',
      )}
    >
      <Button
        variant="ghost"
        size="icon-sm"
        aria-label={t({ id: 'status.reply', defaultMessage: 'Reply' })}
        onClick={() => onReply(status)}
      >
        <Reply />
      </Button>
      <Button
        variant="ghost"
        size="icon-sm"
        aria-label={t({ id: 'eunha.status_card.favourite', defaultMessage: 'Favourite' })}
        aria-pressed={status.favourited ?? false}
        disabled={busy}
        onClick={() => void favourite()}
        className={cn(status.favourited && 'text-yellow-500')}
      >
        <Star />
      </Button>
      <DropdownMenu>
        <DropdownMenuTrigger
          render={<Button variant="ghost" size="icon-sm" aria-label={t({ id: 'navigation_bar.more', defaultMessage: 'More' })} />}
        >
          <MoreHorizontal />
        </DropdownMenuTrigger>
        <DropdownMenuContent align={mine ? 'end' : 'start'}>
          {remoteUrl && (
            <DropdownMenuItem render={<a href={remoteUrl} target="_blank" rel="noopener noreferrer" />}>
              <ExternalLink /> {t({ id: 'account.open_original_page', defaultMessage: 'Open original page' })}
            </DropdownMenuItem>
          )}
          {!mine && (
            <DropdownMenuItem variant="destructive" onClick={() => setReporting(true)}>
              <Flag /> {t({ id: 'eunha.status_card.report_post', defaultMessage: 'Report post' })}
            </DropdownMenuItem>
          )}
          {mine && (
            <DropdownMenuItem onClick={() => void startEdit()}>
              <Pencil /> {t({ id: 'status.edit', defaultMessage: 'Edit' })}
            </DropdownMenuItem>
          )}
          {mine && (
            <DropdownMenuItem variant="destructive" onClick={() => void remove()}>
              <Trash2 /> {t({ id: 'confirmations.delete.confirm', defaultMessage: 'Delete' })}
            </DropdownMenuItem>
          )}
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  )

  if (editing) {
    return (
      <form
        className="ml-auto w-full max-w-[80%] space-y-2"
        onSubmit={(e) => {
          e.preventDefault()
          void saveEdit()
        }}
      >
        <Input
          value={editSpoiler}
          onChange={(e) => setEditSpoiler(e.target.value)}
          placeholder={t({ id: 'eunha.status_card.content_warning_optional', defaultMessage: 'Content warning (optional)' })}
        />
        <Textarea
          value={editText}
          onChange={(e) => setEditText(e.target.value)}
          rows={3}
          className="resize-y"
          autoFocus
        />
        <div className="flex justify-end gap-2">
          <PostLanguagePicker value={editLanguage} onChange={setEditLanguage} disabled={saving} />
          <Button type="button" size="sm" variant="ghost" onClick={() => setEditing(false)}>
            {t({ id: 'confirmation_modal.cancel', defaultMessage: 'Cancel' })}
          </Button>
          <Button type="submit" size="sm" disabled={saving || !editText.trim()}>
            {t({ id: 'lists.save', defaultMessage: 'Save' })}
          </Button>
        </div>
      </form>
    )
  }

  return (
    <div className={cn('group flex items-end gap-1', mine && 'flex-row-reverse')}>
      <div
        data-message={status.id}
        className={cn(
          'min-w-0 max-w-[80%] space-y-2 rounded-2xl px-3 py-2 text-sm',
          mine
            ? 'bg-primary text-primary-foreground [&_a]:text-primary-foreground rounded-br-md [&_a]:underline'
            : 'bg-muted rounded-bl-md [&_a]:text-primary [&_a]:underline',
          focused && 'ring-ring ring-2 ring-offset-2 ring-offset-background',
        )}
      >
        {status.spoilerText && (
          <AnimateEmoji className={cn('text-xs', mine ? 'text-primary-foreground/80' : 'text-muted-foreground')}>
            <span>
              <EmojiText text={status.spoilerText} emojis={status.emojis} />
            </span>
            <button
              type="button"
              onClick={() => setExpanded((e) => !e)}
              className="ml-2 font-medium underline"
            >
              {expanded ? t({ id: 'block_modal.show_less', defaultMessage: 'Show less' }) : t({ id: 'content_warning.show_more', defaultMessage: 'Show more' })}
            </button>
          </AnimateEmoji>
        )}
        {expanded && (
          <>
            {/* As in a chat, a picture comes first and its words under it. */}
            {status.mediaAttachments.length > 0 && (
              <MediaAttachments attachments={status.mediaAttachments} sensitive={status.sensitive} />
            )}
            {hasText && (
              <PostContent
                key={status.id}
                buttonClassName="text-inherit"
                className="status-content rich-text break-words [&_a]:font-medium"
                html={content}
                emojis={status.emojis}
              />
            )}
            {status.poll && <Poll poll={status.poll} token={token} />}
            {status.mediaAttachments.length === 0 && !status.quote && status.card && (
              <PreviewCard card={status.card} sensitive={status.sensitive} />
            )}
            {status.quote && <QuotedStatus quote={status.quote} token={token} filterContext="thread" />}
          </>
        )}
        {status.editedAt && (
          <Link
            to={`${threadPath}/history`}
            className={cn('block text-xs no-underline hover:underline', mine ? 'text-primary-foreground/70' : 'text-muted-foreground')}
          >
            {t({ id: 'eunha.status_card.edited', defaultMessage: '(edited)' })}
          </Link>
        )}
      </div>
      {actions}
      {!mine && token && (
        <ReportDialog
          account={status.account}
          status={status}
          open={reporting}
          onOpenChange={setReporting}
          token={token}
        />
      )}
    </div>
  )
}

// Who a private mention goes to: its author and everyone it mentions. A
// thread reads as one conversation only while every message in it goes to
// the same people — Mastodon has no group to belong to, and a reply that
// leaves a mention out leaves that person out of the reply.
export function audienceKey(status: mastodon.v1.Status): string {
  return [...new Set([status.account.id, ...status.mentions.map((m) => m.id)])].sort().join(',')
}

export function isOneConversation(messages: mastodon.v1.Status[]): boolean {
  return (
    messages.length > 0 &&
    messages.every((m) => m.visibility === 'direct' && audienceKey(m) === audienceKey(messages[0]!))
  )
}

function SenderAvatar({ account }: { account: mastodon.v1.Account }) {
  const avatar = useAnimatedImage(account.avatar, account.avatarStatic)
  const name = account.displayName || account.username
  return (
    <Link to={`/@${account.acct}`} className="shrink-0" {...avatar.hover} aria-label={name}>
      <Avatar className="size-7">
        <AvatarImage src={avatar.src} alt="" />
        <AvatarFallback>{name.slice(0, 1).toUpperCase()}</AvatarFallback>
      </Avatar>
    </Link>
  )
}

const dayKey = (iso: string) => new Date(iso).toDateString()

function dayLabel(iso: string): string {
  const day = new Date(iso)
  const today = new Date()
  const days = Math.round(
    (new Date(today.toDateString()).getTime() - new Date(day.toDateString()).getTime()) / 86_400_000,
  )
  if (days >= 0 && days < 2) {
    return new Intl.RelativeTimeFormat(currentLocale(), { numeric: 'auto' }).format(-days, 'day')
  }
  return new Intl.DateTimeFormat(currentLocale(), {
    dateStyle: 'medium',
  }).format(day)
}

const timeLabel = (iso: string) =>
  new Intl.DateTimeFormat(currentLocale(), { timeStyle: 'short' }).format(new Date(iso))

// Messages from one person a few minutes apart sit together as one run, with
// one name above and one avatar and time below, as chat windows draw them.
const RUN_GAP_MS = 5 * 60_000

function excerpt(html: string): string {
  const el = document.createElement('div')
  el.innerHTML = stripLeadingMentions(html)
  return (el.textContent ?? '').replace(/\s+/g, ' ').trim()
}

/**
 * A private-mention thread as a chat: oldest at the top, runs of bubbles by
 * sender, a line where the day changes. Only for a thread
 * `isOneConversation` accepts; one whose audience changes partway is drawn
 * as posts instead, because bubbles would claim a room nobody is in.
 */
export function MessageList({
  messages,
  token,
  focusedId,
  onReply,
}: {
  messages: mastodon.v1.Status[]
  token: string
  focusedId?: string
  onReply: (status: mastodon.v1.Status) => void
}) {
  const me = readerId()
  // Names above runs only help when more than one other person talks.
  const group = new Set(audienceKey(messages[0]!).split(',')).size > 2
  const byId = new Map(messages.map((m) => [m.id, m]))

  return (
    <div className="space-y-0.5">
      {messages.map((m, i) => {
        const prev = messages[i - 1]
        const next = messages[i + 1]
        const newDay = !prev || dayKey(prev.createdAt) !== dayKey(m.createdAt)
        const sameRun = (a?: mastodon.v1.Status, b?: mastodon.v1.Status) =>
          !!a && !!b &&
          a.account.id === b.account.id &&
          dayKey(a.createdAt) === dayKey(b.createdAt) &&
          Math.abs(Date.parse(b.createdAt) - Date.parse(a.createdAt)) < RUN_GAP_MS
        const runStart = newDay || !sameRun(prev, m)
        const runEnd = !sameRun(m, next) || (!!next && dayKey(next.createdAt) !== dayKey(m.createdAt))
        const mine = m.account.id === me
        // A reply to something other than the message just above it says
        // what it answers, since the thread is drawn flat.
        const repliedTo = m.inReplyToId && m.inReplyToId !== prev?.id ? byId.get(m.inReplyToId) : undefined

        return (
          <div key={m.id}>
            {newDay && (
              <p className="text-muted-foreground py-3 text-center text-xs first-letter:uppercase">
                {dayLabel(m.createdAt)}
              </p>
            )}
            {runStart && group && !mine && (
              <p className="text-muted-foreground mt-3 mb-0.5 ml-9 text-xs">
                <EmojiText text={m.account.displayName || m.account.username} emojis={m.account.emojis} />
              </p>
            )}
            {repliedTo && (
              <p
                className={cn(
                  'text-muted-foreground mt-2 mb-0.5 flex max-w-[80%] items-center gap-1 truncate text-xs',
                  mine ? 'ml-auto justify-end' : 'ml-9',
                )}
              >
                <Reply className="size-3 shrink-0" />
                <span className="truncate">
                  {(repliedTo.account.id === me
                    ? t({ id: 'eunha.messages.you', defaultMessage: 'You' })
                    : repliedTo.account.displayName || repliedTo.account.username) + ': ' + excerpt(repliedTo.content)}
                </span>
              </p>
            )}
            <div className={cn('flex items-end gap-2', runStart && !newDay && 'mt-2')}>
              {!mine && (runEnd ? <SenderAvatar account={m.account} /> : <span className="w-7 shrink-0" />)}
              <div className="min-w-0 flex-1">
                <MessageBubble
                  status={m}
                  token={token}
                  focused={m.id === focusedId && m.id !== messages.at(-1)?.id}
                  onReply={onReply}
                />
              </div>
            </div>
            {runEnd && (
              <p className={cn('text-muted-foreground mt-0.5 text-xs', mine ? 'text-right' : 'ml-9')}>
                {timeLabel(m.createdAt)}
              </p>
            )}
          </div>
        )
      })}
    </div>
  )
}
