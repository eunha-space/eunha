import { useCallback, useEffect, useRef, type ReactNode } from 'react'
import { Link } from 'react-router-dom'
import {
  AtSign,
  Bell,
  Flag,
  Gavel,
  Pencil,
  Repeat2,
  Star,
  UserPlus,
  UserX,
} from 'lucide-react'

import type { mastodon } from '../masto.ts'
import { getNotifications, markNotificationsRead } from '../api.ts'
import { beginLogin, getToken } from '../auth.ts'
import { useInfiniteFeed } from '../hooks/use-infinite-feed.ts'
import { useStreamingSubscription } from '../hooks/use-streaming-subscription.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { ColumnHeader } from '@/components/column-header.tsx'
import { StatusCard } from '@/components/status-card.tsx'
import { useComposeModal } from '@/components/compose-modal.tsx'
import { FollowRequestActions } from '@/components/follow-request-actions.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { Card, CardContent } from '@/components/ui/card.tsx'
import { Button } from '@/components/ui/button.tsx'
import { TimelineStack } from '@/components/timeline-stack.tsx'

function describe(type: string): { icon: ReactNode; verb: string } {
  switch (type) {
    case 'mention':
      return { icon: <AtSign className="size-4" />, verb: 'mentioned you' }
    case 'reblog':
      return { icon: <Repeat2 className="size-4" />, verb: 'boosted your post' }
    case 'quote':
      return { icon: <Repeat2 className="size-4" />, verb: 'quoted your post' }
    case 'favourite':
      return { icon: <Star className="size-4" />, verb: 'favourited your post' }
    case 'follow':
      return { icon: <UserPlus className="size-4" />, verb: 'followed you' }
    case 'follow_request':
      return { icon: <UserPlus className="size-4" />, verb: 'requested to follow you' }
    case 'status':
      return { icon: <Bell className="size-4" />, verb: 'posted' }
    case 'update':
      return { icon: <Pencil className="size-4" />, verb: 'edited a post' }
    case 'poll':
      return { icon: <Bell className="size-4" />, verb: 'ran a poll that ended' }
    case 'admin.sign_up':
      return { icon: <UserPlus className="size-4" />, verb: 'signed up' }
    case 'admin.report':
      return { icon: <Flag className="size-4" />, verb: 'filed a report' }
    default:
      return { icon: <Bell className="size-4" />, verb: type }
  }
}

// Mastodon's `notification.moderation_warning.action_*`.
const WARNING_TEXT: Record<mastodon.v1.AccountWarningAction, string> = {
  none: 'You have received a moderation warning.',
  disable: 'Your account has been disabled.',
  mark_statuses_as_sensitive: 'Some of your posts have been marked as sensitive.',
  delete_statuses: 'Some of your posts have been removed.',
  sensitive: 'Your posts will be marked as sensitive from now on.',
  silence: 'Your account has been limited.',
  suspend: 'Your account has been suspended.',
}

// Mastodon's `notification.relationships_severance_event.*`.
function severanceText(event: mastodon.v1.RelationshipSeveranceEvent): string {
  const lost = `${event.followersCount} of your followers and ${event.followingCount} accounts you follow`
  switch (event.type) {
    case 'account_suspension':
      return `A moderator has suspended ${event.targetName}, so you can no longer receive updates from them or interact with them.`
    case 'domain_block':
      return `A moderator has blocked ${event.targetName}, including ${lost}.`
    case 'user_domain_block':
      return `You have blocked ${event.targetName}, removing ${lost}.`
    default:
      return `Relationships with ${event.targetName} were severed.`
  }
}

/**
 * The notifications that are about the server rather than a person: a warning
 * or action from its moderators, and follows lost to a block or suspension.
 * Neither has someone to name at its head, so they read as a sentence.
 */
function SystemNotice({ icon, title, children }: { icon: ReactNode; title: string; children?: ReactNode }) {
  return (
    <Card className="rounded-none border-0 py-3 shadow-none">
      <CardContent className="space-y-1 px-3 sm:px-4">
        <div className="flex items-center gap-1.5 text-sm font-medium">
          {icon}
          <span>{title}</span>
        </div>
        {children}
      </CardContent>
    </Card>
  )
}

const CATEGORY_LABELS: Record<string, string> = {
  spam: 'Spam',
  legal: 'Legal',
  violation: 'Rule violation',
  other: 'Other',
}

function NotificationItem({
  n,
  token,
  onResolve,
  onReply,
}: {
  n: mastodon.v1.Notification
  token: string
  // Removes this notification once its follow request is accepted/rejected.
  onResolve: (id: string) => void
  // Opens the reply composer inline instead of navigating to the thread.
  onReply: (status: mastodon.v1.Status) => void
}) {
  if (n.type === 'moderation_warning') {
    const w = n.moderationWarning
    return (
      <SystemNotice icon={<Gavel className="size-4" />} title={WARNING_TEXT[w.action] ?? w.action}>
        {w.text && <p className="text-muted-foreground text-sm whitespace-pre-wrap">{w.text}</p>}
        <Link to={`/disputes/strikes/${w.id}`} className="text-sm">
          Learn more
        </Link>
      </SystemNotice>
    )
  }
  if (n.type === 'severed_relationships') {
    return (
      <SystemNotice
        icon={<UserX className="size-4" />}
        title={`Relationships with ${n.event.targetName} severed`}
      >
        <p className="text-muted-foreground text-sm">{severanceText(n.event)}</p>
      </SystemNotice>
    )
  }

  const { icon, verb } = describe(n.type)
  const name = n.account.displayName || n.account.username
  const header = (
    <div className="text-muted-foreground flex items-center gap-1.5 text-sm">
      {icon}
      <Link
        to={`/@${n.account.acct}`}
        className="text-foreground font-medium no-underline hover:underline"
      >
        {name}
      </Link>
      <span>{verb}</span>
    </div>
  )

  if (n.status) {
    return (
      <div>
        <div className="px-3 pt-3 sm:px-4">{header}</div>
        <StatusCard
          status={n.status.reblog ?? n.status}
          token={token}
          boostedBy={n.status.reblog ? n.status.account : undefined}
          filterContext="notifications"
          onReply={onReply}
        />
      </div>
    )
  }
  return (
    <Card className="rounded-none border-0 py-3 shadow-none">
      <CardContent className="space-y-2 px-3 sm:px-4">
        {header}
        {n.type === 'admin.sign_up' && (
          <Link to={`/admin/accounts/${n.account.id}`} className="text-sm">
            Review @{n.account.acct}
          </Link>
        )}
        {n.type === 'admin.report' && (
          <Link
            to={`/admin/reports/${n.report.id}`}
            className="hover:bg-muted/40 block space-y-0.5 rounded-lg border p-2 text-sm no-underline"
          >
            <span className="block">
              Report on <span className="font-medium">@{n.report.targetAccount.acct}</span>
              {' · '}
              {CATEGORY_LABELS[n.report.category] ?? n.report.category}
              {n.report.statusIds?.length
                ? ` · ${n.report.statusIds.length} post${n.report.statusIds.length === 1 ? '' : 's'}`
                : ''}
            </span>
            {n.report.comment && (
              <span className="text-muted-foreground line-clamp-2 block">{n.report.comment}</span>
            )}
          </Link>
        )}
        {n.type === 'follow_request' && (
          <FollowRequestActions
            account={n.account}
            token={token}
            onResolved={() => onResolve(n.id)}
          />
        )}
      </CardContent>
    </Card>
  )
}

export function NotificationsFeed() {
  const token = getToken()
  const { openCompose } = useComposeModal()
  const feed = useInfiniteFeed<mastodon.v1.Notification>(
    (maxId) => (token ? getNotifications(token, maxId) : Promise.resolve([])),
    [token],
  )
  const { mutate } = feed

  // Moving the notifications marker to the newest item on screen is what makes
  // the nav badge clear — the server counts everything after it. Only ever
  // forward: a reader who scrolls back through older pages has not unread
  // them, and `useInfiniteFeed` appends, so `items[0]` stays the newest.
  const newestId = feed.items?.[0]?.id
  const markedRef = useRef<string | null>(null)
  useEffect(() => {
    if (!token || !newestId || markedRef.current === newestId) return
    // Ids are numeric strings of no fixed width, so compare them as numbers:
    // "9" sorts after "10" as text, which would move the marker backwards.
    if (markedRef.current && BigInt(newestId) <= BigInt(markedRef.current)) return
    markedRef.current = newestId
    markNotificationsRead(token, newestId).catch(() => {})
  }, [token, newestId])
  const handleReply = useCallback(
    (status: mastodon.v1.Status) => openCompose({ replyTo: status }),
    [openCompose],
  )
  const removeNotification = useCallback(
    (id: string) => mutate((items) => items.filter((item) => item.id !== id)),
    [mutate],
  )
  const subscribeNotifications = useCallback(
    (client: mastodon.streaming.Client) => client.user.notification.subscribe(),
    [],
  )
  const handleStreamingEvent = useCallback(
    (event: mastodon.streaming.Event) => {
      if (event.event === 'notification') {
        mutate((items) =>
          items.some((item) => item.id === event.payload.id)
            ? items
            : [event.payload, ...items],
        )
        return
      }

      if (event.event === 'status.update') {
        mutate((items) =>
          items.map((item) => {
            if (!('status' in item) || item.status?.id !== event.payload.id) {
              return item
            }
            return { ...item, status: event.payload } as mastodon.v1.Notification
          }),
        )
        return
      }

      if (event.event === 'delete') {
        mutate((items) =>
          items.filter(
            (item) => !('status' in item) || item.status?.id !== event.payload,
          ),
        )
      }
    },
    [mutate],
  )

  useStreamingSubscription({
    enabled: !!token,
    token: token ?? undefined,
    subscribe: subscribeNotifications,
    onEvent: handleStreamingEvent,
  })
  const items = feed.items

  if (!token) {
    return (
      <div className="space-y-2">
        <p className="text-muted-foreground text-sm">
          Sign in to see your notifications.
        </p>
        <Button size="sm" onClick={() => beginLogin()}>
          Sign in
        </Button>
      </div>
    )
  }

  return (
    <>
          {feed.error && <p className="text-destructive text-sm">{feed.error}</p>}
          {items === null && !feed.error && (
            <p className="text-muted-foreground text-sm">Loading…</p>
          )}
          {!!items?.length && (
            <TimelineStack>
              {items.map((n) => (
                <NotificationItem
                  key={n.id}
                  n={n}
                  token={token}
                  onResolve={removeNotification}
                  onReply={handleReply}
                />
              ))}
            </TimelineStack>
          )}
          {items?.length === 0 && (
            <p className="text-muted-foreground text-sm">No notifications yet.</p>
          )}
          <InfiniteScroll
            onLoadMore={feed.loadMore}
            loading={feed.loadingMore}
            done={feed.done}
            hasItems={!!items?.length}
          />
    </>
  )
}

export default function Notifications() {
  return (
    <>
      <TopBar />
      <div className="column-frame">
        <ColumnHeader title="Notifications" />
        <div className="space-y-2 p-3">
          <NotificationsFeed />
        </div>
      </div>
    </>
  )
}
