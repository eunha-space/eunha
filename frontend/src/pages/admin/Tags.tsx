import { useState } from 'react'
import { Link } from 'react-router-dom'
import { toast } from 'sonner'

import { listTags, updateTag, type AdminTag } from '../../admin-api.ts'
import { getToken } from '../../auth.ts'
import { useInfinitePaginator } from '../../hooks/use-infinite-paginator.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { InfiniteScroll } from '@/components/infinite-scroll.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Label } from '@/components/ui/label.tsx'
import { Switch } from '@/components/ui/switch.tsx'

type Flag = 'usable' | 'trendable' | 'listable'

// Mastodon's admin tag form, one switch per flag.
const FLAGS: { key: Flag; label: string; hint: string }[] = [
  { key: 'usable', label: 'Usable', hint: 'Posts can use this hashtag' },
  { key: 'trendable', label: 'Trendable', hint: 'Can appear in trends' },
  { key: 'listable', label: 'Listable', hint: 'Can appear in search and suggestions' },
]

function TagRow({
  tag,
  token,
  onSaved,
}: {
  tag: AdminTag
  token: string
  onSaved: (tag: AdminTag) => void
}) {
  const [busy, setBusy] = useState(false)
  const week = tag.history.reduce((sum, d) => sum + Number(d.uses || 0), 0)

  const toggle = async (key: Flag, on: boolean) => {
    setBusy(true)
    try {
      onSaved(await updateTag(token, tag.id, { [key]: on }))
    } catch (e) {
      toast.error(errorMessage(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="space-y-2 rounded-lg border p-3">
      <div className="flex flex-wrap items-center gap-2">
        <Link to={`/tags/${tag.name}`} className="min-w-0 flex-1 truncate font-medium">
          #{tag.name}
        </Link>
        {tag.requires_review && <Badge variant="secondary">Pending review</Badge>}
        <span className="text-muted-foreground text-xs">{week} posts this week</span>
      </div>
      <div className="flex flex-wrap gap-x-5 gap-y-2">
        {FLAGS.map((f) => (
          <Label key={f.key} className="text-sm font-normal" title={f.hint}>
            <Switch
              size="sm"
              disabled={busy}
              // `listable` is null until a moderator has decided; upstream
              // reads that as listable.
              checked={tag[f.key] ?? true}
              onCheckedChange={(on) => void toggle(f.key, on)}
            />
            {f.label}
          </Label>
        ))}
      </div>
    </div>
  )
}

/**
 * Every hashtag the server knows, with the switches Mastodon's admin tag page
 * has: whether it may be used, trend, and be listed. Changing one marks the tag
 * reviewed, as upstream's update does.
 */
export default function Tags() {
  const token = getToken()
  const feed = useInfinitePaginator<AdminTag>(() => listTags(token ?? ''), [token])

  return (
    <AdminLayout title="Hashtags" permission="manage_taxonomies">
      <AdminError error={feed.error} />
      {feed.items === null && !feed.error && (
        <p className="text-muted-foreground text-sm">Loading…</p>
      )}
      <div className="space-y-2">
        {token &&
          feed.items?.map((t) => (
            <TagRow
              key={t.id}
              tag={t}
              token={token}
              onSaved={(updated) =>
                feed.mutate((items) => items.map((i) => (i.id === updated.id ? updated : i)))
              }
            />
          ))}
      </div>
      {feed.items?.length === 0 && (
        <p className="text-muted-foreground text-sm">No hashtags yet.</p>
      )}
      <InfiniteScroll
        onLoadMore={feed.loadMore}
        loading={feed.loadingMore}
        done={feed.done}
        hasItems={!!feed.items?.length}
      />
    </AdminLayout>
  )
}
