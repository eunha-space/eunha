import { useEffect, useState } from 'react'
import { Link, NavLink, useLocation } from 'react-router-dom'
import { EyeOff, Film, Hash, Images } from 'lucide-react'

import type { mastodon } from '../masto.ts'
import {
  getAccountCollections,
  getAccountStatuses,
  getEndorsements,
  type AccountCollection,
} from '../api.ts'
import { useInfiniteFeed } from '../hooks/use-infinite-feed.ts'
import { AccountRow } from '@/components/account-row.tsx'
import { Blurhash } from '@/components/blurhash.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Switch } from '@/components/ui/switch.tsx'
import { cn } from '@/lib/utils.ts'

// What masto.js leaves out of an account: the profile's display preferences,
// which choose its tabs.
interface ProfilePreferences {
  showMedia?: boolean
  showMediaReplies?: boolean
  showFeatured?: boolean
}

function preferences(account: mastodon.v1.Account): Required<ProfilePreferences> {
  const extra = account as mastodon.v1.Account & ProfilePreferences
  return {
    showMedia: extra.showMedia ?? true,
    showMediaReplies: extra.showMediaReplies ?? true,
    showFeatured: extra.showFeatured ?? true,
  }
}

export type ProfileView = 'activity' | 'media' | 'featured'

// Mastodon's profile routes: `/@acct`, `/@acct/with_replies` and
// `/@acct/tagged/:tag` are the activity tab, `/@acct/media` and
// `/@acct/featured` the other two.
export function profileView(pathname: string): ProfileView {
  if (/\/media\/?$/.test(pathname)) return 'media'
  if (/\/featured\/?$/.test(pathname)) return 'featured'
  return 'activity'
}

/**
 * Whether an account offers each tab. An account that hides both its media
 * and its featured tabs gets no tab bar at all, as `AccountTabs` draws none.
 */
export function offeredViews(account: mastodon.v1.Account): ProfileView[] {
  const { showMedia, showFeatured } = preferences(account)
  return [
    'activity',
    ...(showMedia ? (['media'] as const) : []),
    ...(showFeatured ? (['featured'] as const) : []),
  ]
}

const tab =
  'border-b-2 border-transparent px-3 py-2 text-sm font-medium text-muted-foreground no-underline hover:text-foreground'

/** `AccountTabs`: Activity, and Media and Featured where the account shows them. */
export function ProfileTabs({ account }: { account: mastodon.v1.Account }) {
  const { pathname } = useLocation()
  const views = offeredViews(account)
  if (views.length === 1) return <hr className="mb-2" />
  const current = profileView(pathname)
  const base = `/@${account.acct}`
  const link = (view: ProfileView, to: string, label: string) => (
    <NavLink
      to={to}
      end
      className={cn(tab, current === view && 'border-primary text-foreground')}
      aria-current={current === view ? 'page' : undefined}
    >
      {label}
    </NavLink>
  )
  return (
    <nav className="mb-2 flex gap-1 border-b" aria-label="Profile">
      {link('activity', base, 'Activity')}
      {views.includes('media') && link('media', `${base}/media`, 'Media')}
      {views.includes('featured') && link('featured', `${base}/featured`, 'Featured')}
    </nav>
  )
}

// The activity tab's filters, kept per browser as Mastodon keeps them
// (`account-filters` in local storage): boosts shown, replies hidden.
const FILTERS_KEY = 'eunha:account-filters'

export interface ActivityFilters {
  boosts: boolean
  replies: boolean
}

function readFilters(): ActivityFilters {
  try {
    const saved = JSON.parse(localStorage.getItem(FILTERS_KEY) ?? '{}') as Partial<ActivityFilters>
    return { boosts: saved.boosts ?? true, replies: saved.replies ?? false }
  } catch {
    return { boosts: true, replies: false }
  }
}

export function useActivityFilters(): [ActivityFilters, (next: ActivityFilters) => void] {
  const [filters, setFilters] = useState(readFilters)
  const update = (next: ActivityFilters) => {
    setFilters(next)
    try {
      localStorage.setItem(FILTERS_KEY, JSON.stringify(next))
    } catch {
      // A private window keeps the choice for this page only.
    }
  }
  return [filters, update]
}

/** The boosts and replies switches, and the featured hashtags to narrow by. */
export function ActivityFilterBar({
  account,
  filters,
  onChange,
  repliesForced,
  featuredTags,
  tagged,
}: {
  account: mastodon.v1.Account
  filters: ActivityFilters
  onChange: (next: ActivityFilters) => void
  repliesForced: boolean
  featuredTags: mastodon.v1.FeaturedTag[]
  tagged?: string
}) {
  const base = `/@${account.acct}`
  return (
    <div className="mb-2 space-y-2">
      <div className="flex flex-wrap gap-4">
        <Label className="text-sm font-normal">
          <Switch
            checked={filters.boosts}
            onCheckedChange={(boosts) => onChange({ ...filters, boosts })}
          />
          Boosts
        </Label>
        <Label className="text-sm font-normal">
          <Switch
            checked={repliesForced || filters.replies}
            disabled={repliesForced}
            onCheckedChange={(replies) => onChange({ ...filters, replies })}
          />
          Replies
        </Label>
      </div>
      {featuredTags.length > 0 && (
        <div className="flex flex-wrap gap-1.5" aria-label="Featured hashtags">
          {featuredTags.map((tag) => {
            const active = tagged?.toLowerCase() === tag.name.toLowerCase()
            return (
              <Link
                key={tag.id}
                to={active ? base : `${base}/tagged/${encodeURIComponent(tag.name)}`}
                aria-current={active ? 'page' : undefined}
                className={cn(
                  'inline-flex items-center gap-0.5 rounded-full border px-2.5 py-0.5 text-xs no-underline',
                  active
                    ? 'border-primary bg-primary text-primary-foreground'
                    : 'text-muted-foreground hover:text-foreground',
                )}
              >
                <Hash className="size-3" />
                {tag.name}
                <span className="opacity-70">· {tag.statusesCount}</span>
              </Link>
            )
          })}
        </div>
      )}
    </div>
  )
}

/**
 * `AccountGallery`: an account's media, as a grid of thumbnails each opening its
 * post. Replies are left out unless the account shows them
 * (`show_media_replies`), as upstream asks with `exclude_replies`.
 */
export function MediaGrid({ account, token }: { account: mastodon.v1.Account; token?: string }) {
  const { showMediaReplies } = preferences(account)
  const feed = useInfiniteFeed<mastodon.v1.Status>(
    (maxId) =>
      getAccountStatuses(account.id, token, maxId, {
        onlyMedia: true,
        excludeReplies: !showMediaReplies,
      }),
    [account.id, token, showMediaReplies],
    `profile-media:${account.acct}`,
  )
  const tiles = (feed.items ?? []).flatMap((status) => {
    const original = status.reblog ?? status
    return original.mediaAttachments
      .filter((m) => m.type !== 'unknown')
      .map((media) => ({ status: original, media }))
  })

  return (
    <div className="space-y-2">
      {feed.items === null && <p className="text-muted-foreground text-sm">Loading…</p>}
      {tiles.length > 0 && (
        <ul className="grid grid-cols-3 gap-1" aria-label="Media">
          {tiles.map(({ status, media }) => (
            <li key={`${status.id}:${media.id}`}>
              <MediaTile status={status} media={media} />
            </li>
          ))}
        </ul>
      )}
      {feed.items?.length === 0 && <p className="text-muted-foreground text-sm">No media yet.</p>}
      <InfiniteScroll
        onLoadMore={feed.loadMore}
        loading={feed.loadingMore}
        done={feed.done}
        hasItems={tiles.length > 0}
      />
    </div>
  )
}

function MediaTile({
  status,
  media,
}: {
  status: mastodon.v1.Status
  media: mastodon.v1.MediaAttachment
}) {
  // A sensitive post's media stays behind its blurhash here too, as the
  // gallery hides it until opened.
  const hidden = status.sensitive
  const preview = media.previewUrl || media.url || ''
  const moving = media.type === 'video' || media.type === 'gifv'
  return (
    <Link
      to={`/@${status.account.acct}/${status.id}`}
      className="bg-muted relative block aspect-square overflow-hidden rounded-md"
      aria-label={media.description || (hidden ? 'Sensitive media' : 'Media')}
    >
      {hidden ? (
        <>
          {media.blurhash && (
            <Blurhash hash={media.blurhash} className="absolute inset-0 h-full w-full" />
          )}
          <span className="absolute inset-0 flex items-center justify-center text-white">
            <EyeOff className="size-5 drop-shadow" />
          </span>
        </>
      ) : media.type === 'audio' || !preview ? (
        <span className="text-muted-foreground absolute inset-0 flex items-center justify-center">
          <Images className="size-6" />
        </span>
      ) : (
        <img
          src={preview}
          alt={media.description ?? ''}
          loading="lazy"
          className="absolute inset-0 h-full w-full object-cover"
        />
      )}
      {moving && !hidden && (
        <Film className="absolute right-1.5 bottom-1.5 size-4 text-white drop-shadow" />
      )}
    </Link>
  )
}

/**
 * `AccountFeatured`: the profiles an account endorses, and its collections —
 * the discoverable ones, and to its owner the unlisted ones as well.
 */
export function FeaturedPane({
  account,
  token,
  isSelf,
}: {
  account: mastodon.v1.Account
  token?: string
  isSelf: boolean
}) {
  const [profiles, setProfiles] = useState<mastodon.v1.Account[] | null>(null)
  const [collections, setCollections] = useState<AccountCollection[] | null>(null)

  useEffect(() => {
    setProfiles(null)
    setCollections(null)
    getEndorsements(account.id, token)
      .then(setProfiles)
      .catch(() => setProfiles([]))
    getAccountCollections(account.id, token)
      .then(setCollections)
      .catch(() => setCollections([]))
  }, [account.id, token])

  if (profiles === null || collections === null) {
    return <p className="text-muted-foreground text-sm">Loading…</p>
  }

  const listed = collections.filter((c) => c.discoverable)
  const unlisted = isSelf ? collections.filter((c) => !c.discoverable) : []

  if (profiles.length === 0 && listed.length === 0 && unlisted.length === 0) {
    return (
      <p className="text-muted-foreground text-sm">
        {isSelf
          ? 'You have not featured any profiles or collections yet.'
          : 'This account has not featured any profiles or collections yet.'}
      </p>
    )
  }

  return (
    <div className="space-y-4">
      {profiles.length > 0 && (
        <section className="space-y-1">
          <h2 className="text-muted-foreground text-sm font-semibold">Profiles</h2>
          <div className="divide-y rounded-lg border">
            {profiles.map((p) => (
              <AccountRow key={p.id} account={p} />
            ))}
          </div>
        </section>
      )}
      {(listed.length > 0 || unlisted.length > 0) && (
        <section className="space-y-1">
          <h2 className="text-muted-foreground text-sm font-semibold">Collections</h2>
          <CollectionList collections={listed} />
          {unlisted.length > 0 && (
            <>
              <h3 className="text-muted-foreground pt-2 text-xs font-semibold">
                Unlisted collections ({unlisted.length})
              </h3>
              <p className="text-muted-foreground text-xs">
                These don’t appear on your profile to others. Anyone with the link can discover
                them.
              </p>
              <CollectionList collections={unlisted} />
            </>
          )}
        </section>
      )}
    </div>
  )
}

function CollectionList({ collections }: { collections: AccountCollection[] }) {
  if (collections.length === 0) return null
  return (
    <ul className="divide-y rounded-lg border">
      {collections.map((c) => (
        <li key={c.id} className="space-y-0.5 px-3 py-2">
          <p className="text-sm font-medium">{c.name}</p>
          {c.description && <p className="text-muted-foreground text-xs">{c.description}</p>}
          <p className="text-muted-foreground text-xs">
            {c.item_count === 1 ? '1 account' : `${c.item_count} accounts`}
          </p>
        </li>
      ))}
    </ul>
  )
}
