import { useState } from 'react'
import { Link } from 'react-router-dom'
import { toast } from 'sonner'
import { LockOpen, Plus, Trash2 } from 'lucide-react'

import type { mastodon } from '../masto.ts'
import { deleteConversation, getConversations, markConversationRead } from '../api.ts'
import { beginLogin, getToken } from '../auth.ts'
import { useInfinitePaginator } from '../hooks/use-infinite-paginator.ts'
import { useAnimatedImage } from '../hooks/use-animated-image.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { ColumnHeader } from '@/components/column-header.tsx'
import { RelativeTime } from '@/components/relative-time.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { useComposeModal } from '@/components/compose-modal.tsx'
import { Avatar, AvatarFallback, AvatarImage } from '@/components/ui/avatar.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Card, CardContent } from '@/components/ui/card.tsx'
import { cn, errorMessage } from '@/lib/utils.ts'
import { AnimateEmoji, EmojiText } from '@/components/emoji.tsx'

// The plain text of a status, for a one-line preview. The API sends HTML and a
// conversation row is not the place to render it.
function excerpt(html: string): string {
  const el = document.createElement('div')
  el.innerHTML = html
  return (el.textContent ?? '').replace(/\s+/g, ' ').trim()
}

function participants(accounts: mastodon.v1.Account[]): string {
  if (accounts.length === 0) return 'You'
  return accounts.map((a) => a.displayName || a.username).join(', ')
}

function ConversationRow({
  conversation,
  token,
  onRead,
  onRemoved,
}: {
  conversation: mastodon.v1.Conversation
  token: string
  onRead: (id: string) => void
  onRemoved: (id: string) => void
}) {
  const [busy, setBusy] = useState(false)
  const last = conversation.lastStatus
  const accounts = conversation.accounts
  const lead = accounts[0]
  const avatar = useAnimatedImage(lead?.avatar, lead?.avatarStatic)
  const name = participants(accounts)
  // Without a last status there is no thread to open and nothing to preview,
  // so the row is a dead end — the server can return one after its only post
  // was deleted.
  const href = last ? `/@${last.account.acct}/${last.id}` : null

  const open = () => {
    if (conversation.unread) {
      markConversationRead(token, conversation.id)
        .then(() => onRead(conversation.id))
        .catch(() => {})
    }
  }

  const remove = async () => {
    if (busy) return
    setBusy(true)
    try {
      await deleteConversation(token, conversation.id)
      onRemoved(conversation.id)
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  const inner = (
    <>
      <div className="relative shrink-0">
        <Avatar className="size-10" {...avatar.hover}>
          <AvatarImage src={avatar.src} alt="" />
          <AvatarFallback>{name.slice(0, 1).toUpperCase()}</AvatarFallback>
        </Avatar>
        {conversation.unread && (
          <span
            className="bg-primary absolute -top-0.5 -right-0.5 size-2.5 rounded-full"
            aria-label="Unread"
          />
        )}
      </div>
      <div className="min-w-0 flex-1">
        <div className="flex items-baseline gap-2">
          {/* Mastodon's `conversation__content__names`: every name, with its
              emoji, animating together on hover. */}
          <AnimateEmoji
            as="span"
            className={cn('truncate text-sm', conversation.unread && 'font-semibold')}
          >
            {accounts.length === 0
              ? name
              : accounts.map((a, i) => (
                  <span key={a.id}>
                    {i > 0 && ', '}
                    <EmojiText text={a.displayName || a.username} emojis={a.emojis} />
                  </span>
                ))}
          </AnimateEmoji>
          {last && (
            <span className="text-muted-foreground shrink-0 text-xs">
              <RelativeTime value={last.createdAt} />
            </span>
          )}
        </div>
        <p className="text-muted-foreground truncate text-sm">
          {last ? (
            <EmojiText text={excerpt(last.content)} emojis={last.emojis} />
          ) : (
            'No messages left in this thread.'
          )}
        </p>
      </div>
    </>
  )

  return (
    <div className="flex items-center gap-2">
      {href ? (
        <Link
          to={href}
          onClick={open}
          className="hover:bg-muted/50 flex min-w-0 flex-1 items-center gap-3 rounded-lg p-2 no-underline"
        >
          {inner}
        </Link>
      ) : (
        <div className="flex min-w-0 flex-1 items-center gap-3 p-2">{inner}</div>
      )}
      <Button
        variant="ghost"
        size="icon"
        aria-label={`Delete conversation with ${name}`}
        disabled={busy}
        onClick={() => void remove()}
      >
        <Trash2 />
      </Button>
    </div>
  )
}

export function MessagesFeed() {
  const token = getToken()
  const { openCompose } = useComposeModal()

  const feed = useInfinitePaginator<mastodon.v1.Conversation>(
    () => getConversations(token ?? ''),
    [token],
  )
  const conversations = feed.items

  if (!token) {
    return (
      <Card>
          <CardContent className="space-y-3 py-6 text-center">
            <p className="text-muted-foreground text-sm">
              Sign in to read your messages.
            </p>
          <Button onClick={() => void beginLogin()}>Sign in</Button>
        </CardContent>
      </Card>
    )
  }

  return (
    <>
      <div className="mb-2 flex justify-end">
        {/* Opening with `messageTo: null` starts an unaddressed message — the
            recipient is typed into the text, as upstream does from this page. */}
        <Button size="sm" onClick={() => openCompose({ messageTo: null })}>
          <Plus /> New
        </Button>
      </div>
      <p className="text-muted-foreground mb-3 flex items-start gap-1.5 text-xs">
        <LockOpen className="mt-0.5 size-3.5 shrink-0" />
        Messages are not end-to-end encrypted. Don't share anything here you
        wouldn't share with the servers involved.
      </p>

      {feed.error && <p className="text-destructive text-sm">{feed.error}</p>}
      <div className="space-y-1">
        {conversations === null && !feed.error && (
          <p className="text-muted-foreground text-sm">Loading…</p>
        )}
        {conversations?.map((c) => (
          <ConversationRow
            key={c.id}
            conversation={c}
            token={token}
            onRead={(id) =>
              feed.mutate((items) =>
                items.map((item) => (item.id === id ? { ...item, unread: false } : item)),
              )
            }
            onRemoved={(id) => feed.mutate((items) => items.filter((i) => i.id !== id))}
          />
        ))}
        {conversations?.length === 0 && (
          <p className="text-muted-foreground text-sm">
            No messages yet. A message you send or receive shows up here.
          </p>
        )}
        <InfiniteScroll
          onLoadMore={feed.loadMore}
          loading={feed.loadingMore}
          done={feed.done}
          hasItems={!!conversations?.length}
        />
      </div>
    </>
  )
}

export default function Messages() {
  return (
    <>
      <TopBar />
      <div className="column-frame">
        <ColumnHeader title="Messages" />
        <div className="p-3">
          <MessagesFeed />
        </div>
      </div>
    </>
  )
}
