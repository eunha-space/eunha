import { useEffect, useState } from 'react'
import { toast } from 'sonner'

import {
  listFollowRecommendations,
  setFollowRecommendationsSuppressed,
  type AdminFollowRecommendation,
} from '../../admin-server-api.ts'
import { getToken } from '../../auth.ts'
import { errorMessage } from '@/lib/utils.ts'
import { AdminError, AdminLayout } from '@/components/admin/admin-layout.tsx'
import { AdminAccountLink, ChoiceSelect } from '@/components/admin/admin-common.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { Button } from '@/components/ui/button.tsx'
import { Input } from '@/components/ui/input.tsx'
import { Label } from '@/components/ui/label.tsx'
import { AccountListSkeleton } from '@/components/skeletons.tsx'
import { t } from '@/i18n.tsx'

const STATUSES = { get active() { return t({ id: 'mastodon.admin.webhooks.enabled', defaultMessage: 'Active' }) }, get suppressed() { return t({ id: 'mastodon.admin.follow_recommendations.suppressed', defaultMessage: 'Suppressed' }) } }
const REASONS: Record<string, string> = {
  get most_followed() { return t({ id: 'eunha.admin.follow_recommendations.most_followed', defaultMessage: 'Most followed' }) },
  get most_interactions() { return t({ id: 'eunha.admin.follow_recommendations.most_interactions', defaultMessage: 'Most interactions' }) },
}

/**
 * Follow recommendations: Mastodon's `Admin::FollowRecommendationsController`.
 * The accounts new users are recommended, refreshed daily, and those kept out.
 */
export default function FollowRecommendations() {
  const token = getToken()
  const [status, setStatus] = useState<keyof typeof STATUSES>('active')
  const [language, setLanguage] = useState('')
  const [page, setPage] = useState(1)
  const [items, setItems] = useState<AdminFollowRecommendation[] | null>(null)
  const [error, setError] = useState<string | null>(null)

  const load = () => {
    if (!token) return
    listFollowRecommendations(token, {
      language: language.trim() || undefined,
      status: status === 'suppressed' ? 'suppressed' : undefined,
      page,
    })
      .then((list) => {
        setItems(list)
        setError(null)
      })
      .catch((e) => setError(String(e)))
  }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(load, [token, status, language, page])

  const toggle = async (item: AdminFollowRecommendation) => {
    try {
      await setFollowRecommendationsSuppressed(token ?? '', [item.account.id], !item.suppressed)
      toast.success(item.suppressed ? t({ id: 'eunha.admin.follow_recommendations.recommended_again', defaultMessage: 'Recommended again.' }) : t({ id: 'eunha.common.suppressed', defaultMessage: 'Suppressed.' }))
      load()
    } catch (e) {
      toast.error(errorMessage(e))
    }
  }

  return (
    <AdminLayout title={t({ id: 'mastodon.admin.follow_recommendations.title', defaultMessage: 'Follow recommendations' })} permission="manage_taxonomies">
      <p className="text-muted-foreground mb-3 text-sm">
        {t({ id: 'eunha.admin.follow_recommendations.follow_recommendations_help_new_users_quickly', defaultMessage: 'Follow recommendations help new users quickly find interesting content. When a user has not interacted with others enough to form personalized follow recommendations, these accounts are recommended instead. They are re-calculated daily from a mix of the accounts with the highest recent engagements and the most local followers for a given language.' })}
      </p>
      <div className="mb-3 flex flex-wrap items-end gap-2">
        <div className="space-y-1">
          <Label>{t({ id: 'mastodon.admin.follow_recommendations.status', defaultMessage: 'Status' })}</Label>
          <ChoiceSelect
            label={t({ id: 'mastodon.admin.follow_recommendations.status', defaultMessage: 'Status' })}
            value={status}
            items={STATUSES}
            onChange={(v) => {
              setStatus(v)
              setPage(1)
            }}
            className="w-36"
          />
        </div>
        <div className="space-y-1">
          <Label htmlFor="rec-language">{t({ id: 'about.language_label', defaultMessage: 'Language' })}</Label>
          <Input
            id="rec-language"
            className="w-28"
            value={language}
            placeholder="en"
            onChange={(e) => {
              setLanguage(e.target.value)
              setPage(1)
            }}
          />
        </div>
      </div>
      <AdminError error={error} />
      {items === null && !error && <AccountListSkeleton />}
      {items?.length === 0 && (
        <p className="text-muted-foreground text-sm">
          {status === 'suppressed' ? t({ id: 'eunha.admin.follow_recommendations.no_accounts_are_suppressed', defaultMessage: 'No accounts are suppressed.' }) : t({ id: 'eunha.admin.follow_recommendations.nothing_is_recommended_yet', defaultMessage: 'Nothing is recommended yet.' })}
        </p>
      )}
      <div className="space-y-2">
        {items?.map((item) => (
          <div key={item.account.id} className="flex flex-wrap items-center gap-2 rounded-lg border p-3">
            <div className="min-w-0 flex-1">
              <AdminAccountLink account={item.account} />
            </div>
            {item.reason.map((r) => (
              <Badge key={r} variant="outline">
                {REASONS[r] ?? r}
              </Badge>
            ))}
            {item.language && <Badge variant="secondary">{item.language}</Badge>}
            <Button size="xs" variant="outline" onClick={() => void toggle(item)}>
              {item.suppressed ? t({ id: 'eunha.admin.follow_recommendations.restore', defaultMessage: 'Restore' }) : t({ id: 'eunha.admin.follow_recommendations.suppress', defaultMessage: 'Suppress' })}
            </Button>
          </div>
        ))}
      </div>
      <div className="mt-2 flex gap-2">
        <Button size="sm" variant="outline" disabled={page === 1} onClick={() => setPage(page - 1)}>
          {t({ id: 'mastodon.pagination.newer', defaultMessage: 'Newer' })}
        </Button>
        <Button
          size="sm"
          variant="outline"
          disabled={(items?.length ?? 0) < 40}
          onClick={() => setPage(page + 1)}
        >
          {t({ id: 'mastodon.pagination.older', defaultMessage: 'Older' })}
        </Button>
      </div>
    </AdminLayout>
  )
}
