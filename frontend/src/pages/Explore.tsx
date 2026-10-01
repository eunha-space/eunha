import type { ReactNode } from 'react'
import { Link, NavLink, useLocation } from 'react-router-dom'

import type { mastodon } from '../masto.ts'
import { getTrendingLinks, getTrendingStatuses, getTrendingTags } from '../api.ts'
import { getToken } from '../auth.ts'
import { useInfinitePaginator } from '../hooks/use-infinite-paginator.ts'
import { TopBar } from '@/components/top-bar.tsx'
import { StatusCard } from '@/components/status-card.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { TimelineStack } from '@/components/timeline-stack.tsx'
import { cn } from '@/lib/utils.ts'

const tab =
  'border-b-2 border-transparent px-3 py-2 text-sm font-medium text-muted-foreground no-underline hover:text-foreground'
const cls = ({ isActive }: { isActive: boolean }) =>
  cn(tab, isActive && 'border-primary text-foreground')

// Each pane owns its own paginator, and only the one on screen is mounted —
// so switching tabs fetches that tab rather than all three on arrival, and
// leaving one drops its feed instead of holding a stale page.
function Pane<T>({
  open,
  deps,
  empty,
  children,
}: {
  open: () => AsyncIterable<T[]>
  deps: unknown[]
  empty: string
  children: (items: T[]) => ReactNode
}) {
  const feed = useInfinitePaginator<T>(open, deps)
  return (
    <div className="space-y-2">
      {feed.error && <p className="text-destructive text-sm">{feed.error}</p>}
      {feed.items === null && !feed.error && (
        <p className="text-muted-foreground text-sm">Loading…</p>
      )}
      {!!feed.items?.length && children(feed.items)}
      {feed.items?.length === 0 && (
        <p className="text-muted-foreground text-sm">{empty}</p>
      )}
      <InfiniteScroll
        onLoadMore={feed.loadMore}
        loading={feed.loadingMore}
        done={feed.done}
        hasItems={!!feed.items?.length}
      />
    </div>
  )
}

// A tag's `history` is the last seven days, newest first, with its counts as
// strings. Summing it is what "this week" means on the row.
function weekly(history: mastodon.v1.Tag['history']) {
  if (!history?.length) return null
  const uses = history.reduce((n, d) => n + Number(d.uses ?? 0), 0)
  const people = history.reduce((n, d) => Math.max(n, Number(d.accounts ?? 0)), 0)
  return uses > 0 ? { uses, people } : null
}

function TagRow({ tag }: { tag: mastodon.v1.Tag }) {
  const week = weekly(tag.history)
  return (
    <Link
      to={`/tags/${tag.name}`}
      className="hover:bg-muted/50 block rounded-lg p-2 no-underline"
    >
      <div className="font-medium">#{tag.name}</div>
      {week && (
        <div className="text-muted-foreground text-sm">
          {week.uses} {week.uses === 1 ? 'post' : 'posts'} this week
          {week.people > 1 ? ` from ${week.people} people` : ''}
        </div>
      )}
    </Link>
  )
}

// A trending link carries a preview card's fields, but most of them arrive
// empty: `trending_links` selects `NULL` for the image outright, and an
// instance that has not scraped a card has no provider or author either. So
// the host stands in for the provider, and every other field renders only when
// it is actually there — including the image, which is here for the entity's
// sake rather than because this server has ever sent one.
function LinkRow({ card }: { card: mastodon.v1.TrendLink }) {
  let host: string
  try {
    host = new URL(card.url).host.replace(/^www\./, '')
  } catch {
    host = card.url
  }
  return (
    <a
      href={card.url}
      target="_blank"
      rel="noopener noreferrer"
      className="hover:bg-muted/50 flex gap-3 rounded-lg border p-3 no-underline"
    >
      {/* The image is decorative: the title beside it is the accessible name. */}
      {card.image && (
        <img
          src={card.image}
          alt=""
          className="size-20 shrink-0 rounded object-cover"
          loading="lazy"
        />
      )}
      <div className="min-w-0">
        <div className="text-muted-foreground text-xs">
          {card.providerName || host}
        </div>
        <div className="font-medium">{card.title || card.url}</div>
        {card.description && (
          <p className="text-muted-foreground line-clamp-2 text-sm">
            {card.description}
          </p>
        )}
      </div>
    </a>
  )
}

export default function Explore() {
  const pathname = useLocation().pathname
  const token = getToken()
  const pane = pathname.endsWith('/tags')
    ? 'tags'
    : pathname.endsWith('/links')
      ? 'links'
      : 'statuses'

  return (
    <div className="page-frame">
      <TopBar />
      <h1 className="mb-2 text-lg font-bold">Explore</h1>
      <nav className="mb-2 flex gap-1 border-b">
        <NavLink to="/explore" end className={cls}>
          Posts
        </NavLink>
        <NavLink to="/explore/tags" className={cls}>
          Hashtags
        </NavLink>
        <NavLink to="/explore/links" className={cls}>
          Links
        </NavLink>
      </nav>

      {/*
        `key` is what makes switching tabs fetch. All three branches render the
        same component type in the same position, so without it React keeps the
        instance and only swaps props — and since the props a hook closes over
        live in refs, the paginator would go on showing the previous tab's
        items, having never asked for these. The pane is in `deps` for the same
        reason, so the effect is right even without the remount.
      */}
      {pane === 'tags' ? (
        <Pane<mastodon.v1.Tag>
          key={pane}
          open={() => getTrendingTags(token ?? undefined)}
          deps={[token, pane]}
          empty="No hashtags are trending yet."
        >
          {(items) => (
            <div className="space-y-1">
              {items.map((t) => (
                <TagRow key={t.name} tag={t} />
              ))}
            </div>
          )}
        </Pane>
      ) : pane === 'links' ? (
        <Pane<mastodon.v1.TrendLink>
          key={pane}
          open={() => getTrendingLinks(token ?? undefined)}
          deps={[token, pane]}
          empty="No links are trending yet."
        >
          {(items) => (
            <div className="space-y-2">
              {items.map((c) => (
                <LinkRow key={c.url} card={c} />
              ))}
            </div>
          )}
        </Pane>
      ) : (
        // Trends need no token — an instance publishes them to anyone — but
        // passing one lets the server drop posts from accounts the viewer has
        // blocked or muted.
        <Pane<mastodon.v1.Status>
          key={pane}
          open={() => getTrendingStatuses(token ?? undefined)}
          deps={[token, pane]}
          empty="No posts are trending yet."
        >
          {(items) => (
            <TimelineStack>
              {items.map((s) => (
                <StatusCard
                  key={s.id}
                  status={s.reblog ?? s}
                  token={token ?? ''}
                  filterContext="public"
                  boostedBy={s.reblog ? s.account : undefined}
                />
              ))}
            </TimelineStack>
          )}
        </Pane>
      )}
    </div>
  )
}
