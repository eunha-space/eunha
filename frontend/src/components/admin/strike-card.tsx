import { Link } from 'react-router-dom'

import { type Strike, type StrikeAction } from '../../admin-api.ts'
import { formatDate } from '@/components/admin/admin-common.tsx'
import { Badge } from '@/components/ui/badge.tsx'

/** Mastodon's `user_mailer.warning.title`. */
export const STRIKE_TITLES: Record<StrikeAction, string> = {
  none: 'Warning',
  disable: 'Account frozen',
  mark_statuses_as_sensitive: 'Posts marked as sensitive',
  delete_statuses: 'Posts removed',
  sensitive: 'Account marked as sensitive',
  silence: 'Account limited',
  suspend: 'Account suspended',
}

/** Mastodon's `disputes.strikes.title_actions`, for `"%{action} from %{date}"`. */
export const STRIKE_TITLE_ACTIONS: Record<StrikeAction, string> = {
  none: 'Warning',
  disable: 'Freezing of account',
  mark_statuses_as_sensitive: 'Marking of posts as sensitive',
  delete_statuses: 'Post removal',
  sensitive: 'Marking of account as sensitive',
  silence: 'Limitation of account',
  suspend: 'Suspension of account',
}

/** Where an appeal stands, as the strike list says it. */
export function AppealBadge({ strike }: { strike: Strike }) {
  if (strike.overruled_at) return <Badge variant="secondary">Your appeal has been approved</Badge>
  if (!strike.appeal) return null
  if (strike.appeal.state === 'rejected') {
    return <Badge variant="destructive">Your appeal has been rejected</Badge>
  }
  if (strike.appeal.state === 'pending') {
    return <Badge variant="outline">You have submitted an appeal</Badge>
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
          {STRIKE_TITLE_ACTIONS[strike.action]} from {formatDate(strike.created_at)}
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
