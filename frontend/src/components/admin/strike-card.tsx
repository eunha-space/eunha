import { Link } from 'react-router-dom'

import { type Strike, type StrikeAction } from '../../admin-api.ts'
import { formatDate } from '@/components/admin/admin-common.tsx'
import { Badge } from '@/components/ui/badge.tsx'
import { t } from '@/i18n.tsx'

/** Mastodon's `user_mailer.warning.title`. */
export const STRIKE_TITLES: Record<StrikeAction, string> = {
  get none() { return t({ id: 'mastodon.admin.instances.back_to_warning', defaultMessage: 'Warning' }) },
  get disable() { return t({ id: 'mastodon.user_mailer.warning.title.disable', defaultMessage: 'Account frozen' }) },
  get mark_statuses_as_sensitive() { return t({ id: 'mastodon.user_mailer.warning.title.mark_statuses_as_sensitive', defaultMessage: 'Posts marked as sensitive' }) },
  get delete_statuses() { return t({ id: 'mastodon.user_mailer.warning.title.delete_statuses', defaultMessage: 'Posts removed' }) },
  get sensitive() { return t({ id: 'mastodon.user_mailer.warning.title.sensitive', defaultMessage: 'Account marked as sensitive' }) },
  get silence() { return t({ id: 'mastodon.user_mailer.warning.title.silence', defaultMessage: 'Account limited' }) },
  get suspend() { return t({ id: 'empty_column.account_suspended', defaultMessage: 'Account suspended' }) },
}

/** Mastodon's `disputes.strikes.title_actions`, for `"%{action} from %{date}"`. */
export const STRIKE_TITLE_ACTIONS: Record<StrikeAction, string> = {
  get none() { return t({ id: 'mastodon.admin.instances.back_to_warning', defaultMessage: 'Warning' }) },
  get disable() { return t({ id: 'mastodon.disputes.strikes.title_actions.disable', defaultMessage: 'Freezing of account' }) },
  get mark_statuses_as_sensitive() { return t({ id: 'mastodon.disputes.strikes.title_actions.mark_statuses_as_sensitive', defaultMessage: 'Marking of posts as sensitive' }) },
  get delete_statuses() { return t({ id: 'mastodon.disputes.strikes.title_actions.delete_statuses', defaultMessage: 'Post removal' }) },
  get sensitive() { return t({ id: 'mastodon.disputes.strikes.title_actions.sensitive', defaultMessage: 'Marking of account as sensitive' }) },
  get silence() { return t({ id: 'mastodon.disputes.strikes.title_actions.silence', defaultMessage: 'Limitation of account' }) },
  get suspend() { return t({ id: 'mastodon.disputes.strikes.title_actions.suspend', defaultMessage: 'Suspension of account' }) },
}

/** Where an appeal stands, as the strike list says it. */
export function AppealBadge({ strike }: { strike: Strike }) {
  if (strike.overruled_at) return <Badge variant="secondary">{t({ id: 'mastodon.disputes.strikes.your_appeal_approved', defaultMessage: 'Your appeal has been approved' })}</Badge>
  if (!strike.appeal) return null
  if (strike.appeal.state === 'rejected') {
    return <Badge variant="destructive">{t({ id: 'mastodon.disputes.strikes.your_appeal_rejected', defaultMessage: 'Your appeal has been rejected' })}</Badge>
  }
  if (strike.appeal.state === 'pending') {
    return <Badge variant="outline">{t({ id: 'mastodon.disputes.strikes.your_appeal_pending', defaultMessage: 'You have submitted an appeal' })}</Badge>
  }
  return null
}

/** One strike in a list, linking to its page. */
export function StrikeCard({ strike }: { strike: Strike }) {
  return (
    <Link
      to={`/disputes/strikes/${strike.id}`}
      className="block space-y-1 rounded-lg border p-3 no-underline hover:bg-muted/40"
    >
      <div className="flex flex-wrap items-center gap-2">
        <span className={`text-sm font-medium ${strike.overruled_at ? 'line-through' : ''}`}>
          {t(
            { id: 'mastodon.disputes.strikes.title', defaultMessage: '{action} from {date}' },
            { action: STRIKE_TITLE_ACTIONS[strike.action], date: formatDate(strike.created_at) },
          )}
        </span>
        <AppealBadge strike={strike} />
      </div>
      {strike.text && (
        <p className="text-muted-foreground line-clamp-2 text-sm whitespace-pre-wrap">
          {strike.text}
        </p>
      )}
    </Link>
  )
}
