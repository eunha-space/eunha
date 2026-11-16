import { useState } from 'react'
import { Link } from 'react-router-dom'
import { toast } from 'sonner'

import type { mastodon } from '../masto.ts'
import { getFollowedTags, setTagFollow } from '../api.ts'
import { beginLogin, getActiveAccountId, getToken } from '../auth.ts'
import { useInfinitePaginator } from '../hooks/use-infinite-paginator.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { Button } from '@/components/ui/button.tsx'
import { TagsSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

function FollowedTagRow({
  name,
  token,
  onUnfollow,
}: {
  name: string
  token: string
  onUnfollow: () => void
}) {
  const [busy, setBusy] = useState(false)

  const unfollow = async () => {
    setBusy(true)
    try {
      await setTagFollow(name, token, false)
      onUnfollow()
    } catch {
      toast.error(t({ id: 'eunha.followed_tags.could_not_unfollow', defaultMessage: 'Could not unfollow #{name}' }, { name }))
      setBusy(false)
    }
  }

  return (
    <li className="flex items-center justify-between gap-3 p-3">
      <Link className="min-w-0 break-all font-medium" to={`/tags/${encodeURIComponent(name)}`}>
        #{name}
      </Link>
      <Button
        variant="outline"
        size="sm"
        aria-label={t({ id: 'eunha.followed_tags.unfollow', defaultMessage: 'Unfollow #{name}' }, { name })}
        disabled={busy}
        onClick={unfollow}
      >
        {t({ id: 'account.unfollow', defaultMessage: 'Unfollow' })}
      </Button>
    </li>
  )
}

function FollowedTagList({ token }: { token: string }) {
  const [attempt, setAttempt] = useState(0)
  const { items, error, loadingMore, done, loadMore, mutate } =
    useInfinitePaginator<mastodon.v1.Tag>(() => getFollowedTags(token), [token, attempt])

  return (
    <div className="space-y-3">
      {error && (
        <div className="flex items-center gap-3" role="alert">
          <p className="text-destructive text-sm">{t({ id: 'eunha.followed_tags.could_not_load_followed_hashtags', defaultMessage: 'Could not load followed hashtags.' })}</p>
          <Button variant="outline" size="sm" onClick={() => setAttempt(attempt + 1)}>
            {t({ id: 'status.context.retry', defaultMessage: 'Retry' })}
          </Button>
        </div>
      )}
      {items === null && !error && <TagsSkeleton />}
      {items?.length === 0 && done && (
        <p className="text-muted-foreground text-sm">
          {t({ id: 'eunha.followed_tags.you_aren_t_following_any_hashtags', defaultMessage: 'You aren’t following any hashtags. Follow one from a post or a hashtag timeline.' })}
        </p>
      )}
      {!!items?.length && (
        <ul className="divide-y rounded-lg border">
          {items.map(({ name }) => (
            <FollowedTagRow
              key={name}
              name={name}
              token={token}
              onUnfollow={() => mutate((tags) => tags.filter((tag) => tag.name !== name))}
            />
          ))}
        </ul>
      )}
      {items && !done && !error && (
        <Button variant="outline" disabled={loadingMore} onClick={loadMore}>
          {loadingMore ? t({ id: 'loading_indicator.label', defaultMessage: 'Loading…' }) : t({ id: 'status.load_more', defaultMessage: 'Load more' })}
        </Button>
      )}
    </div>
  )
}

export default function FollowedTags() {
  const token = getToken()

  return (
    <div className="page-frame">
      <TopBar />
      <h1 className="mb-4 text-xl font-bold">{t({ id: 'navigation_bar.followed_tags', defaultMessage: 'Followed hashtags' })}</h1>
      {token ? (
        <FollowedTagList key={getActiveAccountId()} token={token} />
      ) : (
        <div className="space-y-3">
          <p className="text-muted-foreground text-sm">{t({ id: 'eunha.followed_tags.sign_in_to_manage_the_hashtags', defaultMessage: 'Sign in to manage the hashtags you follow.' })}</p>
          <Button onClick={() => void beginLogin()}>{t({ id: 'mastodon.user_mailer.welcome.sign_in_action', defaultMessage: 'Sign in' })}</Button>
        </div>
      )}
    </div>
  )
}
