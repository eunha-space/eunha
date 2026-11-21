import { useState } from 'react'
import { toast } from 'sonner'

import type { mastodon } from '../masto.ts'
import { setFollow } from '../api.ts'
import { Button } from '@/components/ui/button.tsx'
import { Spinner } from '@/components/ui/spinner.tsx'
import { usePendingFollow } from '@/hooks/use-pending-follow.ts'
import { t } from '@/i18n.tsx'

/** Follow, Requested or Following, for an account in a list: the profile's
 *  follow button without the rest of its menu. Pressing it again unfollows,
 *  or withdraws a pending request, as on the profile. A pending request
 *  turns into Following by itself once it is accepted. A spinner shows while
 *  the follow or unfollow is being sent. */
export function FollowButton({
  account,
  relationship,
  token,
  onChange,
}: {
  account: mastodon.v1.Account
  relationship: mastodon.v1.Relationship
  token: string
  onChange: (relationship: mastodon.v1.Relationship) => void
}) {
  const [busy, setBusy] = useState(false)
  usePendingFollow(relationship, token, onChange)
  // Neither side can follow across a block; the profile says why.
  if (relationship.blocking || relationship.blockedBy) return null
  const on = relationship.following || relationship.requested
  const toggle = async () => {
    setBusy(true)
    try {
      onChange(await setFollow(account.id, token, !on))
    } catch {
      toast.error(t({ id: 'eunha.follow_button.could_not_update', defaultMessage: 'Could not update the follow for @{acct}' }, { acct: account.acct }))
    } finally {
      setBusy(false)
    }
  }
  return (
    <Button
      size="sm"
      variant={on ? 'outline' : 'default'}
      onClick={toggle}
      disabled={busy}
      aria-busy={busy}
      title={relationship.requested && !relationship.following ? t({ id: 'eunha.profile.withdraw_follow_request', defaultMessage: 'Withdraw follow request' }) : undefined}
    >
      {busy && <Spinner data-icon="inline-start" className="size-3.5" aria-hidden />}
      {relationship.following
        ? t({ id: 'account.following', defaultMessage: 'Following' })
        : relationship.requested
          ? t({ id: 'eunha.profile.requested', defaultMessage: 'Requested' })
          : t({ id: 'account.follow', defaultMessage: 'Follow' })}
    </Button>
  )
}
