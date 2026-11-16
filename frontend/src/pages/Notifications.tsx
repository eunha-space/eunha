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
import { DisplayName } from '@/components/emoji.tsx'
import { NotificationsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'
import { FormattedMessage } from 'react-intl'

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
    case 'quoted_update':
      return { icon: <Pencil className="size-4" />, verb: 'edited a post you have quoted' }
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
  get none() { return t({ id: 'mastodon.notification_fallbacks.moderation_warning.title', defaultMessage: 'You have received a moderation warning.' }) },
  get disable() { return t({ id: 'notification.moderation_warning.action_disable', defaultMessage: 'Your account has been disabled.' }) },
  get mark_statuses_as_sensitive() { return t({ id: 'notification.moderation_warning.action_mark_statuses_as_sensitive', defaultMessage: 'Some of your posts have been marked as sensitive.' }) },
  get delete_statuses() { return t({ id: 'notification.moderation_warning.action_delete_statuses', defaultMessage: 'Some of your posts have been removed.' }) },
  get sensitive() { return t({ id: 'notification.moderation_warning.action_sensitive', defaultMessage: 'Your posts will be marked as sensitive from now on.' }) },
  get silence() { return t({ id: 'notification.moderation_warning.action_silence', defaultMessage: 'Your account has been limited.' }) },
  get suspend() { return t({ id: 'notification.moderation_warning.action_suspend', defaultMessage: 'Your account has been suspended.' }) },
}

// Mastodon's `notification.relationships_severance_event.*`.
function severanceText(event: mastodon.v1.RelationshipSeveranceEvent): string {
  const lost = `${event.followersCount} of your followers and ${event.followingCount} accounts you follow`
  switch (event.type) {
    case 'account_suspension':
      return t({ id: 'eunha.notifications.a_moderator_has_suspended_so_you', defaultMessage: 'A moderator has suspended {targetName}, so you can no longer receive updates from them or interact with them.' }, { targetName: event.targetName })
    case 'domain_block':
      return t({ id: 'eunha.notifications.a_moderator_has_blocked_including', defaultMessage: 'A moderator has blocked {targetName}, including {lost}.' }, { targetName: event.targetName, lost })
    case 'user_domain_block':
      return t({ id: 'eunha.notifications.you_have_blocked_removing', defaultMessage: 'You have blocked {targetName}, removing {lost}.' }, { targetName: event.targetName, lost })
    default:
      return t({ id: 'eunha.notifications.relationships_with_were_severed', defaultMessage: 'Relationships with {targetName} were severed.' }, { targetName: event.targetName })
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
  get spam() { return t({ id: 'report.categories.spam', defaultMessage: 'Spam' }) },
  get legal() { return t({ id: 'report.categories.legal', defaultMessage: 'Legal' }) },
  get violation() { return t({ id: 'report_notification.categories.violation', defaultMessage: 'Rule violation' }) },
  get other() { return t({ id: 'report.categories.other', defaultMessage: 'Other' }) },
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
          {t({ id: 'learn_more_link.learn_more', defaultMessage: 'Learn more' })}
        </Link>
      </SystemNotice>
    )
  }
  if (n.type === 'severed_relationships') {
    return (
      <SystemNotice
        icon={<UserX className="size-4" />}
        title={t({ id: 'eunha.notifications.relationships_with_severed', defaultMessage: 'Relationships with {targetName} severed' }, { targetName: n.event.targetName })}
      >
        <p className="text-muted-foreground text-sm">{severanceText(n.event)}</p>
      </SystemNotice>
    )
  }

  const { icon, verb } = describe(n.type)
  const header = (
    <div className="text-muted-foreground flex items-center gap-1.5 text-sm">
      {icon}
      <Link
        to={`/@${n.account.acct}`}
        className="text-foreground font-medium no-underline hover:underline"
      >
        <DisplayName account={n.account} />
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
            {t({ id: 'eunha.notifications.review_account', defaultMessage: 'Review @{acct}' }, { acct: n.account.acct })}
          </Link>
        )}
        {n.type === 'admin.report' && (
          <Link
            to={`/admin/reports/${n.report.id}`}
            className="hover:bg-muted/40 block space-y-0.5 rounded-lg border p-2 text-sm no-underline"
          >
            <span className="block">
              <FormattedMessage
                id='eunha.notifications.report_on'
                defaultMessage='Report on {target}'
                values={{ target: <span className="font-medium">@{n.report.targetAccount.acct}</span> }}
              />
              {' · '}
              {CATEGORY_LABELS[n.report.category] ?? n.report.category}
              {n.report.statusIds?.length
                ? ' · ' + t({ id: 'account_edit_tags.tag_status_count', defaultMessage: '{count, plural, one {# post} other {# posts}}' }, { count: n.report.statusIds.length })
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
    'notifications',
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
          {t({ id: 'eunha.notifications.sign_in_to_see_your_notifications', defaultMessage: 'Sign in to see your notifications.' })}
        </p>
        <Button size="sm" onClick={() => beginLogin()}>
          {t({ id: 'mastodon.user_mailer.welcome.sign_in_action', defaultMessage: 'Sign in' })}
        </Button>
      </div>
    )
  }

  return (
    <>
          {feed.error && <p className="text-destructive text-sm">{feed.error}</p>}
          {items === null && !feed.error && (
            <NotificationsSkeleton />
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
            <p className="text-muted-foreground text-sm">{t({ id: 'eunha.notifications.no_notifications_yet', defaultMessage: 'No notifications yet.' })}</p>
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
        <ColumnHeader title={t({ id: 'column.notifications', defaultMessage: 'Notifications' })} />
        <div className="space-y-2 p-3">
          <Button variant="outline" size="sm" render={<Link to="/notifications/requests" />}>
            {t({ id: 'eunha.notifications.notification_requests_and_policy', defaultMessage: 'Notification requests and policy' })}
          </Button>
          <NotificationsFeed />
        </div>
      </div>
    </>
  )
}
