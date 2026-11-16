import type { Invite } from '../eunha-api.ts'
import { currentLocale, t } from '@/i18n.tsx'

export function inviteDate(value: string): Date {
  return new Date(/[Z+]|-\d\d:\d\d$/.test(value) ? value : `${value}Z`)
}
export type InviteStatus = 'Available' | 'Used' | 'Expired' | 'Unavailable'

export function inviteStatus(invite: Invite): InviteStatus {
  // i18n-ignore: a value compared below; `inviteStatusLabel` is its text.
  if (invite.max_uses !== null && invite.uses >= invite.max_uses) return 'Used'
  if (
    invite.expired ||
    (invite.expires_at && inviteDate(invite.expires_at).getTime() <= Date.now())
  )
    // i18n-ignore
    return 'Expired'
  // i18n-ignore
  return invite.valid_for_use ? 'Available' : 'Unavailable'
}

export function inviteStatusLabel(status: InviteStatus): string {
  switch (status) {
    case 'Used':
      return t({ id: 'eunha.invites.used', defaultMessage: 'Used' })
    case 'Expired':
      return t({ id: 'filter_modal.select_filter.expired', defaultMessage: 'Expired' })
    case 'Available':
      return t({ id: 'mastodon.admin.invites.filter.available', defaultMessage: 'Available' })
    case 'Unavailable':
      return t({ id: 'mastodon.admin.instances.delivery.unavailable', defaultMessage: 'Unavailable' })
  }
}

export function inviteExpiry(invite: Invite): string {
  if (!invite.expires_at) return t({ id: 'eunha.invites.never_expires', defaultMessage: 'Never expires' })
  const date = inviteDate(invite.expires_at).toLocaleString(currentLocale())
  return inviteStatus(invite) === 'Expired'
    ? t({ id: 'eunha.invites.expired_on', defaultMessage: 'Expired {date}' }, { date })
    : t({ id: 'eunha.invites.expires_on', defaultMessage: 'Expires {date}' }, { date })
}
/** A link count is meaningful as a people allowance only for single-use links. */
export function availableInviteCount(invites: Invite[]): number | null {
  const usable = invites.filter((i) => inviteStatus(i) === 'Available')
  return usable.length && usable.every((i) => i.max_uses === 1)
    ? usable.length
    : null
}
